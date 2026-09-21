use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::thread::sleep;
use std::os::fd::AsRawFd;
use std::process::Command;
use std::time::{Duration, Instant};

const SWID: u8 = 0x0a;
const CHANGE_HOST: u16 = 0x1814;

// AORUS FI32U, found by serial so it survives i2c bus renumbering and works on any host
const MONITOR_SN: &str = "23430B003207";
const INPUT_DP: u8 = 0x0f;
const INPUT_USB_C: u8 = 0x10;

// DDC/CI: the monitor listens at 0x37, requests carry the host address 0x51, and checksums are
// seeded with the address the bytes travel to
const DDC_ADDR: u16 = 0x37;
const DDC_PEER: u8 = (DDC_ADDR as u8) << 1;
// The host answers to 0x50; requests carry it with the low bit set to mark them as a source address
const DDC_HOST: u8 = 0x50;
const VCP_INPUT: u8 = 0x60;
const DDC_REPLY_DELAY: Duration = Duration::from_millis(40);
const DDC_ATTEMPTS: u8 = 4;
const DDC_RETRY: Duration = Duration::from_millis(60);
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

fn input_for_host(host: u8) -> Option<u8> {
    match host {
        1 => Some(INPUT_DP),
        2 => Some(INPUT_USB_C),
        _ => None,
    }
}

fn host_for_input(input: u8) -> Option<u8> {
    match input {
        INPUT_DP => Some(1),
        INPUT_USB_C => Some(2),
        _ => None,
    }
}

// The bus number holds for as long as the GPU driver stays bound, so remember it for the session.
// The runtime directory is wiped at boot, which is precisely when a renumbering could have happened.
fn cached_bus() -> Option<String> {
    let path = format!("{}/logi-bus", std::env::var("XDG_RUNTIME_DIR").ok()?);
    let bus = fs::read_to_string(path).ok()?.trim().to_owned();
    let dev = open_bus(Some(bus.clone()))?;
    read_vcp(&dev, VCP_INPUT).is_some().then_some(bus)
}

// Probing every bus costs ~7 s, so this is the slow path behind the cache
fn detect_bus() -> Option<String> {
    let bus = monitor_bus()?;
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let _ = fs::write(format!("{dir}/logi-bus"), &bus);
    }
    Some(bus)
}

fn find_bus() -> Option<String> {
    cached_bus().or_else(detect_bus)
}

fn open_bus(bus: Option<String>) -> Option<File> {
    OpenOptions::new().read(true).write(true).open(format!("/dev/i2c-{}", bus?)).ok()
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

// One DDC/CI exchange. The spec asks for a 40 ms pause before reading the reply, and adapters differ
// wildly in how much of that they spend on their own: this NVIDIA one sits a flat ~21 ms in the driver
// per transfer (1 byte and 128 bytes cost the same, the bus itself running at ~27 us/byte), while a
// normal adapter is done in ~1 ms and leaves the monitor no time to prepare. So wait out the remainder,
// which is free on a slow adapter and correct on a fast one.
// The reply is checked closely because a concurrent reader is answered from the same queue, so a reply
// meant for someone else, or one read before the monitor was ready, can land here.
fn read_vcp_once(dev: &File, code: u8) -> Option<u8> {
    let _lock = BusLock::new(dev);
    let mut req = [DDC_HOST | 1, 0x82, 0x01, code, 0];
    req[4] = checksum(DDC_PEER, &req[..4]);
    let sent = Instant::now();
    if !i2c_xfer(dev, 0, &mut req) {
        return None;
    }
    if let Some(left) = DDC_REPLY_DELAY.checked_sub(sent.elapsed()) {
        sleep(left);
    }
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

// Around one read in ten comes back empty or garbled even on an idle bus, so never decide anything on
// a single one
fn read_vcp(dev: &File, code: u8) -> Option<u8> {
    (0..DDC_ATTEMPTS).find_map(|attempt| {
        if attempt > 0 {
            sleep(DDC_RETRY);
        }
        read_vcp_once(dev, code)
    })
}

fn write_vcp(dev: &File, code: u8, value: u16) -> bool {
    let _lock = BusLock::new(dev);
    let [hi, lo] = value.to_be_bytes();
    let mut req = [DDC_HOST | 1, 0x84, 0x03, code, hi, lo, 0];
    req[6] = checksum(DDC_PEER, &req[..6]);
    i2c_xfer(dev, 0, &mut req)
}

// Pressing a key on the host that holds the devices is the one moment when both halves of a switch can
// be done at once, with nobody having to notice anything: aim the monitor at the other host, then send
// the devices after it. No polling, and no guessing from a monitor that answers only when it feels like it.
fn switch(host: Option<u8>) {
    let Some(dev) = open_bus(find_bus()) else {
        eprintln!("monitor not found");
        std::process::exit(1);
    };
    let host = match host {
        Some(h) => h,
        // Without an argument, move to whichever host is not on screen now
        None => {
            let Some(input) = read_vcp(&dev, VCP_INPUT) else {
                eprintln!("no answer from the monitor");
                std::process::exit(1);
            };
            match host_for_input(input) {
                Some(1) => 2,
                Some(_) => 1,
                None => {
                    eprintln!("monitor is on input {input:#04x}, which maps to no host");
                    std::process::exit(1);
                }
            }
        }
    };
    let Some(input) = input_for_host(host) else {
        eprintln!("no monitor input known for host {host}");
        std::process::exit(2);
    };
    if !write_vcp(&dev, VCP_INPUT, input.into()) {
        eprintln!("failed to point the monitor at host {host}");
    }
    for path in receivers() {
        if let Err(e) = run(&path, Some(host)) {
            eprintln!("{path}: {e}");
        }
    }
}

fn main() {
    let arg = std::env::args().nth(1);
    if arg.as_deref() == Some("switch") {
        switch(std::env::args().nth(2).and_then(|h| h.parse().ok()));
        return;
    }
    if arg.as_deref() == Some("input") {
        // An explicit bus skips the lookup, which is handy when testing
        let bus = std::env::args().nth(2).or_else(find_bus);
        let input = open_bus(bus).and_then(|dev| read_vcp(&dev, VCP_INPUT));
        match input {
            Some(input) => println!("{input:#04x}"),
            None => {
                eprintln!("no answer from the monitor");
                std::process::exit(1);
            }
        }
        return;
    }
    let host = match arg {
        None => None,
        Some(a) => match a.parse::<u8>() {
            Ok(h @ 1..=3) => Some(h),
            _ => {
                eprintln!("usage: logi [1|2|3|watch|switch [host]|input [bus]]");
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
