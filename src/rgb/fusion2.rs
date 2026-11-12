//! Gigabyte RGB Fusion 2 USB controller (ITE IT5702 / IT8297 / IT5711 / IT8950).
//!
//! The protocol as GIGABYTE's own software drives it, observed for interoperability (OpenRGB's
//! `GigabyteRGBFusion2USBController` documents the same packets). Everything is exchanged as
//! 64-byte feature reports with report id 0xCC; there is no ACK protocol, success means the
//! `HidD_SetFeature` call returned TRUE.

use std::time::Duration;

use anyhow::{Context, Result};

use super::transport::{Candidate, HidApiTransport, HidTransport, is_controller_collection};
use crate::error::{ErrorCode, err};

pub const VID_ITE: u16 = 0x048D;
pub const PIDS: [u16; 4] = [0x5702, 0x8297, 0x5711, 0x8950];
pub const USAGE_PAGE: u16 = 0xFF89;
pub const USAGE: u16 = 0xCC;
pub const REPORT_ID: u8 = 0xCC;
pub const PKT: usize = 64;
pub const LEDS_PER_PKT: usize = 19;
/// Product byte of the `[CC 60]` info reply for a motherboard MCU.
pub const PRODUCT_MOTHERBOARD: u8 = 1;

/// ARGB header divisions on IT8297/IT5701/IT5702/IT5711 (D_LED1 = bit 0x20, D_LED2 = bit 0x40).
pub const SLOT_DLED1: u8 = 5;
pub const SLOT_DLED2: u8 = 6;

/// Settle time after clearing registers / leaving a header's built-in effect (GCC: 100 ms).
pub const SETTLE: Duration = Duration::from_millis(100);
/// GCC waits 25 ms after the first whole-board sync packet.
pub const SYNC_SETTLE: Duration = Duration::from_millis(25);

/// Speed tables, index = GCC speed level 0 (slowest) ..= 9 (fastest): GIGABYTE Control Center's
/// default timings.
pub mod speed {
    pub const PULSE: [u16; 10] = [1600, 1400, 1200, 1000, 900, 800, 700, 600, 500, 400];
    pub const PULSE_T2: u16 = 200;
    pub const FLASH: [u16; 10] = [2400, 2200, 2000, 1800, 1600, 1400, 1200, 1000, 800, 600];
    pub const DFLASH: [u16; 10] = [2600, 2400, 2200, 2000, 1800, 1600, 1400, 1200, 1000, 800];
    pub const CYCLE: [(u16, u16); 10] = [
        (2400, 2200),
        (1100, 900),
        (1000, 800),
        (900, 700),
        (800, 600),
        (700, 500),
        (600, 400),
        (500, 300),
        (400, 200),
        (300, 100),
    ];
    pub const WAVE: [u16; 10] = [300, 250, 205, 165, 130, 100, 75, 55, 40, 30];
}

/// Built-in firmware effects. `DoubleFlash` is sent as mode 3 with `CtrlOem0 = 2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HwMode {
    Static,
    Pulse,
    Blink,
    DoubleFlash,
    ColorCycle,
    Wave,
    Random,
}

/// One effect register's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HwEffect {
    pub mode: HwMode,
    pub color: [u8; 3],
    /// 0..=255 (Pulse is rescaled to GCC's 0..=100).
    pub brightness: u8,
    /// GCC speed level 0 (slowest) ..= 9 (fastest).
    pub level: u8,
}

