//! Cross-platform discovery of USB-attached ESP-family devices.
//!
//! Combines two sources:
//!   1. `serialport::available_ports()` — the OS-mapped serial-port list.
//!      Authoritative for the device path you actually open (`/dev/cu.*`,
//!      `/dev/ttyUSB*`, `COM*`).
//!   2. `nusb::list_devices()` — raw USB enumeration. Enriches each entry
//!      with the manufacturer / product / serial string descriptors that
//!      `serialport`'s portable metadata often leaves empty (especially on
//!      Linux).
//!
//! What we surface: ports whose USB VID matches Espressif (0x303A) or a
//! known USB-UART bridge chip used on common ESP dev boards (Silicon Labs
//! CP210x, WCH CH34x, FTDI). Random other USB-serial devices (GPS pucks,
//! 3D printers, Bluetooth modems) are filtered out.
//!
//! Cross-platform notes:
//!   * macOS exposes each device twice as `/dev/cu.*` and `/dev/tty.*`;
//!     we de-duplicate by (vid, pid, serial_number) and prefer `cu.*`.
//!   * Linux requires read/write on the `/dev/ttyUSB*` node — usually
//!     means the user is in the `dialout` group.
//!   * Windows enumerates COM ports through usbser.sys; the only quirk
//!     is that the port number can change between unplugs.

use std::collections::BTreeMap;

use serde::Serialize;
use serialport::{available_ports, SerialPortType};

/// Espressif's USB Vendor ID — used by every native USB peripheral on the
/// S2/S3/C3/C5/C6/H2/P4 family.
pub const ESPRESSIF_VID: u16 = 0x303A;
/// PID used by Espressif's USB-Serial/JTAG peripheral across S3/C3/C6/H2/P4.
pub const USB_JTAG_SERIAL_PID: u16 = 0x1001;

const CP210X_VID: u16 = 0x10C4;
const WCH_VID: u16 = 0x1A86;
const FTDI_VID: u16 = 0x0403;

/// What kind of USB device the port belongs to, classified by VID/PID.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BridgeKind {
    /// Chip's own native USB-Serial/JTAG (no UART bridge involved).
    NativeUsbSerialJtag,
    /// Chip's own native USB-OTG. PID typically equals `IMAGE_CHIP_ID`.
    NativeUsbOtg,
    /// Silicon Labs CP2102 / CP2102N / CP2104 / CP2105 / CP2108.
    Cp210x,
    /// WCH CH340 / CH341 / CH343 / CH9102 — common on cheap boards.
    Ch34x,
    /// FTDI FT232 / FT2232 / FT232H / FT-X series.
    Ftdi,
    /// Vendor matched no known bridge family. We don't surface these.
    Unknown,
}

impl BridgeKind {
    pub fn human(&self) -> &'static str {
        match self {
            BridgeKind::NativeUsbSerialJtag => "Espressif USB-Serial/JTAG (native)",
            BridgeKind::NativeUsbOtg => "Espressif USB-OTG (native)",
            BridgeKind::Cp210x => "Silicon Labs CP210x",
            BridgeKind::Ch34x => "WCH CH340/CH343",
            BridgeKind::Ftdi => "FTDI",
            BridgeKind::Unknown => "unknown",
        }
    }
}

pub fn classify_bridge(vid: u16, pid: u16) -> BridgeKind {
    match (vid, pid) {
        (ESPRESSIF_VID, USB_JTAG_SERIAL_PID) => BridgeKind::NativeUsbSerialJtag,
        (ESPRESSIF_VID, _) => BridgeKind::NativeUsbOtg,
        (CP210X_VID, _) => BridgeKind::Cp210x,
        (WCH_VID, _) => BridgeKind::Ch34x,
        (FTDI_VID, _) => BridgeKind::Ftdi,
        _ => BridgeKind::Unknown,
    }
}

/// One discovered ESP-likely device, with its OS device path and USB
/// descriptor info needed to talk to it.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredPort {
    pub path: String,
    /// Hex-formatted, eg `"0x303a"`.
    pub vid: String,
    pub pid: String,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial_number: Option<String>,
    pub bridge: BridgeKind,
    /// Human-readable bridge name (so JSON consumers don't need to
    /// re-map the snake_case enum themselves).
    pub bridge_human: String,
}

