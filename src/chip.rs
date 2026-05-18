//! Chip registry. Each supported ESP32-family target has an entry containing
//! the chip-specific magic numbers, register layouts, and capabilities that
//! the protocol needs in order to talk to that silicon.
//!
//! Numbers are sourced from upstream esptool's `targets/*.py` per chip.
//! Adding a new chip is a matter of appending another entry to `REGISTRY`.

use crate::error::{Error, Result};

/// SPI controller register layout. Different ESP32 generations have
/// different relative offsets within the SPI peripheral.
#[derive(Copy, Clone, Debug)]
pub struct SpiLayout {
    pub reg_base: u32,
    pub usr_offs: u32,
    pub usr1_offs: u32,
    pub usr2_offs: u32,
    /// Newer chips (ESP32+) have explicit MOSI/MISO data-length registers;
    /// older chips encode it in USR1. `None` => use USR1 packing.
    pub mosi_dlen_offs: Option<u32>,
    pub miso_dlen_offs: Option<u32>,
    pub w0_offs: u32,
    /// `true` means addresses are packed into the MSBs of the addr register
    /// (ESP32 original); `false` means the LSBs (ESP32-S2 and later).
    pub addr_reg_msb: bool,
}

/// Watchdog peripherals that need to be tickled-off before flashing on some
/// chips.  When `Some`, before we start flashing we disable the watchdog via
/// the recipe documented in upstream esptool's `disable_watchdogs()`.
#[derive(Copy, Clone, Debug)]
pub struct WatchdogConfig {
    pub rtc_cntl_wdtwprotect_reg: u32,
    pub rtc_cntl_wdt_wkey: u32,
    pub rtc_cntl_wdtconfig0_reg: u32,
    pub rtc_cntl_wdtconfig1_reg: u32,
    /// Super-watchdog (S3/C3+); `None` if not present.
    pub swd_wprotect_reg: Option<u32>,
    pub swd_wkey: Option<u32>,
    pub swd_conf_reg: Option<u32>,
    pub swd_auto_feed_en: Option<u32>,
}

#[derive(Copy, Clone, Debug)]
pub struct Chip {
    pub name: &'static str,
    pub image_chip_id: u8,

    /// Some chips identify themselves via a 32-bit value at a fixed register
    /// (CHIP_DETECT_MAGIC_REG_ADDR). Newer chips (S3 and later) instead
    /// return their `chip_id` via GET_SECURITY_INFO.
    pub magic_value: Option<u32>,
    pub uses_magic: bool,

    pub uart_date_reg_addr: u32,

    pub efuse_base: u32,
    pub mac_efuse_reg: u32,

    pub spi: SpiLayout,

    pub watchdog: Option<WatchdogConfig>,

    /// `true` if the OS-level USB Serial/JTAG peripheral exists on this chip.
    pub has_usb_jtag_serial: bool,
    /// USB-OTG peripheral exists (ESP32-S2/S3); used to distinguish reset
    /// modes and post-reset re-enumeration timing.
    pub has_usb_otg: bool,

    /// Embedded stub filename in `stubs/<name>.json` (compile-time include).
    pub stub_blob_name: &'static str,
}