impl HwEffect {
    pub fn new(mode: HwMode, color: [u8; 3], brightness: u8, level: u8) -> Self {
        HwEffect { mode, color, brightness, level: level.min(9) }
    }
    /// A plain colour (GCC's sync packet and software colours on 12 V zones: Static, brightness 255).
    pub fn solid(color: [u8; 3]) -> Self {
        HwEffect::new(HwMode::Static, color, 255, 0)
    }
    /// GCC "Off": Static, colour 0, brightness 255.
    pub fn off() -> Self {
        HwEffect::solid([0, 0, 0])
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
#[allow(dead_code)] // several fields are only shown in diagnostics output
pub struct Fusion2Info {
    pub product: u8,
    pub device_num: u8,
    pub strip_detect: u8,
    pub fw: String,
    pub name: String,
    pub support_flags: u8,
    pub cal_strip0: u32,
    pub cal_strip1: u32,
    pub rgb_cali: u32,
    pub chip_id: u32,
}

impl Fusion2Info {
    /// `[CC 47 xx]` "keep LED settings" is supported (SuppCmdFlag bit 0).
    pub fn supports_persistence(&self) -> bool {
        self.support_flags & 0x01 != 0
    }
    /// `[CC 48]` Windows Dynamic Lighting switch is supported (SuppCmdFlag bit 1).
    pub fn supports_dynamic_lighting(&self) -> bool {
        self.support_flags & 0x02 != 0
    }
}

/// Byte positions (r, g, b) inside a 3-byte LED slot.
pub type ByteOrder = [usize; 3];

/// WS2812-style default byte order: G, R, B.
pub const ORDER_GRB: ByteOrder = [1, 0, 2];

/// Decodes a stored strip calibration value. Byte 3 is ignored; the low three bytes must be a
/// permutation of {0, 1, 2}.
pub fn order_from_calibration(cal: u32) -> Option<ByteOrder> {
    let b = (cal & 0xFF) as usize;
    let g = ((cal >> 8) & 0xFF) as usize;
    let r = ((cal >> 16) & 0xFF) as usize;
    if r > 2 || g > 2 || b > 2 || r == g || r == b || g == b { None } else { Some([r, g, b]) }
}

/// Wire order string like "GRB" to byte positions.
pub fn order_from_str(s: &str) -> Option<ByteOrder> {
    let s = s.to_ascii_uppercase();
    if s.len() != 3 {
        return None;
    }
    let r = s.find('R')?;
    let g = s.find('G')?;
    let b = s.find('B')?;
    Some([r, g, b])
}

pub fn led_count_enum(n: u16) -> u8 {
    match n {
        0..=32 => 0,
        33..=64 => 1,
        65..=256 => 2,
        257..=512 => 3,
        _ => 4,
    }
}

/// ARGB header divisions. IT8950 / MixType1 uses divisions 2/3.
pub fn dled_slots(pid: u16) -> [u8; 2] {
    if pid == 0x8950 { [2, 3] } else { [SLOT_DLED1, SLOT_DLED2] }
}

/// `Zone_Sel0` for "all zones" (`write_to_mcu(-1)`).
pub fn all_zones_mask(pid: u16) -> u32 {
    if pid == 0x5711 { 0x7FF } else { 0xFF }
}

/// Effect register of a division: 0..=7 are 0x20..=0x27, 8..=10 (IT5711 only) 0x90..=0x92. The
/// command byte is computed from the division, so an out-of-range one must never reach the wire
/// (audit FR-CTL-005: 0x90 + n wraps, and e.g. division 171 would address 0x33, the pin-definition
/// table, 214 the 0x5E flash command). Such a division is a bug; it is mapped to the last one.
fn effect_register(slot: u8) -> u8 {
    let slot = slot.min(10);
    if slot < 8 { 0x20 + slot } else { 0x90 + (slot - 8) }
}

pub fn cmd_packet(bytes: &[u8]) -> [u8; PKT] {
    let mut p = [0u8; PKT];
    p[0] = REPORT_ID;
    p[1..1 + bytes.len()].copy_from_slice(bytes);
    p
}

/// All-zero record (mode 0, zone 0): how GCC releases a register.
pub fn clear_packet(slot: u8) -> [u8; PKT] {
    cmd_packet(&[effect_register(slot)])
}

/// Effect record for `slot`, driving the zones in `zones`.
pub fn effect_packet(slot: u8, e: &HwEffect, zones: u32) -> [u8; PKT] {
    let mut p = [0u8; PKT];
    p[0] = REPORT_ID;
    p[1] = effect_register(slot);
    p[2..6].copy_from_slice(&zones.to_le_bytes());
    let lv = e.level.min(9) as usize;
    let mut t = [0u16; 4];
    let mut ctrl = [0u8; 4]; // CtrlVal0, CtrlVal1, CtrlOem0, CtrlOem1
    let mut bri = e.brightness;
    let mode = match e.mode {
        HwMode::Static => 1,
        HwMode::Pulse => {
            t[0] = speed::PULSE[lv];
            t[1] = speed::PULSE[lv];
            t[2] = speed::PULSE_T2;
            ctrl[1] = 1;
            bri = (e.brightness as u16 * 100 / 255) as u8;
            2
        }
        HwMode::Blink => {
            t = [100, 100, speed::FLASH[lv], 0];
            ctrl[1] = 1;
            ctrl[2] = 1;
            3
        }
        HwMode::DoubleFlash => {
            t = [100, 100, speed::DFLASH[lv], 0];
            ctrl[1] = 1;
            ctrl[2] = 2;
            3
        }
        HwMode::ColorCycle => {
            (t[0], t[1]) = speed::CYCLE[lv];
            ctrl[0] = 7;
            4
        }
        HwMode::Wave => {
            t[0] = speed::WAVE[lv];
            ctrl[0] = 7;
            ctrl[1] = 1;
            6
        }
        HwMode::Random => {
            // OpenRGB's values (CtrlVal0 = 1, CtrlVal1 = 5). GIGABYTE Control Center writes
            // CtrlVal0 = 5 instead, but its UI never offers Random: untested on hardware (F7).
            t[0] = 100;
            ctrl[0] = 1;
            ctrl[1] = 5;
            8
        }
    };
    p[11] = mode;
    p[12] = bri;
    p[13] = 0;
    let rgb = ((e.color[0] as u32) << 16) | ((e.color[1] as u32) << 8) | e.color[2] as u32;
    p[14..18].copy_from_slice(&rgb.to_le_bytes());
    for (k, v) in t.iter().enumerate() {
        p[22 + k * 2..24 + k * 2].copy_from_slice(&v.to_le_bytes());
    }
    p[30..34].copy_from_slice(&ctrl);
    p
}

/// Per-LED streaming packets (0x58 D_LED1 / 0x59 D_LED2): u16 byte offset, byte count (max 57),
/// colour bytes in `order`.
pub fn strip_packets(header: u8, colors: &[[u8; 3]], order: ByteOrder) -> Vec<[u8; PKT]> {
    let mut out = Vec::with_capacity(colors.len().div_ceil(LEDS_PER_PKT));
    for (k, chunk) in colors.chunks(LEDS_PER_PKT).enumerate() {
        let mut p = [0u8; PKT];
        p[0] = REPORT_ID;
        p[1] = if header == 0 { 0x58 } else { 0x59 };
        p[2..4].copy_from_slice(&((k * LEDS_PER_PKT * 3) as u16).to_le_bytes());
        p[4] = (chunk.len() * 3) as u8;
        for (i, c) in chunk.iter().enumerate() {
            let o = 5 + i * 3;
            p[o + order[0]] = c[0];
            p[o + order[1]] = c[1];
            p[o + order[2]] = c[2];
        }
        out.push(p);
    }
    out
}

/// Validates and decodes a `[CC 60]` reply (the controller's info record).
pub fn parse_info(buf: &[u8], n: usize) -> Result<Fusion2Info> {
    if n < PKT || buf.len() < PKT {
        return Err(err(ErrorCode::RgbBadReply, format!("device info reply is too short ({n} of {PKT} bytes)")));
    }
    if buf[0] != REPORT_ID {
        return Err(err(
            ErrorCode::RgbBadReply,
            format!("device info reply has report id {:02X}, expected CC", buf[0]),
        ));
    }
    let payload = &buf[1..PKT];
    if payload.iter().all(|&b| b == 0) || payload.iter().all(|&b| b == 0xFF) {
        return Err(err(
            ErrorCode::RgbBadReply,
            format!("device info reply is all {:02X}: the controller is not responding", payload[0]),
        ));
    }
    if buf[1] != PRODUCT_MOTHERBOARD {
        return Err(err(
            ErrorCode::RgbUnsupported,
            format!("device type {} is not a motherboard RGB controller (expected {PRODUCT_MOTHERBOARD})", buf[1]),
        ));
    }
    let u32_at = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    let fw = u32_at(4);
    let name_raw = &buf[12..40];
    let end = name_raw.iter().position(|&c| c == 0).unwrap_or(name_raw.len());
    let name: String =
        name_raw[..end].iter().map(|&c| if (0x20..0x7F).contains(&c) { c as char } else { '?' }).collect();
    Ok(Fusion2Info {
        product: buf[1],
        device_num: buf[2],
        strip_detect: buf[3],
        // The firmware version is shown byte-reversed, b4.b5.b6.b7, as GIGABYTE's software shows it.
        fw: format!("{}.{}.{}.{}", fw & 0xFF, (fw >> 8) & 0xFF, (fw >> 16) & 0xFF, (fw >> 24) & 0xFF),
        name: name.trim().to_string(),
        support_flags: buf[11],
        cal_strip0: u32_at(44),
        cal_strip1: u32_at(48),
        rgb_cali: u32_at(52),
        chip_id: u32_at(56),
    })
}

/// `[CC 48]` reply byte 1: 1 = Windows Dynamic Lighting is off, 99 = the firmware does
/// not support the query (unknown), anything else = Dynamic Lighting may drive the LEDs.
pub fn dynamic_lighting_state(reply: u8) -> Option<bool> {
    match reply {
        1 => Some(false),
        99 => None,
        _ => Some(true),
    }
}

/// A connected controller. Keeps the 0x32 / 0x34 state to avoid redundant writes.
pub struct Fusion2 {
    io: Box<dyn HidTransport>,
    pub pid: u16,
    pub info: Fusion2Info,
    /// Windows Dynamic Lighting state from `[CC 48]` (true = Windows may drive the LEDs).
    pub dynamic_lighting: Option<bool>,
    /// Raw `[CC 48]` reply byte 1 (diagnostics; None = not queried).
    pub dynamic_lighting_raw: Option<u8>,
    direct_bits: Option<u8>,
    led_count_bytes: Option<[u8; 3]>,
    /// Feature reports written since connect.
    pub packets: u64,
}

impl Fusion2 {
    /// Reads and validates the device info. Does not change any lighting.
    pub fn connect(io: Box<dyn HidTransport>, pid: u16) -> Result<Self> {
        let mut f = Fusion2 {
            io,
            pid,
            info: Fusion2Info::default(),
            dynamic_lighting: None,
            dynamic_lighting_raw: None,
            direct_bits: None,
            led_count_bytes: None,
            packets: 0,
        };
        f.info = f.read_info()?;
        Ok(f)
    }

    /// Finds and opens the first supported controller with hidapi (used by `--diag`).
    pub fn open(api: &hidapi::HidApi) -> Result<Option<Self>> {
        let cands: Vec<Candidate> = api
            .device_list()
            .filter(|d| is_controller_collection(d.vendor_id(), d.product_id(), d.usage_page(), d.usage()))
            .map(|d| Candidate { path: d.path().to_owned(), pid: d.product_id() })
            .collect();
        let mut last = None;
        for c in cands {
            let dev = match api.open_path(&c.path) {
                Ok(d) => d,
                Err(e) => {
                    last = Some(super::transport::io_error(
                        "opening the RGB controller",
                        super::transport::hid_win32(&e),
                        &e.to_string(),
                        true,
                    ));
                    continue;
                }
            };
            match Fusion2::connect(Box::new(HidApiTransport::new(dev)), c.pid) {
                Ok(f) => return Ok(Some(f)),
                Err(e) => last = Some(e),
            }
        }
        match last {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }

    fn send(&mut self, p: &[u8; PKT]) -> Result<()> {
        self.packets += 1;
        self.io.send_feature(p)
    }

    fn cmd(&mut self, bytes: &[u8]) -> Result<()> {
        self.send(&cmd_packet(bytes))
    }

    pub fn sleep(&self, d: Duration) {
        self.io.sleep(d);
    }

    /// A query: `HidD_SetFeature([CC cmd 00..])` then `HidD_GetFeature`.
    fn query(&mut self, cmd: u8) -> Result<([u8; PKT], usize)> {
        self.cmd(&[cmd, 0])?;
        let mut buf = [0u8; PKT];
        buf[0] = REPORT_ID;
        let n = self.io.get_feature(&mut buf)?;
        Ok((buf, n))
    }

    pub fn read_info(&mut self) -> Result<Fusion2Info> {
        let (buf, n) = self.query(0x60)?;
        parse_info(&buf, n)
    }

    /// Re-reads the info block and checks it still describes the same controller.
    pub fn check_alive(&mut self) -> Result<()> {
        let now = self.read_info()?;
        if now.fw != self.info.fw || now.chip_id != self.info.chip_id {
            return Err(err(
                ErrorCode::RgbBadReply,
                format!(
                    "controller identity changed (firmware {} -> {}, chip {:08X} -> {:08X})",
                    self.info.fw, now.fw, self.info.chip_id, now.chip_id
                ),
            ));
        }
        Ok(())
    }

    /// Forgets the cached 0x32 / 0x34 state (after resume or a firmware reset).
    pub fn invalidate(&mut self) {
        self.direct_bits = None;
        self.led_count_bytes = None;
    }

    /// Start-up sequence (the one GIGABYTE's software sends): leave LampArray mode, default LED count,
    /// normal (non-persist) mode, built-in effects on, all registers cleared, 100 ms settle,
    /// beat mode off.
    pub fn init(&mut self) -> Result<()> {
        self.invalidate();
        self.info = self.read_info()?;
        if self.info.supports_dynamic_lighting() {
            // Byte-identical to GCC's 0x48 query and OpenRGB's "disable LampArray" write.
            let (buf, n) = self.query(0x48)?;
            self.dynamic_lighting_raw = (n >= 2 && buf[0] == REPORT_ID).then_some(buf[1]);
            self.dynamic_lighting = self.dynamic_lighting_raw.and_then(dynamic_lighting_state);
        }
        self.set_led_counts(1, 1)?;
        if self.info.supports_persistence() {
            self.cmd(&[0x47, 0])?;
        }
        self.set_direct_bits(0)?;
        self.clear_all_registers()?;
        self.sleep(SETTLE);
        self.cmd(&[0x31, 0])?;
        Ok(())
    }

    /// Zeroes every effect register and applies.
    pub fn clear_all_registers(&mut self) -> Result<()> {
        for reg in 0x20u8..=0x27 {
            self.cmd(&[reg])?;
        }
        if self.pid == 0x5711 {
            for reg in 0x90u8..=0x92 {
                self.cmd(&[reg])?;
            }
        }
        self.apply_all()
    }

    pub fn apply_all(&mut self) -> Result<()> {
        self.cmd(&[0x28, 0xFF, if self.pid == 0x5711 { 0x07 } else { 0x00 }])
    }

    /// Applies pending effect records for the slots in `mask`.
    pub fn apply(&mut self, mask: u32) -> Result<()> {
        let mut p = cmd_packet(&[0x28]);
        p[2..6].copy_from_slice(&mask.to_le_bytes());
        self.send(&p)
    }

    pub fn clear_slot(&mut self, slot: u8) -> Result<()> {
        self.send(&clear_packet(slot))
    }

    pub fn set_effect(&mut self, slot: u8, e: &HwEffect, zones: u32) -> Result<()> {
        self.send(&effect_packet(slot, e, zones))
    }

    /// `[CC 34 nn]`: LED count enum per ARGB header. Returns whether a packet was sent.
    pub fn set_led_counts(&mut self, d1: u16, d2: u16) -> Result<bool> {
        let bytes = [(led_count_enum(d2) << 4) | led_count_enum(d1), 0, 0];
        if self.led_count_bytes == Some(bytes) {
            return Ok(false);
        }
        self.cmd(&[0x34, bytes[0], bytes[1], bytes[2]])?;
        self.led_count_bytes = Some(bytes);
        Ok(true)
    }

    pub fn force_led_counts(&mut self) {
        self.led_count_bytes = None;
    }

    /// Current 0x32 bits (bit 0 = D_LED1 direct, bit 1 = D_LED2 direct), 0 if unknown.
    pub fn direct_bits(&self) -> u8 {
        self.direct_bits.unwrap_or(0)
    }

    /// `[CC 32 bits]`: per-header streaming enable. Returns whether a packet was sent.
    pub fn set_direct_bits(&mut self, bits: u8) -> Result<bool> {
        if self.direct_bits == Some(bits) {
            return Ok(false);
        }
        self.cmd(&[0x32, bits])?;
        self.direct_bits = Some(bits);
        Ok(true)
    }

    /// Streams per-LED colours to an ARGB header (0 = D_LED1, 1 = D_LED2).
    pub fn send_strip(&mut self, header: u8, colors: &[[u8; 3]], order: ByteOrder) -> Result<()> {
        for p in strip_packets(header, colors, order) {
            self.send(&p)?;
        }
        Ok(())
    }

    /// `[CC 47 01]` keep the current hardware effects through sleep/shutdown, `[CC 47 00]` normal
    /// mode. Returns `Ok(false)` if the firmware doesn't support it (no 0x5E flash
    /// write, that one is only for calibration).
    pub fn persist(&mut self, keep: bool) -> Result<bool> {
        if !self.info.supports_persistence() {
            return Ok(false);
        }
        self.cmd(&[0x47, keep as u8]).with_context(|| crate::error::AppError::new(ErrorCode::RgbFlashFailed, ""))?;
        Ok(true)
    }

    pub fn dled_slot(&self, header: u8) -> u8 {
        dled_slots(self.pid)[header.min(1) as usize]
    }

    pub fn all_zones(&self) -> u32 {
        all_zones_mask(self.pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::code_of;
    use crate::rgb::mock::{MockDev, Op, info_reply, pkt};

    fn le16(p: &[u8; PKT], o: usize) -> u16 {
        u16::from_le_bytes([p[o], p[o + 1]])
    }

    /// audit FR-CTL-005: whatever the division, an effect record only ever addresses an effect register.
    #[test]
    fn effect_records_only_address_effect_registers() {
        for slot in 0..=255u8 {
            for reg in [clear_packet(slot)[1], effect_packet(slot, &HwEffect::off(), 0)[1]] {
                assert!((0x20..=0x27).contains(&reg) || (0x90..=0x92).contains(&reg), "division {slot}: {reg:02X}");
            }
        }
    }

    #[test]
    fn calibration_decoding() {
        // "GRB" as stored by Gigabyte: red at byte 1, green at byte 0, blue at byte 2.
        assert_eq!(order_from_calibration(0x0001_0002), Some([1, 0, 2]));
        // F11: byte 3 is ignored.
        assert_eq!(order_from_calibration(0x7F01_0002), Some([1, 0, 2]));
        // identity pin define {0,1,2,0} = 0x00020100 -> wire B G R
        assert_eq!(order_from_calibration(0x0002_0100), Some([2, 1, 0]));
        assert_eq!(order_from_str("GRB"), Some([1, 0, 2]));
        assert_eq!(order_from_str("RGB"), Some([0, 1, 2]));
        assert_eq!(order_from_str("Auto (controller)"), None);
        assert_eq!(order_from_calibration(0), None);
        assert_eq!(order_from_calibration(0x0101_0101), None);
        assert_eq!(order_from_calibration(0x0003_0100), None);
    }

    #[test]
    fn led_count_enums() {
        assert_eq!(led_count_enum(30), 0);
        assert_eq!(led_count_enum(32), 0);
        assert_eq!(led_count_enum(60), 1);
        assert_eq!(led_count_enum(120), 2);
        assert_eq!(led_count_enum(512), 3);
        assert_eq!(led_count_enum(600), 4);
    }

    #[test]
    fn slots_depend_on_pid() {
        assert_eq!(dled_slots(0x5702), [5, 6]);
        assert_eq!(dled_slots(0x8950), [2, 3]);
        assert_eq!(all_zones_mask(0x5702), 0xFF);
        assert_eq!(all_zones_mask(0x5711), 0x7FF);
    }

    #[test]
    fn effect_records_match_gcc_tables() {
        let c = [0x11, 0x22, 0x33];
        for lv in 0..10u8 {
            let i = lv as usize;
            // Static
            let p = effect_packet(1, &HwEffect::new(HwMode::Static, c, 200, lv), 0x02);
            assert_eq!(&p[..14], &[0xCC, 0x21, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 1, 200, 0]);
            assert_eq!(&p[14..18], &[0x33, 0x22, 0x11, 0x00], "colour is 0x00RRGGBB LE");
            assert!(p[18..].iter().all(|&b| b == 0));
            // Pulse: MaxBrightness 0..100, t0 = t1 = table, t2 = 200, CtrlVal1 = 1 (F5)
            let p = effect_packet(5, &HwEffect::new(HwMode::Pulse, c, 255, lv), 0x20);
            assert_eq!((p[1], p[2], p[11], p[12]), (0x25, 0x20, 2, 100));
            assert_eq!((le16(&p, 22), le16(&p, 24), le16(&p, 26)), (speed::PULSE[i], speed::PULSE[i], 200));
            assert_eq!(&p[30..34], &[0, 1, 0, 0]);
            assert_eq!(effect_packet(5, &HwEffect::new(HwMode::Pulse, c, 102, lv), 0x20)[12], 40);
            // Flash (F3, F4)
            let p = effect_packet(4, &HwEffect::new(HwMode::Blink, c, 77, lv), 0x10);
            assert_eq!((p[11], p[12]), (3, 77));
            assert_eq!((le16(&p, 22), le16(&p, 24), le16(&p, 26)), (100, 100, speed::FLASH[i]));
            assert_eq!(&p[30..34], &[0, 1, 1, 0]);
            // Double flash
            let p = effect_packet(4, &HwEffect::new(HwMode::DoubleFlash, c, 77, lv), 0x10);
            assert_eq!(p[11], 3);
            assert_eq!((le16(&p, 22), le16(&p, 24), le16(&p, 26)), (100, 100, speed::DFLASH[i]));
            assert_eq!(&p[30..34], &[0, 1, 2, 0]);
            // Colour cycle (F6)
            let p = effect_packet(1, &HwEffect::new(HwMode::ColorCycle, c, 255, lv), 0x02);
            assert_eq!((p[11], p[12]), (4, 255));
            assert_eq!((le16(&p, 22), le16(&p, 24), le16(&p, 26)), (speed::CYCLE[i].0, speed::CYCLE[i].1, 0));
            assert_eq!(&p[30..34], &[7, 0, 0, 0]);
            // Wave (ARGB): mode 6, CtrlVal0 = 7, CtrlVal1 = 1
            let p = effect_packet(6, &HwEffect::new(HwMode::Wave, c, 255, lv), 0x40);
            assert_eq!((p[1], p[2], p[11]), (0x26, 0x40, 6));
            assert_eq!((le16(&p, 22), le16(&p, 24)), (speed::WAVE[i], 0));
            assert_eq!(&p[30..34], &[7, 1, 0, 0]);
            // Random
            let p = effect_packet(5, &HwEffect::new(HwMode::Random, c, 255, lv), 0x20);
            assert_eq!((p[11], le16(&p, 22)), (8, 100));
            assert_eq!(&p[30..32], &[1, 5]);
        }
        // Exact values: the default timings of GIGABYTE's software.
        let t2 = |m, lv| le16(&effect_packet(1, &HwEffect::new(m, c, 255, lv), 2), 26);
        assert_eq!(t2(HwMode::Blink, 9).to_le_bytes(), [0x58, 0x02]);
        assert_eq!(t2(HwMode::DoubleFlash, 9).to_le_bytes(), [0x20, 0x03]);
        assert_eq!(t2(HwMode::Blink, 0).to_le_bytes(), [0x60, 0x09]);
        assert_eq!(t2(HwMode::DoubleFlash, 0).to_le_bytes(), [0x28, 0x0A]);
        let p = effect_packet(1, &HwEffect::new(HwMode::ColorCycle, c, 255, 0), 2);
        assert_eq!((le16(&p, 22), le16(&p, 24)), (2400, 2200));
        // Registers for divisions 8..10 (IT5711).
        assert_eq!(effect_packet(9, &HwEffect::off(), 1 << 9)[1], 0x91);
        assert_eq!(clear_packet(5), cmd_packet(&[0x25]));
    }

    #[test]
    fn strip_packets_are_19_leds_with_byte_offsets() {
        let colors: Vec<[u8; 3]> = (0..40u8).map(|i| [i, 100 + i, 200]).collect();
        let pk = strip_packets(0, &colors, ORDER_GRB);
        assert_eq!(pk.len(), 3);
        assert_eq!((pk[0][1], le16(&pk[0], 2), pk[0][4]), (0x58, 0, 57));
        assert_eq!((le16(&pk[1], 2), pk[1][4]), (57, 57));
        assert_eq!((le16(&pk[2], 2), pk[2][4]), (114, 6)); // 19 + 19 + 2 LEDs
        // GRB on the wire
        assert_eq!(&pk[0][5..8], &[100, 0, 200]);
        assert_eq!(&pk[1][5..8], &[119, 19, 200]);
        assert!(pk[2][11..].iter().all(|&b| b == 0));
        let pk = strip_packets(1, &colors[..19], [0, 1, 2]);
        assert_eq!(pk.len(), 1);
        assert_eq!((pk[0][1], pk[0][4]), (0x59, 57));
        assert_eq!(&pk[0][59..62], &[18, 118, 200]);
    }

    #[test]
    fn info_validation() {
        let good = info_reply(0x03);
        let i = parse_info(&good, 64).unwrap();
        assert_eq!(i.name, "IT5702-GIGABYTE V2.0.10.0");
        assert_eq!(i.fw, "2.0.10.0");
        assert_eq!(order_from_calibration(i.cal_strip0), Some(ORDER_GRB));
        assert_eq!(code_of(&parse_info(&good, 20).unwrap_err()), ErrorCode::RgbBadReply);
        let mut bad = good;
        bad[0] = 0x00;
        assert_eq!(code_of(&parse_info(&bad, 64).unwrap_err()), ErrorCode::RgbBadReply);
        let mut zero = [0u8; 64];
        zero[0] = 0xCC;
        assert_eq!(code_of(&parse_info(&zero, 64).unwrap_err()), ErrorCode::RgbBadReply);
        let mut ff = [0xFFu8; 64];
        ff[0] = 0xCC;
        assert_eq!(code_of(&parse_info(&ff, 64).unwrap_err()), ErrorCode::RgbBadReply);
        let mut monitor = good;
        monitor[1] = 3;
        assert_eq!(code_of(&parse_info(&monitor, 64).unwrap_err()), ErrorCode::RgbUnsupported);
    }

    #[test]
    fn init_sequence_matches_gcc() {
        let m = MockDev::new(0x03);
        let mut f = Fusion2::connect(m.transport(), 0x5702).unwrap();
        m.clear_log();
        f.init().unwrap();
        let mut want = vec![
            Op::Send(pkt(&[0x60, 0])),
            Op::Get(0x60),
            Op::Send(pkt(&[0x48, 0])),
            Op::Get(0x48),
            Op::Send(pkt(&[0x34, 0, 0, 0])),
            Op::Send(pkt(&[0x47, 0])),
            Op::Send(pkt(&[0x32, 0])),
        ];
        for reg in 0x20..=0x27 {
            want.push(Op::Send(pkt(&[reg])));
        }
        want.push(Op::Send(pkt(&[0x28, 0xFF, 0])));
        want.push(Op::Sleep(100));
        want.push(Op::Send(pkt(&[0x31, 0])));
        assert_eq!(m.log(), want);
        assert_eq!(f.dynamic_lighting, Some(false), "reply [1] = 1 means Dynamic Lighting is off");
        m.set_dynamic_lighting_reply(99);
        f.init().unwrap();
        assert_eq!(f.dynamic_lighting, None, "99 = query unsupported, not \"Dynamic Lighting on\"");
        assert_eq!(f.dynamic_lighting_raw, Some(99), "raw byte kept for diagnostics");
        m.set_dynamic_lighting_reply(0);
        f.init().unwrap();
        assert_eq!(f.dynamic_lighting, Some(true));
        assert_eq!(f.dynamic_lighting_raw, Some(0));

        // Without SuppCmdFlag bits neither 0x48 nor 0x47 is sent.
        let m = MockDev::new(0x00);
        let mut f = Fusion2::connect(m.transport(), 0x5702).unwrap();
        f.init().unwrap();
        assert!(!m.sent().iter().any(|p| p[1] == 0x48 || p[1] == 0x47));
    }

    #[test]
    fn persistence_is_0x47_only() {
        let m = MockDev::new(0x01);
        let mut f = Fusion2::connect(m.transport(), 0x5702).unwrap();
        m.clear_log();
        assert!(f.persist(true).unwrap());
        assert!(f.persist(false).unwrap());
        assert_eq!(m.log(), vec![Op::Send(pkt(&[0x47, 1])), Op::Send(pkt(&[0x47, 0]))]);
        let m = MockDev::new(0x00);
        let mut f = Fusion2::connect(m.transport(), 0x5702).unwrap();
        m.clear_log();
        assert!(!f.persist(true).unwrap());
        assert!(m.log().is_empty());
        // A failed 0x47 write is reported as a flash failure.
        let m = MockDev::new(0x01);
        let mut f = Fusion2::connect(m.transport(), 0x5702).unwrap();
        m.fail_next_send(31);
        assert_eq!(code_of(&f.persist(true).unwrap_err()), ErrorCode::RgbFlashFailed);
    }

    #[test]
    fn led_counts_and_direct_bits_are_cached() {
        let m = MockDev::new(0x03);
        let mut f = Fusion2::connect(m.transport(), 0x5702).unwrap();
        m.clear_log();
        assert!(f.set_led_counts(30, 60).unwrap());
        assert!(!f.set_led_counts(31, 64).unwrap());
        assert!(f.set_led_counts(300, 1024).unwrap());
        assert!(f.set_direct_bits(1).unwrap());
        assert!(!f.set_direct_bits(1).unwrap());
        assert_eq!(
            m.log(),
            vec![Op::Send(pkt(&[0x34, 0x10])), Op::Send(pkt(&[0x34, 0x43])), Op::Send(pkt(&[0x32, 0x01]))]
        );
    }

    #[test]
    fn identity_check() {
        let m = MockDev::new(0x03);
        let mut f = Fusion2::connect(m.transport(), 0x5702).unwrap();
        f.check_alive().unwrap();
        m.set_info_byte(4, 9);
        assert_eq!(code_of(&f.check_alive().unwrap_err()), ErrorCode::RgbBadReply);
    }
}
