//! Transport abstraction. Today: serial port (UART bridge or native USB CDC).

pub mod serial;

use std::time::Duration;

use crate::error::Result;

/// A bidirectional serial-like transport with control over baud rate and
/// modem-control lines.  All operations are blocking with explicit timeouts.
pub trait Transport: Send {
    /// Read up to `buf.len()` bytes, returning the number read. Returns
    /// `Err(io::ErrorKind::TimedOut)` on timeout.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize>;

    /// Write all bytes; flushes on return.
    fn write(&mut self, data: &[u8]) -> Result<()>;

    /// Set the read timeout for subsequent `read` calls.
    fn set_timeout(&mut self, t: Duration) -> Result<()>;

    /// Set baud rate.
    fn set_baud(&mut self, baud: u32) -> Result<()>;

    /// Drop any buffered input. Implementations should also flush OS-level
    /// receive buffers.
    fn flush_input(&mut self) -> Result<()>;

    /// Drop any pending output.
    fn flush_output(&mut self) -> Result<()>;

    /// Set DTR (Data Terminal Ready). `true` asserts the line (active-low at
    /// the physical level — pyserial-compatible semantics).
    fn set_dtr(&mut self, on: bool) -> Result<()>;

    /// Set RTS (Request To Send). Same active-low semantics.
    fn set_rts(&mut self, on: bool) -> Result<()>;

    /// Atomically set both DTR and RTS on Unix systems (ioctl TIOCMSET).
    /// On systems that don't support this, the default implementation
    /// calls the two setters in order.
    fn set_dtr_rts(&mut self, dtr: bool, rts: bool) -> Result<()> {
        self.set_dtr(dtr)?;
        self.set_rts(rts)?;
        Ok(())
    }

    /// USB VID, if discoverable.
    fn usb_vid(&self) -> Option<u16> {
        None
    }
    /// USB PID, if discoverable.
    fn usb_pid(&self) -> Option<u16> {
        None
    }

    /// Human-readable port identifier (e.g. `/dev/cu.usbserial-XYZ`).
    fn port_name(&self) -> &str;
}