/// All supported chips.  Order doesn't matter; detection iterates the slice.
pub const REGISTRY: &[Chip] = &[
    // ESP32 — original Xtensa LX6, SPI on 0x3FF42000, magic 0x00F01D83.
    Chip {
        name: "ESP32",
        image_chip_id: 0,
        magic_value: Some(0x00F01D83),
        uses_magic: true,
        uart_date_reg_addr: 0x60000078,
        efuse_base: 0x3FF5A000,
        mac_efuse_reg: 0x3FF5A004, // BLOCK0 word 1 — handled specially in read_mac
        spi: SpiLayout {
            reg_base: 0x3FF42000,
            usr_offs: 0x1C,
            usr1_offs: 0x20,
            usr2_offs: 0x24,
            mosi_dlen_offs: Some(0x28),
            miso_dlen_offs: Some(0x2C),
            w0_offs: 0x80,
            addr_reg_msb: true,
        },
        watchdog: Some(WatchdogConfig {
            rtc_cntl_wdtwprotect_reg: 0x3FF480A4,
            rtc_cntl_wdt_wkey: 0x50D83AA1,
            rtc_cntl_wdtconfig0_reg: 0x3FF4808C,
            rtc_cntl_wdtconfig1_reg: 0x3FF48090,
            swd_wprotect_reg: None,
            swd_wkey: None,
            swd_conf_reg: None,
            swd_auto_feed_en: None,
        }),
        has_usb_jtag_serial: false,
        has_usb_otg: false,
        stub_blob_name: "esp32",
    },
    // ESP32-S2 — magic 0x000007C6, EFUSE in MMIO range 0x3F41A000.
    Chip {
        name: "ESP32-S2",
        image_chip_id: 2,
        magic_value: Some(0x000007C6),
        uses_magic: true,
        uart_date_reg_addr: 0x60000078,
        efuse_base: 0x3F41A000,
        mac_efuse_reg: 0x3F41A044,
        spi: SpiLayout {
            reg_base: 0x3F402000,
            usr_offs: 0x18,
            usr1_offs: 0x1C,
            usr2_offs: 0x20,
            mosi_dlen_offs: Some(0x24),
            miso_dlen_offs: Some(0x28),
            w0_offs: 0x58,
            addr_reg_msb: false,
        },
        watchdog: Some(WatchdogConfig {
            rtc_cntl_wdtwprotect_reg: 0x3F4080B0,
            rtc_cntl_wdt_wkey: 0x50D83AA1,
            rtc_cntl_wdtconfig0_reg: 0x3F408094,
            rtc_cntl_wdtconfig1_reg: 0x3F408098,
            swd_wprotect_reg: Some(0x3F4080B8),
            swd_wkey: Some(0x8F1D312A),
            swd_conf_reg: Some(0x3F4080B4),
            swd_auto_feed_en: Some(1 << 31),
        }),
        has_usb_jtag_serial: false,
        has_usb_otg: true,
        stub_blob_name: "esp32s2",
    },
    // ESP32-S3 — uses chip_id (GET_SECURITY_INFO) for detection.
    Chip {
        name: "ESP32-S3",
        image_chip_id: 9,
        magic_value: None,
        uses_magic: false,
        uart_date_reg_addr: 0x60000080,
        efuse_base: 0x60007000,
        mac_efuse_reg: 0x60007044,
        spi: SpiLayout {
            reg_base: 0x60002000,
            usr_offs: 0x18,
            usr1_offs: 0x1C,
            usr2_offs: 0x20,
            mosi_dlen_offs: Some(0x24),
            miso_dlen_offs: Some(0x28),
            w0_offs: 0x58,
            addr_reg_msb: false,
        },
        watchdog: Some(WatchdogConfig {
            rtc_cntl_wdtwprotect_reg: 0x600080B0,
            rtc_cntl_wdt_wkey: 0x50D83AA1,
            rtc_cntl_wdtconfig0_reg: 0x60008098,
            rtc_cntl_wdtconfig1_reg: 0x6000809C,
            swd_wprotect_reg: Some(0x600080B8),
            swd_wkey: Some(0x8F1D312A),
            swd_conf_reg: Some(0x600080B4),
            swd_auto_feed_en: Some(1 << 31),
        }),
        has_usb_jtag_serial: true,
        has_usb_otg: true,
        stub_blob_name: "esp32s3",
    },
    // ESP32-C2 — magic via chip_id (inherits from C3).
    Chip {
        name: "ESP32-C2",
        image_chip_id: 12,
        magic_value: None,
        uses_magic: false,
        uart_date_reg_addr: 0x6000007C,
        efuse_base: 0x60008800,
        mac_efuse_reg: 0x60008840,
        spi: SpiLayout {
            reg_base: 0x60002000,
            usr_offs: 0x18,
            usr1_offs: 0x1C,
            usr2_offs: 0x20,
            mosi_dlen_offs: Some(0x24),
            miso_dlen_offs: Some(0x28),
            w0_offs: 0x58,
            addr_reg_msb: false,
        },
        watchdog: Some(WatchdogConfig {
            rtc_cntl_wdtwprotect_reg: 0x600080A8,
            rtc_cntl_wdt_wkey: 0x50D83AA1,
            rtc_cntl_wdtconfig0_reg: 0x60008090,
            rtc_cntl_wdtconfig1_reg: 0x60008094,
            swd_wprotect_reg: None,
            swd_wkey: None,
            swd_conf_reg: None,
            swd_auto_feed_en: None,
        }),
        has_usb_jtag_serial: false,
        has_usb_otg: false,
        stub_blob_name: "esp32c2",
    },
    // ESP32-C3 — chip_id based detection.
    Chip {
        name: "ESP32-C3",
        image_chip_id: 5,
        magic_value: None,
        uses_magic: false,
        uart_date_reg_addr: 0x6000007C,
        efuse_base: 0x60008800,
        mac_efuse_reg: 0x60008844,
        spi: SpiLayout {
            reg_base: 0x60002000,
            usr_offs: 0x18,
            usr1_offs: 0x1C,
            usr2_offs: 0x20,
            mosi_dlen_offs: Some(0x24),
            miso_dlen_offs: Some(0x28),
            w0_offs: 0x58,
            addr_reg_msb: false,
        },
        watchdog: Some(WatchdogConfig {
            rtc_cntl_wdtwprotect_reg: 0x600080A8,
            rtc_cntl_wdt_wkey: 0x50D83AA1,
            rtc_cntl_wdtconfig0_reg: 0x60008090,
            rtc_cntl_wdtconfig1_reg: 0x60008094,
            swd_wprotect_reg: Some(0x600080B0),
            swd_wkey: Some(0x8F1D312A),
            swd_conf_reg: Some(0x600080AC),
            swd_auto_feed_en: Some(1 << 31),
        }),
        has_usb_jtag_serial: true,
        has_usb_otg: false,
        stub_blob_name: "esp32c3",
    },
    // ESP32-C6 — chip_id based detection. SPI base differs from C3.
    Chip {
        name: "ESP32-C6",
        image_chip_id: 13,
        magic_value: None,
        uses_magic: false,
        uart_date_reg_addr: 0x6000007C,
        efuse_base: 0x600B0800,
        mac_efuse_reg: 0x600B0844,
        spi: SpiLayout {
            reg_base: 0x60003000,
            usr_offs: 0x18,
            usr1_offs: 0x1C,
            usr2_offs: 0x20,
            mosi_dlen_offs: Some(0x24),
            miso_dlen_offs: Some(0x28),
            w0_offs: 0x58,
            addr_reg_msb: false,
        },
        // C6/H2 have LP_WDT at 0x600B1C00 (different base from ESP32 family).
        watchdog: Some(WatchdogConfig {
            rtc_cntl_wdtwprotect_reg: 0x600B1C1C,
            rtc_cntl_wdt_wkey: 0x50D83AA1,
            rtc_cntl_wdtconfig0_reg: 0x600B1C00,
            rtc_cntl_wdtconfig1_reg: 0x600B1C04,
            swd_wprotect_reg: Some(0x600B1C24),
            swd_wkey: Some(0x50D83AA1),
            swd_conf_reg: Some(0x600B1C20),
            swd_auto_feed_en: Some(1 << 18),
        }),
        has_usb_jtag_serial: true,
        has_usb_otg: false,
        stub_blob_name: "esp32c6",
    },
    // ESP32-H2 — inherits C6 layout but slightly different LP_WDT regs.
    Chip {
        name: "ESP32-H2",
        image_chip_id: 16,
        magic_value: None,
        uses_magic: false,
        uart_date_reg_addr: 0x6000007C,
        efuse_base: 0x600B0800,
        mac_efuse_reg: 0x600B0844,
        spi: SpiLayout {
            reg_base: 0x60003000,
            usr_offs: 0x18,
            usr1_offs: 0x1C,
            usr2_offs: 0x20,
            mosi_dlen_offs: Some(0x24),
            miso_dlen_offs: Some(0x28),
            w0_offs: 0x58,
            addr_reg_msb: false,
        },
        watchdog: Some(WatchdogConfig {
            rtc_cntl_wdtwprotect_reg: 0x600B1C1C,
            rtc_cntl_wdt_wkey: 0x50D83AA1,
            rtc_cntl_wdtconfig0_reg: 0x600B1C00,
            rtc_cntl_wdtconfig1_reg: 0x600B1C04,
            swd_wprotect_reg: Some(0x600B1C24),
            swd_wkey: Some(0x50D83AA1),
            swd_conf_reg: Some(0x600B1C20),
            swd_auto_feed_en: Some(1 << 18),
        }),
        has_usb_jtag_serial: true,
        has_usb_otg: false,
        stub_blob_name: "esp32h2",
    },
    // ESP32-C5 — chip_id 23, new EFUSE base.
    Chip {
        name: "ESP32-C5",
        image_chip_id: 23,
        magic_value: None,
        uses_magic: false,
        uart_date_reg_addr: 0x6000007C,
        efuse_base: 0x600B4800,
        mac_efuse_reg: 0x600B4844,
        spi: SpiLayout {
            reg_base: 0x60003000,
            usr_offs: 0x18,
            usr1_offs: 0x1C,
            usr2_offs: 0x20,
            mosi_dlen_offs: Some(0x24),
            miso_dlen_offs: Some(0x28),
            w0_offs: 0x58,
            addr_reg_msb: false,
        },
        watchdog: None,
        has_usb_jtag_serial: true,
        has_usb_otg: false,
        stub_blob_name: "esp32c5",
    },
    // ESP32-P4 — chip_id 18; new SPIMEM1 base in MMIO 0x5008D000.
    Chip {
        name: "ESP32-P4",
        image_chip_id: 18,
        magic_value: None,
        uses_magic: false,
        uart_date_reg_addr: 0x500CA08C,
        efuse_base: 0x5012D000,
        mac_efuse_reg: 0x5012D044,
        spi: SpiLayout {
            reg_base: 0x5008D000,
            usr_offs: 0x18,
            usr1_offs: 0x1C,
            usr2_offs: 0x20,
            mosi_dlen_offs: Some(0x24),
            miso_dlen_offs: Some(0x28),
            w0_offs: 0x58,
            addr_reg_msb: false,
        },
        watchdog: None,
        has_usb_jtag_serial: true,
        has_usb_otg: true,
        stub_blob_name: "esp32p4",
    },
];

