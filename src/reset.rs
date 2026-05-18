//! Reset strategies to enter the bootloader (download) mode or to hard-reset
//! the chip after operations.
//!
//! Mirrors upstream esptool's `reset.py` semantics:
//!   * ClassicReset — DTR/RTS sequenced classic reset for UART bridges
//!   * UnixTightReset — Unix-only single-ioctl variant that avoids USB-driver
//!     reordering of the DTR/RTS transitions
//!   * UsbJtagSerialReset — native USB-Serial/JTAG on ESP32-S3/C3/C6/H2
//!   * HardReset — EN pulse to restart the chip into application code

use std::thread::sleep;
use std::time::Duration;

use crate::error::Result;
use crate::transport::Transport;

/// Default time to release IO0 after de-asserting reset (matches esptool's
/// `DEFAULT_RESET_DELAY = 0.05`).
pub const DEFAULT_RESET_DELAY: Duration = Duration::from_millis(50);

/// Espressif USB Vendor ID.
pub const ESPRESSIF_VID: u16 = 0x303A;
/// Native USB-Serial/JTAG PID — same value across S3/C3/C6/H2.
pub const USB_JTAG_SERIAL_PID: u16 = 0x1001;

/// Which entry-into-bootloader strategy to attempt.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ResetMode {
    /// Default: choose ClassicReset / UnixTightReset / UsbJtagSerialReset
    /// based on the OS and the port's USB VID/PID.
    Default,
    /// Force USBJTAGSerialReset (native USB).
    UsbReset,
    /// Don't reset; assume the chip is already in download mode.
    NoReset,
    /// Don't reset and don't sync afterwards (pass-through scenarios).
    NoResetNoSync,
}

/// Behavior after we're done — leave alone or hard-reset into app.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AfterMode {
    HardReset,
    NoReset,
    NoResetStub,
}

/// One step of a reset sequence.
#[derive(Copy, Clone, Debug)]
enum Step {
    Dtr(bool),
    Rts(bool),
    DtrRts(bool, bool),
    Wait(Duration),
}

fn run(transport: &mut dyn Transport, steps: &[Step]) -> Result<()> {
    for s in steps {
        match *s {
            Step::Dtr(b) => transport.set_dtr(b)?,
            Step::Rts(b) => {
                transport.set_rts(b)?;
                // Upstream esptool's workaround for usbser.sys on Windows:
                // generate a dummy DTR change to push the line-state.
                // The current DTR is unknown via this API, so re-issue what
                // we last set; if no prior, this is a no-op on most drivers.
                // We skip the workaround here because our set_dtr/set_rts
                // each issue a SET_CONTROL_LINE_STATE on the same line so the
                // semantics are equivalent.
            }
            Step::DtrRts(d, r) => transport.set_dtr_rts(d, r)?,
            Step::Wait(d) => sleep(d),
        }
    }
    Ok(())
}

/// Classic reset: sequential DTR/RTS. Works on every UART bridge.
pub fn classic_reset(transport: &mut dyn Transport, reset_delay: Duration) -> Result<()> {
    run(
        transport,
        &[
            Step::Dtr(false), // IO0=HIGH
            Step::Rts(true),  // EN=LOW (chip in reset)
            Step::Wait(Duration::from_millis(100)),
            Step::Dtr(true),  // IO0=LOW
            Step::Rts(false), // EN=HIGH (out of reset)
            Step::Wait(reset_delay),
            Step::Dtr(false), // IO0=HIGH (done)
        ],
    )
}

/// Unix tight reset: atomic DTR/RTS transitions via ioctl(TIOCMSET).
/// Avoids the brief intermediate states some FTDI / CP210x kernel drivers
/// produce when the two lines are set in sequence.
#[cfg(unix)]
pub fn unix_tight_reset(transport: &mut dyn Transport, reset_delay: Duration) -> Result<()> {
    run(
        transport,
        &[
            Step::DtrRts(false, false),
            Step::DtrRts(true, true),
            Step::DtrRts(false, true), // IO0=HIGH & EN=LOW, in reset
            Step::Wait(Duration::from_millis(100)),
            Step::DtrRts(true, false), // IO0=LOW & EN=HIGH, out of reset
            Step::Wait(reset_delay),
            Step::DtrRts(false, false), // IO0=HIGH (done)
            Step::Dtr(false),           // some envs need this re-asserted
        ],
    )
}