/// List all USB serial devices that look like an ESP32-family board.
///
/// Returns a deduplicated, alphabetically-stable list. The order is
/// suitable for printing or for letting an agent pick "the first one".
pub fn list_esp_candidates() -> Vec<DiscoveredPort> {
    let mut map: BTreeMap<String, DiscoveredPort> = BTreeMap::new();
    let ports = available_ports().unwrap_or_default();
    for p in ports {
        let SerialPortType::UsbPort(info) = p.port_type else {
            continue;
        };
        let bridge = classify_bridge(info.vid, info.pid);
        if matches!(bridge, BridgeKind::Unknown) {
            continue;
        }
        let key = identity_key(info.vid, info.pid, info.serial_number.as_deref());
        let entry = DiscoveredPort {
            path: p.port_name.clone(),
            vid: format!("{:#06x}", info.vid),
            pid: format!("{:#06x}", info.pid),
            manufacturer: info.manufacturer.clone(),
            product: info.product.clone(),
            serial_number: info.serial_number.clone(),
            bridge,
            bridge_human: bridge.human().to_string(),
        };
        match map.get(&key) {
            // On macOS the same physical device shows up twice as
            // /dev/cu.* and /dev/tty.*. Keep the cu.* variant — that's
            // the one you actually open for outgoing serial.
            Some(existing) if prefers_existing(&existing.path, &p.port_name) => {}
            _ => {
                map.insert(key, entry);
            }
        }
    }
    enrich_with_nusb(&mut map);
    map.into_values().collect()
}

/// Auto-select a port when the user omits `--port`. Returns `Ok(path)` only
/// when exactly one ESP-likely candidate is present. Otherwise an error
/// string suitable for the CLI's stderr.
pub fn auto_select_port() -> Result<DiscoveredPort, String> {
    let cands = list_esp_candidates();
    match cands.len() {
        0 => Err("no ESP-like USB serial devices found. \
                  Connect a board or pass --port explicitly."
            .into()),
        1 => Ok(cands.into_iter().next().unwrap()),
        n => {
            let mut msg = format!(
                "{} ESP-like USB serial devices found; specify --port to pick one:\n",
                n
            );
            for c in &cands {
                msg.push_str(&format!(
                    "  {}  vid={} pid={}  {}\n",
                    c.path, c.vid, c.pid, c.bridge_human
                ));
            }
            Err(msg)
        }
    }
}

fn identity_key(vid: u16, pid: u16, serial: Option<&str>) -> String {
    format!("{:04x}:{:04x}:{}", vid, pid, serial.unwrap_or(""))
}

fn prefers_existing(existing_path: &str, new_path: &str) -> bool {
    existing_path.starts_with("/dev/cu.") && new_path.starts_with("/dev/tty.")
}

/// Walk the raw USB device list and fill in any manufacturer / product
/// string descriptors that the serialport-rs enumeration left empty.
/// Best-effort: if nusb itself fails (no USB permissions on Linux, etc.)
/// we just skip enrichment rather than fail the whole discover call.
fn enrich_with_nusb(map: &mut BTreeMap<String, DiscoveredPort>) {
    // nusb 0.2 returns a `MaybeFuture` so the same call works in sync and
    // async callers; we're sync, so .wait() block-resolves it.
    use nusb::MaybeFuture;
    let Ok(devices) = nusb::list_devices().wait() else {
        return;
    };
    for dev in devices {
        let key = identity_key(dev.vendor_id(), dev.product_id(), dev.serial_number());
        let Some(port) = map.get_mut(&key) else {
            continue;
        };
        if port.manufacturer.is_none() {
            if let Some(s) = dev.manufacturer_string() {
                port.manufacturer = Some(s.to_string());
            }
        }
        if port.product.is_none() {
            if let Some(s) = dev.product_string() {
                port.product = Some(s.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_espressif_native_usb_serial_jtag() {
        assert_eq!(
            classify_bridge(0x303A, 0x1001),
            BridgeKind::NativeUsbSerialJtag
        );
    }

    #[test]
    fn classifies_espressif_otg_by_vid() {
        // PID = IMAGE_CHIP_ID for ESP32-P4
        assert_eq!(classify_bridge(0x303A, 18), BridgeKind::NativeUsbOtg);
    }

    #[test]
    fn classifies_known_bridges() {
        assert_eq!(classify_bridge(0x10C4, 0xEA60), BridgeKind::Cp210x);
        assert_eq!(classify_bridge(0x1A86, 0x7523), BridgeKind::Ch34x);
        assert_eq!(classify_bridge(0x1A86, 0x55D3), BridgeKind::Ch34x);
        assert_eq!(classify_bridge(0x0403, 0x6001), BridgeKind::Ftdi);
    }

    #[test]
    fn classifies_unknown_vendor() {
        assert_eq!(classify_bridge(0x1234, 0x5678), BridgeKind::Unknown);
    }

    #[test]
    fn prefers_cu_over_tty_on_macos() {
        assert!(prefers_existing("/dev/cu.usbmodem1", "/dev/tty.usbmodem1"));
        assert!(!prefers_existing("/dev/tty.usbmodem1", "/dev/cu.usbmodem1"));
    }
}
