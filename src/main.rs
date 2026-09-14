use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

const SWID: u8 = 0x0a;
const CHANGE_HOST: u16 = 0x1814;

// AORUS FI32U, found by serial so it survives i2c bus renumbering and works on any host
const MONITOR_SN: &str = "23430B003207";
const INPUT_DP: u8 = 0x0f;
const INPUT_USB_C: u8 = 0x10;
const POLL: Duration = Duration::from_secs(1);

fn receivers() -> Vec<String> {
    let mut paths = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/hidraw") else {
        return paths;
    };
    for entry in entries.flatten() {
        let dev = entry.path().join("device");
        let uevent = fs::read_to_string(dev.join("uevent")).unwrap_or_default();
        if !uevent.contains("HID_ID=0003:0000046D") {
            continue;
        }
        let desc = fs::read(dev.join("report_descriptor")).unwrap_or_default();
        let has = |pat: &[u8]| desc.windows(pat.len()).any(|w| w == pat);
        if has(&[0x06, 0x00, 0xff]) && has(&[0x85, 0x11]) {
            paths.push(format!("/dev/{}", entry.file_name().to_string_lossy()));
        }
    }
    paths
}

fn read_until(f: &mut File, buf: &mut [u8], deadline: Instant) -> Option<usize> {
    let left = deadline.saturating_duration_since(Instant::now());
    let mut pfd = libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    if unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) } <= 0 {
        return None;
    }
    f.read(buf).ok()
}

fn run(path: &str, host: Option<u8>) -> std::io::Result<()> {
    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    let [hi, lo] = CHANGE_HOST.to_be_bytes();
    let mut pending = 0u8;
    for dev in 1..=6u8 {
        f.write_all(&[0x10, dev, 0x00, SWID, hi, lo, 0])?;
        pending |= 1 << dev;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = [0u8; 64];
    while pending != 0 {
        let Some(n) = read_until(&mut f, &mut buf, deadline) else {
            break;
        };
        let r = &buf[..n];
        if n < 7 || !matches!(r[0], 0x10 | 0x11) || !(1..=6).contains(&r[1]) {
            continue;
        }
        let dev = r[1];
        match (r[2], r[3], r[4]) {
            (0x8f, 0x00, SWID) => {
                pending &= !(1 << dev);
                if r[5] == 0x04 {
                    println!("{path} device {dev}: not connected");
                }
            }
            (0x00, SWID, fi) => {
                pending &= !(1 << dev);
                if fi == 0 {
                    continue;
                }
                f.write_all(&[0x10, dev, fi, SWID, 0, 0, 0])?;
                pending |= 1 << dev;
            }
            // Checked before the host info arm: a feature index can equal SWID
            (0x8f | 0xff, _, _) => pending &= !(1 << dev),
            (fi, SWID, count) => {
                pending &= !(1 << dev);
                let current = r[5] + 1;
                match host {
                    // Setting the current host again would needlessly drop the link
                    Some(h) if h != current => {
                        f.write_all(&[0x10, dev, fi, 0x10 | SWID, h - 1, 0, 0])?;
                        println!("{path} device {dev}: host {h}");
                    }
                    _ => println!("{path} device {dev}: host {current} of {count}"),
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// Selecting the monitor by serial makes ddcutil probe every bus (~7 s), so look up its bus once
fn monitor_bus() -> Option<String> {
    let output = Command::new("ddcutil").args(["detect", "--terse"]).output().ok()?;
    let mut bus = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if let Some(path) = line.strip_prefix("I2C bus:") {
            bus = path.trim().strip_prefix("/dev/i2c-").map(str::to_owned);
        } else if line.starts_with("Monitor:") && line.ends_with(&format!(":{MONITOR_SN}")) {
            return bus;
        }
    }
    None
}

fn monitor_input(bus: &mut Option<String>) -> Option<u8> {
    let get = |bus: &str| {
        let output = Command::new("ddcutil").args(["--bus", bus, "-t", "getvcp", "60"]).output().ok()?;
        // Terse output: "VCP 60 SNC x0f"
        let stdout = String::from_utf8_lossy(&output.stdout);
        u8::from_str_radix(stdout.split_whitespace().last()?.strip_prefix('x')?, 16).ok()
    };
    if let Some(input) = bus.as_deref().and_then(get) {
        return Some(input);
    }
    // The bus may have been renumbered (monitor replugged, reboot): look it up again
    *bus = monitor_bus();
    bus.as_deref().and_then(get)
}

// The monitor input is the shared state between hosts: move the devices wherever it points
fn watch() -> ! {
    let mut bus = monitor_bus();
    let mut last = None;
    loop {
        if let Some(input) = monitor_input(&mut bus) {
            let host = match input {
                INPUT_DP => Some(1),
                INPUT_USB_C => Some(2),
                _ => None,
            };
            // Only act on changes, so that switching devices by hand is left alone; the first reading
            // counts as one since the input may have changed during the slow bus lookup
            if last != Some(input)
                && let Some(h) = host
            {
                println!("monitor: input {input:#04x}, devices to host {h}");
                for path in receivers() {
                    if let Err(e) = run(&path, Some(h)) {
                        eprintln!("{path}: {e}");
                    }
                }
            }
            last = Some(input);
        }
        sleep(POLL);
    }
}

fn main() {
    let arg = std::env::args().nth(1);
    if arg.as_deref() == Some("watch") {
        watch();
    }
    let host = match arg {
        None => None,
        Some(a) => match a.parse::<u8>() {
            Ok(h @ 1..=3) => Some(h),
            _ => {
                eprintln!("usage: logi [1|2|3|watch]");
                std::process::exit(2);
            }
        },
    };
    let paths = receivers();
    if paths.is_empty() {
        eprintln!("no Logitech receiver found");
        std::process::exit(1);
    }
    for path in paths {
        if let Err(e) = run(&path, host) {
            eprintln!("{path}: {e}");
        }
    }
}