/// USB-Serial/JTAG reset: native USB on S3/C3/C6/H2. No EN/RESET strap; the
/// stub uses the USB IN endpoint to enter download mode.
pub fn usb_jtag_serial_reset(transport: &mut dyn Transport) -> Result<()> {
    run(
        transport,
        &[
            Step::Rts(false),
            Step::Dtr(false), // idle
            Step::Wait(Duration::from_millis(100)),
            Step::Dtr(true), // IO0
            Step::Rts(false),
            Step::Wait(Duration::from_millis(100)),
            Step::Rts(true), // reset; (1,1) path
            Step::Dtr(false),
            Step::Rts(true), // Windows propagates DTR on RTS set
            Step::Wait(Duration::from_millis(100)),
            Step::Dtr(false),
            Step::Rts(false), // out of reset
        ],
    )
}

/// Hard reset by pulsing EN. Used after a successful run to boot the app.
/// On USB-attached parts we wait longer for re-enumeration.
pub fn hard_reset(transport: &mut dyn Transport, uses_usb: bool) -> Result<()> {
    transport.set_rts(true)?; // EN -> LOW
    if uses_usb {
        sleep(Duration::from_millis(200));
        transport.set_rts(false)?;
        sleep(Duration::from_millis(200));
    } else {
        sleep(Duration::from_millis(100));
        transport.set_rts(false)?;
    }
    Ok(())
}

/// Deterministic "reset into app firmware" sequence used by the serial
/// monitor.  Differs from `hard_reset` in that it explicitly drives DTR
/// (and therefore GPIO0) HIGH first, then pulses EN.  Without the explicit
/// DTR step, the chip will boot into DOWNLOAD mode if the OS opened the
/// port with DTR asserted (the default on many macOS / Linux drivers,
/// including the CH343 used on common ESP32-P4 dev boards).
///
/// Sequence:
///   DTR=false (IO0=HIGH) — guarantee GPIO0 strap is for normal boot
///   RTS=false             — make sure we start from a deasserted EN
///   sleep 50ms
///   RTS=true  (EN=LOW)    — pull chip into reset
///   sleep 100ms
///   RTS=false (EN=HIGH)   — release reset; chip latches GPIO0=HIGH and
///                           boots from flash
///   DTR=false (final)     — leave the line idle
pub fn reset_to_app(transport: &mut dyn Transport) -> Result<()> {
    transport.set_dtr(false)?;
    transport.set_rts(false)?;
    sleep(Duration::from_millis(50));
    transport.set_rts(true)?;
    sleep(Duration::from_millis(100));
    transport.set_rts(false)?;
    transport.set_dtr(false)?;
    Ok(())
}

/// Pick a sequence of reset attempts based on OS, mode, and VID/PID.
/// Each entry has a delay value used by `classic_reset` / `unix_tight_reset`.
///
/// Upstream esptool tries 4 variants on Unix and 2 on Windows; we mirror that.
pub fn strategy_sequence(mode: ResetMode, vid_pid: Option<(u16, u16)>) -> Vec<ResetAttempt> {
    if matches!(mode, ResetMode::NoReset | ResetMode::NoResetNoSync) {
        return vec![ResetAttempt::NoOp];
    }

    let is_usb_jtag_serial = matches!(vid_pid, Some((ESPRESSIF_VID, USB_JTAG_SERIAL_PID)));
    if mode == ResetMode::UsbReset || is_usb_jtag_serial {
        return vec![ResetAttempt::UsbJtagSerial];
    }

    let delay = DEFAULT_RESET_DELAY;
    let extra_delay = DEFAULT_RESET_DELAY + Duration::from_millis(500);

    #[cfg(unix)]
    {
        vec![
            ResetAttempt::UnixTight(delay),
            ResetAttempt::UnixTight(extra_delay),
            ResetAttempt::Classic(delay),
            ResetAttempt::Classic(extra_delay),
        ]
    }
    #[cfg(not(unix))]
    {
        vec![
            ResetAttempt::Classic(delay),
            ResetAttempt::Classic(extra_delay),
        ]
    }
}

/// One step in a connect-retry sequence.
#[derive(Copy, Clone, Debug)]
pub enum ResetAttempt {
    Classic(Duration),
    #[cfg(unix)]
    UnixTight(Duration),
    UsbJtagSerial,
    NoOp,
}

impl ResetAttempt {
    pub fn apply(&self, transport: &mut dyn Transport) -> Result<()> {
        match *self {
            ResetAttempt::Classic(d) => classic_reset(transport, d),
            #[cfg(unix)]
            ResetAttempt::UnixTight(d) => unix_tight_reset(transport, d),
            ResetAttempt::UsbJtagSerial => usb_jtag_serial_reset(transport),
            ResetAttempt::NoOp => Ok(()),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            ResetAttempt::Classic(_) => "classic",
            #[cfg(unix)]
            ResetAttempt::UnixTight(_) => "unix_tight",
            ResetAttempt::UsbJtagSerial => "usb_jtag_serial",
            ResetAttempt::NoOp => "no_reset",
        }
    }
}
