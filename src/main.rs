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
const POLL: Duration = Duration::from_millis(500);
// A silent monitor is nearly always one that is switching inputs, and it is worth catching the new
// input the moment it answers, so poll hard until it does
const RETRY: Duration = Duration::from_millis(100);
const RELOOKUP_AFTER: Duration = Duration::from_secs(10);

// DDC/CI: the monitor listens at 0x37, requests carry the host address 0x51, and checksums are
// seeded with the address the bytes travel to
const DDC_ADDR: u16 = 0x37;
const DDC_PEER: u8 = (DDC_ADDR as u8) << 1;
// The host answers to 0x50; requests carry it with the low bit set to mark them as a source address
const DDC_HOST: u8 = 0x50;
const DDC_REPLY_DELAY: Duration = Duration::from_millis(40);
const VCP_INPUT: u8 = 0x60;
const I2C_RDWR: libc::c_ulong = 0x0707;
const I2C_M_RD: u16 = 0x0001;

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
fn open_bus(bus: Option<String>) -> Option<File> {
    OpenOptions::new().read(true).write(true).open(format!("/dev/i2c-{}", bus?)).ok()
}

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

#[repr(C)]
struct I2cMsg {
    addr: u16,
    flags: u16,
    len: u16,
    buf: *mut u8,
}

#[repr(C)]
struct I2cRdwr {
    msgs: *mut I2cMsg,
    nmsgs: u32,
}

fn i2c_xfer(dev: &File, flags: u16, buf: &mut [u8]) -> bool {
    let mut msg = I2cMsg { addr: DDC_ADDR, flags, len: buf.len() as u16, buf: buf.as_mut_ptr() };
    let mut data = I2cRdwr { msgs: &mut msg, nmsgs: 1 };
    // SAFETY: the ioctl transfers one message, describing a buffer that outlives the call
    let rc = unsafe { libc::ioctl(dev.as_raw_fd(), I2C_RDWR, &mut data) };
    rc >= 0
}

// The monitor keeps one reply buffer per bus, so a concurrent reader is answered from the same queue
// and can walk off with our reply. ddcutil locks the device with flock for this reason; take the same
// lock so the two cooperate without needing a channel of their own.
struct BusLock<'a>(&'a File);

impl<'a> BusLock<'a> {
    fn new(dev: &'a File) -> Self {
        unsafe { libc::flock(dev.as_raw_fd(), libc::LOCK_EX) };
        BusLock(dev)
    }
}

impl Drop for BusLock<'_> {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn checksum(seed: u8, bytes: &[u8]) -> u8 {
    bytes.iter().fold(seed, |sum, b| sum ^ b)
}

// One DDC/CI exchange, ~85 ms. ddcutil would do the same in 240 ms, re-reading the EDID every call,
// and its retry backoff blocks for seconds when the monitor is mid-switch, which is exactly when the
// answer matters. The checks below matter because a concurrent reader on the bus (another ddcutil,
// say) is answered from the same queue, so a reply meant for someone else can land here.
fn read_vcp(dev: &File, code: u8) -> Option<u8> {
    let _lock = BusLock::new(dev);
    let mut req = [DDC_HOST | 1, 0x82, 0x01, code, 0];
    req[4] = checksum(DDC_PEER, &req[..4]);
    if !i2c_xfer(dev, 0, &mut req) {
        return None;
    }
    // The monitor needs time to prepare the reply; the spec asks for at least 40 ms
    sleep(DDC_REPLY_DELAY);
    // Source, length, "VCP reply", result, code, type, max hi, max lo, value hi, value lo, checksum
    let mut rep = [0u8; 11];
    if !i2c_xfer(dev, I2C_M_RD, &mut rep) {
        return None;
    }
    let sane = rep[0] == DDC_PEER
        && rep[1] == 0x88
        && rep[2] == 0x02
        && rep[3] == 0x00
        && rep[4] == code
        && checksum(DDC_HOST, &rep[..10]) == rep[10];
    sane.then(|| rep[9])
}

struct Monitor {
    dev: Option<File>,
    failing_since: Option<Instant>,
}

impl Monitor {
    fn new() -> Self {
        Monitor { dev: open_bus(monitor_bus()), failing_since: None }
    }

    // Silent monitors are usually mid-switch, so the caller polls harder until one answers
    fn silent(&self) -> bool {
        self.failing_since.is_some()
    }

    // A read fails whenever the monitor is busy switching inputs, which is the common case and clears
    // up in a couple of seconds, while the bus number only moves if the GPU driver rebinds. So keep
    // reading the bus we know and let a long outage, or a failed lookup at startup, pay for a probe.
    fn input(&mut self) -> Option<u8> {
        if let Some(input) = self.dev.as_ref().and_then(|dev| read_vcp(dev, VCP_INPUT)) {
            self.failing_since = None;
            return Some(input);
        }
        let failing_since = *self.failing_since.get_or_insert_with(Instant::now);
        if failing_since.elapsed() < RELOOKUP_AFTER {
            return None;
        }
        // Restart the countdown either way: a probe costs 7 s and the monitor may simply be off
        self.failing_since = Some(Instant::now());
        self.dev = open_bus(monitor_bus());
        let input = self.dev.as_ref().and_then(|dev| read_vcp(dev, VCP_INPUT));
        if input.is_some() {
            self.failing_since = None;
        }
        input
    }
}

// The monitor input is the shared state between hosts: move the devices wherever it points
fn watch() -> ! {
    let mut monitor = Monitor::new();
    let mut last = None;
    loop {
        if let Some(input) = monitor.input() {
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
        sleep(if monitor.silent() { RETRY } else { POLL });
    }
}

fn main() {
    let arg = std::env::args().nth(1);
    if arg.as_deref() == Some("watch") {
        watch();
    }
    if arg.as_deref() == Some("input") {
        // An explicit bus skips the slow lookup, which is handy when testing
        let mut monitor = match std::env::args().nth(2) {
            Some(bus) => Monitor { dev: open_bus(Some(bus)), failing_since: None },
            None => Monitor::new(),
        };
        println!("{:?}", monitor.input().map(|i| format!("{i:#04x}")));
        return;
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
