//! The collar's USB serial port at 115,200 baud, raw, on macOS and Linux.
//!
//! A reader thread turns bytes into lines; [`Port::write_line`] paces long
//! lines at the baud rate so the collar's UART keeps up (a boundary can be
//! 12 KB). An unplugged collar ends the reader with [`SerialIn::Gone`].

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc;

pub const BAUD: u32 = 115_200;
/// Bytes written at a time, then a pause long enough to send them.
const CHUNK: usize = 64;

#[derive(Debug)]
pub enum SerialIn {
    Line(String),
    /// The port went away (unplugged, reset into the bootloader, …).
    Gone(String),
}

pub struct Port {
    path: PathBuf,
    file: File,
}

impl Port {
    /// Open and set up the port: raw, 8N1, 115,200 baud, no flow control.
    pub fn open(path: &Path) -> io::Result<Port> {
        // O_NONBLOCK so opening never waits on carrier detect; cleared after.
        let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK).open(path)?;
        let fd = file.as_raw_fd();
        // SAFETY: fd is open for the life of `file`; termios is plain data.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return Err(io::Error::last_os_error());
            }
            libc::cfmakeraw(&mut t);
            t.c_cflag |= libc::CLOCAL | libc::CREAD;
            t.c_cflag &= !(libc::CSTOPB | libc::PARENB | libc::CRTSCTS);
            // Reads return after 100 ms without data, so the reader can notice an unplug.
            t.c_cc[libc::VMIN] = 0;
            t.c_cc[libc::VTIME] = 1;
            if libc::cfsetspeed(&mut t, libc::B115200) != 0 || libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
                return Err(io::Error::last_os_error());
            }
            libc::tcflush(fd, libc::TCIOFLUSH);
        }
        Ok(Port { path: path.to_owned(), file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Start the reader thread; lines arrive on the returned channel.
    pub fn reader(&self) -> io::Result<mpsc::Receiver<SerialIn>> {
        let mut file = self.file.try_clone()?;
        let path = self.path.clone();
        let (tx, rx) = mpsc::channel(1024);
        std::thread::Builder::new().name("serial-reader".into()).spawn(move || {
            let mut buf = [0u8; 512];
            let mut line: Vec<u8> = Vec::with_capacity(256);
            let mut idle = 0u32;
            loop {
                if hung_up(&file) {
                    let _ = tx.blocking_send(SerialIn::Gone("the port hung up".into()));
                    return;
                }
                match file.read(&mut buf) {
                    Ok(0) => {
                        // Linux reads 0 after a hangup; check the device is still there.
                        idle += 1;
                        if idle % 10 == 0 && !path.exists() {
                            let _ = tx.blocking_send(SerialIn::Gone("the port disappeared".into()));
                            return;
                        }
                    }
                    Ok(n) => {
                        idle = 0;
                        for &b in &buf[..n] {
                            if b == b'\n' || b == b'\r' {
                                if !line.is_empty() {
                                    let text = String::from_utf8_lossy(&line).into_owned();
                                    line.clear();
                                    if tx.blocking_send(SerialIn::Line(text)).is_err() {
                                        return;
                                    }
                                }
                            } else if line.len() < 16 * 1024 {
                                line.push(b);
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted || e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        let _ = tx.blocking_send(SerialIn::Gone(e.to_string()));
                        return;
                    }
                }
            }
        })?;
        Ok(rx)
    }

    /// Write one line (a newline is added), paced at the baud rate.
    pub async fn write_line(&self, line: &str) -> io::Result<()> {
        let mut file = self.file.try_clone()?;
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        tokio::task::spawn_blocking(move || {
            // 10 bits a byte on the wire.
            let pause = Duration::from_micros((CHUNK as u64 * 10 * 1_000_000) / u64::from(BAUD));
            for chunk in bytes.chunks(CHUNK) {
                file.write_all(chunk)?;
                file.flush()?;
                std::thread::sleep(pause);
            }
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }
}

/// The other end is gone (USB unplugged, a pty's master closed).
fn hung_up(file: &File) -> bool {
    let mut p = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd for the life of the call.
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    n > 0 && p.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

/// Serial ports that look like a collar: USB modems and USB serial adapters.
pub fn list() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir("/dev")
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                    name.starts_with("cu.usbmodem") || name.starts_with("cu.usbserial") || name.starts_with("ttyACM") || name.starts_with("ttyUSB")
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}
