use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

const SWID: u8 = 0x0a;
const CHANGE_HOST: u16 = 0x1814;
const KIND_KEYBOARD: u8 = 1;
const KIND_MOUSE: u8 = 2;

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
                match host {
                    Some(h) => {
                        f.write_all(&[0x10, dev, fi, 0x10 | SWID, h - 1, 0, 0])?;
                        println!("{path} device {dev}: host {h}");
                    }
                    None => {
                        f.write_all(&[0x10, dev, fi, SWID, 0, 0, 0])?;
                        pending |= 1 << dev;
                    }
                }
            }
            (0xff, _, _) => pending &= !(1 << dev),
            (_, SWID, _) => {
                pending &= !(1 << dev);
                println!("{path} device {dev}: host {} of {}", r[5] + 1, r[4]);
            }
            _ => {}
        }
    }
    Ok(())
}

// Read one report, serving notifications set aside by `request` first
fn next_report(f: &mut File, queue: &mut VecDeque<Vec<u8>>) -> std::io::Result<Vec<u8>> {
    if let Some(r) = queue.pop_front() {
        return Ok(r);
    }
    let mut buf = [0u8; 64];
    let n = f.read(&mut buf)?;
    Ok(buf[..n].to_vec())
}

// Send an HID++ 2.0 request and wait for its response; other reports are queued
fn request(f: &mut File, queue: &mut VecDeque<Vec<u8>>, dev: u8, fi: u8, func: u8, params: &[u8]) -> Option<Vec<u8>> {
    let mut msg = [0x10, dev, fi, func << 4 | SWID, 0, 0, 0];
    msg[4..4 + params.len()].copy_from_slice(params);
    f.write_all(&msg).ok()?;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut buf = [0u8; 64];
    while let Some(n) = read_until(f, &mut buf, deadline) {
        let r = &buf[..n];
        if n >= 7 && matches!(r[0], 0x10 | 0x11) && r[1] == dev {
            if r[2] == fi && r[3] == msg[3] {
                return Some(r[4..].to_vec());
            }
            if matches!(r[2], 0x8f | 0xff) && r[3] == fi && r[4] == msg[3] {
                return None;
            }
        }
        queue.push_back(r.to_vec());
    }
    None
}

fn change_host_index(f: &mut File, queue: &mut VecDeque<Vec<u8>>, dev: u8) -> Option<u8> {
    let fi = request(f, queue, dev, 0x00, 0, &CHANGE_HOST.to_be_bytes())?[0];
    (fi != 0).then_some(fi)
}

fn move_mouse(f: &mut File, queue: &mut VecDeque<Vec<u8>>, dev: u8, host: u8) {
    let fi = change_host_index(f, queue, dev);
    let current = fi.and_then(|fi| request(f, queue, dev, fi, 0, &[])).map(|info| info[1] + 1);
    match fi {
        Some(_) if current == Some(host) => {}
        Some(fi) => {
            // The mouse drops the link immediately, so there is no response to wait for
            let _ = f.write_all(&[0x10, dev, fi, 0x10 | SWID, host - 1, 0, 0]);
            println!("mouse: host {host}");
        }
        None => println!("mouse: not connected"),
    }
}

fn watch(path: &str) -> std::io::Result<()> {
    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    let mut queue = VecDeque::new();
    // Ask the receiver to announce every paired device, which gives the initial state
    f.write_all(&[0x10, 0xff, 0x80, 0x02, 0x02, 0, 0])?;
    let mut mouse = None;
    let mut last = None;
    loop {
        let r = next_report(&mut f, &mut queue)?;
        // HID++ 1.0 device connection notification: kind in the low nibble, bit 6 set when the link is down
        if r.len() < 7 || r[0] != 0x10 || r[2] != 0x41 {
            continue;
        }
        let (dev, kind, linked) = (r[1], r[4] & 0x0f, r[4] & 0x40 == 0);
        match kind {
            KIND_MOUSE => {
                mouse = Some(dev);
                // Catch up when the mouse is announced after the keyboard is already handled here
                if let Some(host @ (1 | 3)) = last
                    && linked
                {
                    move_mouse(&mut f, &mut queue, dev, host);
                }
            }
            KIND_KEYBOARD => {
                let host = if linked {
                    let Some(fi) = change_host_index(&mut f, &mut queue, dev) else { continue };
                    let Some(info) = request(&mut f, &mut queue, dev, fi, 0, &[]) else { continue };
                    info[1] + 1
                } else {
                    // The receiver only knows whether the keyboard is here; assume it left for host 2
                    2
                };
                if last != Some(host) {
                    println!("keyboard: host {host}");
                    if let Some(dev) = mouse {
                        move_mouse(&mut f, &mut queue, dev, host);
                    }
                    last = Some(host);
                }
            }
            _ => {}
        }
    }
}

fn main() {
    let arg = std::env::args().nth(1);
    let host = match arg.as_deref() {
        None | Some("watch") => None,
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
    if arg.as_deref() == Some("watch") {
        std::thread::scope(|s| {
            for path in &paths {
                s.spawn(move || {
                    if let Err(e) = watch(path) {
                        eprintln!("{path}: {e}");
                    }
                });
            }
        });
        return;
    }
    for path in paths {
        if let Err(e) = run(&path, host) {
            eprintln!("{path}: {e}");
        }
    }
}