/// Where on every chip we read a 32-bit "chip identification magic" from
/// (used by the chips that set `uses_magic = true`).
pub const CHIP_DETECT_MAGIC_REG_ADDR: u32 = 0x40001000;

/// Look up a chip by its `--chip` CLI argument (case-insensitive, accepts
/// either "esp32-s3" or "esp32s3").
pub fn by_name(name: &str) -> Option<&'static Chip> {
    let want = name.to_ascii_lowercase().replace('-', "");
    REGISTRY.iter().find(|c| {
        c.name.to_ascii_lowercase().replace('-', "") == want
    })
}

/// Look up a chip by its IMAGE_CHIP_ID (returned by GET_SECURITY_INFO).
pub fn by_image_chip_id(id: u32) -> Option<&'static Chip> {
    REGISTRY.iter().find(|c| c.image_chip_id as u32 == id)
}

/// Look up a chip by the magic value at CHIP_DETECT_MAGIC_REG_ADDR.
pub fn by_magic(magic: u32) -> Option<&'static Chip> {
    REGISTRY.iter().find(|c| c.magic_value == Some(magic))
}

/// Identify which chip we're connected to.
///
/// 1. Try GET_SECURITY_INFO to get the `chip_id` (works on S3+).
/// 2. Fall back to reading CHIP_DETECT_MAGIC_REG_ADDR for the older chips
///    (ESP32, S2).
///
/// The returned `Chip` is the one whose definition matches. Caller can then
/// compare with the chip the user requested via `--chip`.
pub fn detect(conn: &mut crate::protocol::Connection) -> Result<&'static Chip> {
    use crate::protocol::commands::Cmd;
    use crate::protocol::DEFAULT_TIMEOUT;

    // Try GET_SECURITY_INFO first; it works on S3 and later and is the most
    // reliable source of chip identity.
    if let Ok(resp) = conn.command(Cmd::GetSecurityInfo, &[], 0, DEFAULT_TIMEOUT) {
        // 20-byte payload (newer) or 12-byte (S2). chip_id is bytes 12..16 in
        // the 20-byte payload.
        if resp.data.len() >= 20 {
            let chip_id = u32::from_le_bytes([
                resp.data[12],
                resp.data[13],
                resp.data[14],
                resp.data[15],
            ]);
            if let Some(c) = by_image_chip_id(chip_id) {
                return Ok(c);
            }
        }
        // S2 returns 12 bytes here and we can't tell from chip_id; fall through.
    }

    // Otherwise read the magic register.
    let magic = conn.read_reg(CHIP_DETECT_MAGIC_REG_ADDR)?;
    by_magic(magic).ok_or(Error::UnknownChip {
        magic,
        chip_id: None,
    })
}

/// List all chip names; used by `--chip` help output.
pub fn names() -> Vec<&'static str> {
    REGISTRY.iter().map(|c| c.name).collect()
}
