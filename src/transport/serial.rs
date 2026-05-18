//! Serial transport backed by serialport-rs.
//!
//! Works equally for USB-UART bridges (CP210x, CH340, FTDI) and for native
//! USB-Serial/JTAG peripherals (ESP32-S3/C3/C6/H2 built-in USB), since the
//! OS exposes both as the same /dev/cu.* or COM* device. The reset strategy
//! choice (see `crate::reset`) is what differs based on USB VID/PID.

use std::io::{Read, Write};
use std::time::Duration;

use serialport::{ClearBuffer, SerialPort};

use crate::error::{Error, Result};
use crate::transport::Transport;

pub struct SerialTransport {
    port: Box<dyn SerialPort>,
    name: String,
    vid: Option<u16>,
    pid: Option<u16>,
    /// Underlying file descriptor for ioctl-based atomic DTR/RTS on Unix.
    /// Captured at open time from the concrete `TTYPort` so we don't have to
    /// downcast `Box<dyn SerialPort>` later.
    #[cfg(unix)]
    fd: std::os::fd::RawFd,
}

impl SerialTransport {
    /// Open a serial port for ESP bootloader interaction.
    ///
    /// `path` is the OS-level device path (`/dev/cu.usbserial-XYZ`, `COM5`).
    pub fn open(path: &str, baud: u32) -> Result<Self> {
        let builder = serialport::new(path, baud)
            .timeout(Duration::from_millis(100))
            .data_bits(serialport::DataBits::Eight)
            .stop_bits(serialport::StopBits::One)
            .parity(serialport::Parity::None)
            .flow_control(serialport::FlowControl::None);

        let (vid, pid) = match probe_vid_pid(path) {
            Some((v, p)) => (Some(v), Some(p)),
            None => (None, None),
        };

        #[cfg(unix)]
        let (port, fd): (Box<dyn SerialPort>, std::os::fd::RawFd) = {
            use std::os::fd::AsRawFd;
            let tty = builder.open_native().map_err(|e| Error::OpenPort {
                port: path.into(),
                source: e,
            })?;
            let fd = tty.as_raw_fd();
            (Box::new(tty), fd)
        };
        #[cfg(not(unix))]
        let port: Box<dyn SerialPort> = builder.open().map_err(|e| Error::OpenPort {
            port: path.into(),
            source: e,
        })?;

        #[allow(unused_mut)]
        let mut t = Self {
            port,
            name: path.into(),
            vid,
            pid,
            #[cfg(unix)]
            fd,
        };

        // Per upstream esptool: on Windows, drive DTR/RTS to false before
        // opening other operations so the chip isn't held in reset.
        #[cfg(windows)]
        {
            t.set_dtr(false)?;
            t.set_rts(false)?;
        }
        Ok(t)
    }
}

impl Transport for SerialTransport {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        match self.port.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                Err(Error::Io(std::io::Error::from(std::io::ErrorKind::TimedOut)))
            }
            Err(e) => Err(Error::Io(e)),
        }
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.port.write_all(data)?;
        self.port.flush()?;
        Ok(())
    }

    fn set_timeout(&mut self, t: Duration) -> Result<()> {
        self.port.set_timeout(t)?;
        Ok(())
    }

    fn set_baud(&mut self, baud: u32) -> Result<()> {
        self.port.set_baud_rate(baud)?;
        Ok(())
    }

    fn flush_input(&mut self) -> Result<()> {
        self.port.clear(ClearBuffer::Input)?;
        Ok(())
    }

    fn flush_output(&mut self) -> Result<()> {
        self.port.clear(ClearBuffer::Output)?;
        Ok(())
    }

    fn set_dtr(&mut self, on: bool) -> Result<()> {
        self.port.write_data_terminal_ready(on)?;
        Ok(())
    }

    fn set_rts(&mut self, on: bool) -> Result<()> {
        self.port.write_request_to_send(on)?;
        Ok(())
    }

    #[cfg(unix)]
    fn set_dtr_rts(&mut self, dtr: bool, rts: bool) -> Result<()> {
        use nix::libc::{c_int, TIOCMGET, TIOCMSET, TIOCM_DTR, TIOCM_RTS};

        let fd = self.fd;
        let mut status: c_int = 0;
        // SAFETY: TIOCMGET reads the modem-line status into `status`.
        let r = unsafe { nix::libc::ioctl(fd, TIOCMGET, &mut status as *mut c_int) };
        if r < 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        if dtr {
            status |= TIOCM_DTR;
        } else {
            status &= !TIOCM_DTR;
        }
        if rts {
            status |= TIOCM_RTS;
        } else {
            status &= !TIOCM_RTS;
        }
        // SAFETY: TIOCMSET writes modem-line status.
        let r = unsafe { nix::libc::ioctl(fd, TIOCMSET, &status as *const c_int) };
        if r < 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn usb_vid(&self) -> Option<u16> {
        self.vid
    }

    fn usb_pid(&self) -> Option<u16> {
        self.pid
    }

    fn port_name(&self) -> &str {
        &self.name
    }
}

/// Find the USB VID/PID for an OS port path by walking `serialport::available_ports`.
///
/// macOS exposes the same physical device as both `/dev/tty.*` (blocking) and
/// `/dev/cu.*` (call-up); `serialport::available_ports()` may report either.
/// We accept any of: exact match, the original path, the cu/tty-swapped form,
/// and as a last resort, basename match — so that callers opening
/// `/dev/tty.usbmodemXYZ` still pick up VID/PID from `/dev/cu.usbmodemXYZ`.
fn probe_vid_pid(path: &str) -> Option<(u16, u16)> {
    let ports = serialport::available_ports().ok()?;
    let canonical = std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string());
    let cu_form = canonical.replace("/dev/tty.", "/dev/cu.");
    let tty_form = canonical.replace("/dev/cu.", "/dev/tty.");
    let basename = canonical
        .rsplit('/')
        .next()
        .unwrap_or(&canonical)
        .trim_start_matches("cu.")
        .trim_start_matches("tty.")
        .to_string();
    for p in ports {
        let matches = p.port_name == canonical
            || p.port_name == path
            || p.port_name == cu_form
            || p.port_name == tty_form
            || p.port_name
                .rsplit('/')
                .next()
                .unwrap_or("")
                .trim_start_matches("cu.")
                .trim_start_matches("tty.")
                == basename;
        if matches {
            if let serialport::SerialPortType::UsbPort(info) = p.port_type {
                return Some((info.vid, info.pid));
            }
        }
    }
    None
}
