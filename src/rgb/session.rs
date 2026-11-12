//! Turns the lighting engine's desired output into the minimal packet sequence for a connected
//! controller, and remembers what the controller currently shows.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::fusion2::{ByteOrder, Fusion2, HwEffect, HwMode, ORDER_GRB, SETTLE, SYNC_SETTLE, order_from_calibration};
use crate::config::ExitLighting;

/// Re-send streamed frames / software colours at least this often.
pub const KEEPALIVE: Duration = Duration::from_secs(1);

/// What an ARGB header should show.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum HeaderOut {
    /// Not driven by FanRGB (left as is).
    #[default]
    Unused,
    /// Per-LED streaming (software effect).
    Direct(Vec<[u8; 3]>),
    /// A built-in effect on the header's own division.
    Hw(HwEffect),
}

/// What a single-colour (12 V / onboard) division should show.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SingleOut {
    /// A software colour (re-sent as a Static record whenever it changes).
    Soft([u8; 3]),
    Hw(HwEffect),
}

/// The complete desired controller state for one frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Desired {
    /// LED counts of D_LED1 / D_LED2.
    pub led_counts: [u16; 2],
    pub headers: [HeaderOut; 2],
    /// User colour order per header; `None` = controller calibration.
    pub order_override: [Option<ByteOrder>; 2],
    /// (division, output) for single-colour zones.
    pub singles: Vec<(u8, SingleOut)>,
    /// Whole board shows one colour: GCC's sync path, one `[CC 20 FF ..]` record + apply instead
    /// of per-zone packets. `headers`/`singles` still describe the zones.
    pub sync_all: Option<[u8; 3]>,
    /// Board-wide hardware wave; overrides everything else.
    pub whole_wave: Option<HwEffect>,
}

impl Desired {
    /// Feature reports needed for one changed frame (used for the frame-rate cap, M10).
    pub fn packets_per_frame(&self) -> usize {
        if self.whole_wave.is_some() {
            return 0;
        }
        if self.sync_all.is_some() {
            return 2;
        }
        let strips: usize = self
            .headers
            .iter()
            .map(|h| match h {
                HeaderOut::Direct(f) => f.len().div_ceil(super::fusion2::LEDS_PER_PKT),
                _ => 0,
            })
            .sum();
        let soft = self.singles.iter().filter(|(_, o)| matches!(o, SingleOut::Soft(_))).count();
        strips + if soft > 0 { soft + 1 } else { 0 }
    }

    /// Whether frames must be paced like GCC's sync path (an apply each frame).
    pub fn applies_per_frame(&self) -> bool {
        self.sync_all.is_some() || self.singles.iter().any(|(_, o)| matches!(o, SingleOut::Soft(_)))
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Board {
    /// Per-zone registers / streaming.
    Normal,
    /// GCC sync: one record for all zones.
    SyncAll(Option<[u8; 3]>),
    WholeWave(HwEffect),
}

#[derive(Clone, Debug)]
struct StripSent {
    frame: Vec<[u8; 3]>,
    order: ByteOrder,
    at: Instant,
}

#[derive(Clone, Copy, Debug)]
struct SlotSent {
    eff: HwEffect,
    at: Instant,
}

/// A connected, initialised controller plus a cache of what it shows.
pub struct Session {
    pub dev: Fusion2,
    board: Board,
    strips: [Option<StripSent>; 2],
    slots: HashMap<u8, SlotSent>,
    sync_at: Option<Instant>,
    /// Effect records written (to invalidate "saved to controller").
    pub hw_writes: u64,
}

/// The colour to hand a firmware that places channel `k` at byte `cal[k]`, so that the wire carries
/// channel `ch` at byte `want[ch]` (both are permutations of 0..3).
pub fn remap_color(c: [u8; 3], cal: ByteOrder, want: ByteOrder) -> [u8; 3] {
    let mut out = [0u8; 3];
    for ch in 0..3 {
        if let Some(k) = (0..3).find(|&k| cal[k] == want[ch]) {
            out[k] = c[ch];
        }
    }
    out
}

/// The one colour of a frame whose LEDs all show the same bytes.
pub fn uniform(frame: &[[u8; 3]]) -> Option<[u8; 3]> {
    let first = *frame.first()?;
    frame.iter().all(|c| *c == first).then_some(first)
}

impl Session {
    /// `dev` must already be initialised ([`Fusion2::init`]: registers cleared, direct off).
    pub fn new(dev: Fusion2) -> Self {
        Session { dev, board: Board::Normal, strips: [None, None], slots: HashMap::new(), sync_at: None, hw_writes: 0 }
    }

    /// Forget everything the controller shows (after a re-init).
    fn reset_cache(&mut self) {
        self.board = Board::Normal;
        self.strips = [None, None];
        self.slots.clear();
        self.sync_at = None;
    }

    /// Resume from sleep / firmware reset: `[CC 47 00]` + full re-init, caches dropped.
    pub fn reinit(&mut self) -> Result<()> {
        self.reset_cache();
        self.dev.init()
    }

    fn order_for(&self, h: usize, d: &Desired) -> ByteOrder {
        d.order_override[h]
            .or_else(|| {
                order_from_calibration(if h == 1 { self.dev.info.cal_strip1 } else { self.dev.info.cal_strip0 })
            })
            .unwrap_or(ORDER_GRB)
    }

    /// An effect record for ARGB header `h`. The firmware lays the record colour out with its own
    /// calibration order; with a user colour-order override the colour is pre-permuted so the LEDs
    /// receive the bytes in the user's order, as streamed frames do.
    fn ordered_record(&self, h: usize, d: &Desired, e: HwEffect) -> HwEffect {
        let Some(want) = d.order_override[h] else { return e };
        let cal = order_from_calibration(if h == 1 { self.dev.info.cal_strip1 } else { self.dev.info.cal_strip0 })
            .unwrap_or(ORDER_GRB);
        HwEffect { color: remap_color(e.color, cal, want), ..e }
    }

    /// Brings the controller to `d`, sending only what changed (plus keep-alives).
    /// Returns the number of feature reports written.
    pub fn apply(&mut self, d: &Desired, now: Instant) -> Result<u64> {
        let p0 = self.dev.packets;
        if let Some(w) = d.whole_wave {
            self.apply_whole_wave(w, d)?;
        } else if let Some(c) = d.sync_all {
            self.apply_sync_all(c, d, now)?;
        } else {
            self.apply_normal(d, now)?;
        }
        Ok(self.dev.packets - p0)
    }

    fn counts(&mut self, d: &Desired) -> Result<bool> {
        self.dev.set_led_counts(d.led_counts[0].max(1), d.led_counts[1].max(1))
    }

    /// 8 x all-zero record, `[CC 28 FF]`, 100 ms, one `0x20` record with
    /// `Zone_Sel0 = 0xFF`, `[CC 28 01]`.
    fn apply_whole_wave(&mut self, w: HwEffect, d: &Desired) -> Result<()> {
        if self.board == Board::WholeWave(w) {
            self.counts(d)?;
            return Ok(());
        }
        self.dev.set_direct_bits(0)?;
        self.counts(d)?;
        self.dev.clear_all_registers()?;
        self.dev.sleep(SETTLE);
        let zones = self.dev.all_zones();
        self.dev.set_effect(0, &w, zones)?;
        self.dev.apply(0x01)?;
        self.hw_writes += 1;
        self.strips = [None, None];
        self.slots.clear();
        self.board = Board::WholeWave(w);
        Ok(())
    }

    /// Whole board in one colour, as GIGABYTE's software does it: direct off and registers cleared once, then per colour change one
    /// Static record on `0x20` for all zones + `[CC 28 FF]`.
    fn apply_sync_all(&mut self, c: [u8; 3], d: &Desired, now: Instant) -> Result<()> {
        if !matches!(self.board, Board::SyncAll(_)) {
            self.dev.set_direct_bits(0)?;
            self.dev.clear_all_registers()?;
            self.dev.sleep(SYNC_SETTLE);
            self.strips = [None, None];
            self.slots.clear();
            self.board = Board::SyncAll(None);
        }
        self.counts(d)?;
        let due = self.sync_at.is_none_or(|t| now.duration_since(t) >= KEEPALIVE);
        if self.board != Board::SyncAll(Some(c)) || due {
            let zones = self.dev.all_zones();
            self.dev.set_effect(0, &HwEffect::solid(c), zones)?;
            self.dev.apply_all()?;
            self.board = Board::SyncAll(Some(c));
            self.sync_at = Some(now);
        }
        Ok(())
    }

    fn apply_normal(&mut self, d: &Desired, now: Instant) -> Result<()> {
        if self.board != Board::Normal {
            // Leaving a board-wide mode: release every register first.
            self.dev.clear_all_registers()?;
            self.dev.sleep(SETTLE);
            self.reset_cache();
        }
        let dled = [self.dev.dled_slot(0), self.dev.dled_slot(1)];

        // 1. Direct-mode enter/leave: neutralise the header's built-in effect before 0x32.
        let mut bits = self.dev.direct_bits();
        let mut entering = 0u32;
        for (h, header) in d.headers.iter().enumerate() {
            let bit = 1u8 << h;
            match header {
                HeaderOut::Direct(_) if bits & bit == 0 => {
                    self.dev.clear_slot(dled[h])?;
                    self.slots.remove(&dled[h]);
                    entering |= 1 << dled[h];
                    bits |= bit;
                }
                HeaderOut::Hw(_) if bits & bit != 0 => {
                    bits &= !bit;
                    self.strips[h] = None;
                }
                _ => {}
            }
        }
        if entering != 0 {
            self.dev.apply(entering)?;
            self.dev.sleep(SETTLE);
        }
        let bits_changed = self.dev.set_direct_bits(bits)?;
        // 2. LED count before any data; GCC always follows 0x32 with 0x34.
        if bits_changed {
            self.dev.force_led_counts();
        }
        let counts_changed = self.counts(d)?;

        // 3. Streamed frames.
        for h in 0..2 {
            let HeaderOut::Direct(frame) = &d.headers[h] else { continue };
            let order = self.order_for(h, d);
            let due = match &self.strips[h] {
                None => true,
                Some(s) => s.frame != *frame || s.order != order || now.duration_since(s.at) >= KEEPALIVE,
            };
            if due || bits_changed || counts_changed {
                self.dev.send_strip(h as u8, frame, order)?;
                self.strips[h] = Some(StripSent { frame: frame.clone(), order, at: now });
            }
        }

        // 4. Effect records, one apply for all of them.
        let mut mask = 0u32;
        let mut records: Vec<(u8, HwEffect, bool)> = Vec::new();
        for (h, header) in d.headers.iter().enumerate() {
            if let HeaderOut::Hw(e) = header {
                records.push((dled[h], self.ordered_record(h, d, *e), false));
            }
        }
        for (slot, out) in &d.singles {
            if dled.contains(slot) {
                continue; // IT8950: ARGB headers live on divisions 2/3
            }
            match out {
                SingleOut::Soft(c) => records.push((*slot, HwEffect::solid(*c), true)),
                SingleOut::Hw(e) => records.push((*slot, *e, false)),
            }
        }
        for (slot, eff, soft) in records {
            let due = match self.slots.get(&slot) {
                None => true,
                Some(s) => s.eff != eff || (soft && now.duration_since(s.at) >= KEEPALIVE),
            };
            if due {
                self.dev.set_effect(slot, &eff, 1 << slot)?;
                self.slots.insert(slot, SlotSent { eff, at: now });
                mask |= 1 << slot;
                if !soft {
                    self.hw_writes += 1;
                }
            }
        }
        if mask != 0 {
            self.dev.apply(mask)?;
        }
        Ok(())
    }

    /// Exit / suspend: switch to lighting that survives without the app, then `[CC 47 01]`
    /// (as GIGABYTE's software does at exit). Returns whether the firmware supports 0x47.
    pub fn persist(&mut self, plan: &Desired, now: Instant) -> Result<bool> {
        self.apply(plan, now)?;
        self.dev.persist(true)
    }
}

/// What to leave on the controller when FanRGB stops driving it.
pub fn persist_plan(d: &Desired, policy: ExitLighting, cycle_brightness: u8) -> Desired {
    let mut p = d.clone();
    let cycle = HwEffect::new(HwMode::ColorCycle, [255, 255, 255], cycle_brightness, 4);
    match policy {
        ExitLighting::Off => {
            p.whole_wave = None;
            p.sync_all = None;
            for h in p.headers.iter_mut() {
                if *h != HeaderOut::Unused {
                    *h = HeaderOut::Hw(HwEffect::off());
                }
            }
            for (_, o) in p.singles.iter_mut() {
                *o = SingleOut::Hw(HwEffect::off());
            }
        }
        ExitLighting::HardwareCycle => {
            if p.whole_wave.is_some() {
                return p; // already a hardware effect
            }
            p.sync_all = None;
            for h in p.headers.iter_mut() {
                if matches!(h, HeaderOut::Direct(_)) {
                    *h = HeaderOut::Hw(cycle);
                }
            }
            for (_, o) in p.singles.iter_mut() {
                if matches!(o, SingleOut::Soft(_)) {
                    *o = SingleOut::Hw(cycle);
                }
            }
        }
        ExitLighting::KeepLast => {
            if p.whole_wave.is_some() || p.sync_all.is_some() {
                return p; // both are hardware registers already
            }
            // A uniform software frame becomes a hardware Static that also survives sleep;
            // a moving pattern stays in direct mode (the last frame remains).
            for h in p.headers.iter_mut() {
                if let HeaderOut::Direct(f) = h
                    && let Some(c) = uniform(f)
                {
                    *h = HeaderOut::Hw(HwEffect::solid(c));
                }
            }
            for (_, o) in p.singles.iter_mut() {
                if let SingleOut::Soft(c) = o {
                    *o = SingleOut::Hw(HwEffect::solid(*c));
                }
            }
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rgb::fusion2::{Fusion2, effect_packet, strip_packets};
    use crate::rgb::mock::{MockDev, Op, pkt};

    fn session(flags: u8) -> (MockDev, Session) {
        let m = MockDev::new(flags);
        let mut f = Fusion2::connect(m.transport(), 0x5702).unwrap();
        f.init().unwrap();
        m.clear_log();
        (m, Session::new(f))
    }

    fn eff_op(slot: u8, e: HwEffect, zones: u32) -> Op {
        Op::Send(effect_packet(slot, &e, zones).to_vec())
    }

    fn strip_ops(h: u8, f: &[[u8; 3]]) -> Vec<Op> {
        strip_packets(h, f, ORDER_GRB).into_iter().map(|p| Op::Send(p.to_vec())).collect()
    }

    fn frame(n: usize, c: [u8; 3]) -> Vec<[u8; 3]> {
        vec![c; n]
    }

    #[test]
    fn entering_direct_mode_follows_gcc_order() {
        let (m, mut s) = session(0x03);
        let f: Vec<[u8; 3]> = (0..20u8).map(|i| [i, 0, 255 - i]).collect();
        let d = Desired {
            led_counts: [20, 30],
            headers: [HeaderOut::Direct(f.clone()), HeaderOut::Unused],
            ..Default::default()
        };
        let t0 = Instant::now();
        s.apply(&d, t0).unwrap();
        let mut want = vec![
            Op::Send(pkt(&[0x25])),                // clear D_LED1 register (all-zero record)
            Op::Send(pkt(&[0x28, 0x20, 0, 0, 0])), // apply bit 0x20
            Op::Sleep(100),
            Op::Send(pkt(&[0x32, 0x01])), // built-in effect off -> direct
            Op::Send(pkt(&[0x34, 0x00])), // LED count before data
        ];
        want.extend(strip_ops(0, &f)); // 19 + 1 LEDs
        assert_eq!(m.log(), want);
        assert_eq!(strip_ops(0, &f).len(), 2);

        // Unchanged frame: nothing is sent ...
        m.clear_log();
        s.apply(&d, t0 + Duration::from_millis(500)).unwrap();
        assert!(m.log().is_empty());
        // ... until the 1 s keep-alive.
        s.apply(&d, t0 + Duration::from_millis(1100)).unwrap();
        assert_eq!(m.log(), strip_ops(0, &f));

        // More LEDs: 0x34 first, then the data.
        m.clear_log();
        let f2 = frame(100, [1, 2, 3]);
        let d2 = Desired {
            led_counts: [100, 30],
            headers: [HeaderOut::Direct(f2.clone()), HeaderOut::Unused],
            ..Default::default()
        };
        s.apply(&d2, t0 + Duration::from_millis(1200)).unwrap();
        let mut want = vec![Op::Send(pkt(&[0x34, 0x02]))];
        want.extend(strip_ops(0, &f2));
        assert_eq!(m.log(), want);
    }

    #[test]
    fn user_colour_order_overrides_calibration() {
        let (m, mut s) = session(0x03);
        let d = Desired {
            led_counts: [1, 1],
            headers: [HeaderOut::Unused, HeaderOut::Direct(vec![[10, 20, 30]])],
            order_override: [None, Some([0, 1, 2])],
            ..Default::default()
        };
        s.apply(&d, Instant::now()).unwrap();
        let last = m.sent().pop().unwrap();
        assert_eq!(&last[..8], &[0xCC, 0x59, 0, 0, 3, 10, 20, 30]);
    }

    /// Hardware records honour the override too. The mock's calibration
    /// is GRB; for an RGB strip red must be handed to the firmware as "green".
    #[test]
    fn user_colour_order_applies_to_hardware_records() {
        assert_eq!(remap_color([1, 2, 3], ORDER_GRB, ORDER_GRB), [1, 2, 3]);
        assert_eq!(remap_color([255, 0, 0], ORDER_GRB, [0, 1, 2]), [0, 255, 0]);
        // Every permutation pair: the firmware's layout of the remapped colour is the wanted layout.
        let perms = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];
        for cal in perms {
            for want in perms {
                let c = [10, 20, 30];
                let r = remap_color(c, cal, want);
                let (mut fw, mut wire) = ([0u8; 3], [0u8; 3]);
                for k in 0..3 {
                    fw[cal[k]] = r[k];
                    wire[want[k]] = c[k];
                }
                assert_eq!(fw, wire, "cal {cal:?} want {want:?}");
            }
        }
        let (m, mut s) = session(0x03);
        let st = HwEffect::new(HwMode::Static, [255, 0, 0], 255, 0);
        let d = Desired {
            led_counts: [1, 1],
            headers: [HeaderOut::Hw(st), HeaderOut::Hw(st)],
            order_override: [Some([0, 1, 2]), None],
            ..Default::default()
        };
        s.apply(&d, Instant::now()).unwrap();
        let sent = m.sent();
        assert!(sent.contains(&effect_packet(5, &HwEffect { color: [0, 255, 0], ..st }, 0x20).to_vec()), "{sent:?}");
        assert!(sent.contains(&effect_packet(6, &st, 0x40).to_vec()), "no override: firmware order");
    }

    #[test]
    fn hardware_effects_and_led_c_both_slots_share_one_apply() {
        let (m, mut s) = session(0x03);
        let wave = HwEffect::new(HwMode::Wave, [0, 0, 0], 200, 7);
        let cyc = HwEffect::new(HwMode::ColorCycle, [255, 255, 255], 200, 7);
        let d = Desired {
            led_counts: [60, 32],
            headers: [HeaderOut::Hw(wave), HeaderOut::Unused],
            singles: vec![(1, SingleOut::Hw(cyc)), (4, SingleOut::Hw(cyc))],
            ..Default::default()
        };
        s.apply(&d, Instant::now()).unwrap();
        assert_eq!(
            m.log(),
            vec![
                Op::Send(pkt(&[0x34, 0x01])),
                eff_op(5, wave, 0x20), // M1: wave stays a wave on the ARGB header
                eff_op(1, cyc, 0x02),
                eff_op(4, cyc, 0x10),
                Op::Send(pkt(&[0x28, 0x32, 0, 0, 0])),
            ]
        );
        m.clear_log();
        s.apply(&d, Instant::now() + Duration::from_secs(5)).unwrap();
        assert!(m.log().is_empty(), "hardware effects are not re-sent (it would restart them)");
    }

    #[test]
    fn leaving_direct_mode_for_a_hardware_effect() {
        let (m, mut s) = session(0x03);
        let t0 = Instant::now();
        let d = Desired {
            led_counts: [10, 10],
            headers: [HeaderOut::Direct(frame(10, [9, 9, 9])), HeaderOut::Unused],
            ..Default::default()
        };
        s.apply(&d, t0).unwrap();
        m.clear_log();
        let st = HwEffect::new(HwMode::Static, [255, 0, 0], 255, 0);
        let d = Desired { led_counts: [10, 10], headers: [HeaderOut::Hw(st), HeaderOut::Unused], ..Default::default() };
        s.apply(&d, t0).unwrap();
        assert_eq!(
            m.log(),
            vec![
                Op::Send(pkt(&[0x32, 0x00])),
                Op::Send(pkt(&[0x34, 0x00])),
                eff_op(5, st, 0x20),
                Op::Send(pkt(&[0x28, 0x20, 0, 0, 0])),
            ]
        );
    }

    #[test]
    fn software_colours_on_12v_zones_are_static_records_with_keepalive() {
        let (m, mut s) = session(0x03);
        let t0 = Instant::now();
        let d = Desired {
            led_counts: [1, 1],
            singles: vec![(1, SingleOut::Soft([1, 2, 3])), (4, SingleOut::Soft([1, 2, 3]))],
            ..Default::default()
        };
        s.apply(&d, t0).unwrap();
        let solid = HwEffect::solid([1, 2, 3]);
        let records = vec![eff_op(1, solid, 0x02), eff_op(4, solid, 0x10), Op::Send(pkt(&[0x28, 0x12, 0, 0, 0]))];
        assert_eq!(m.log(), records, "LED count unchanged since init: no 0x34");
        assert_eq!(&m.sent()[0][11..18], &[1, 255, 0, 3, 2, 1, 0]);
        m.clear_log();
        s.apply(&d, t0 + Duration::from_millis(300)).unwrap();
        assert!(m.log().is_empty());
        s.apply(&d, t0 + Duration::from_millis(1300)).unwrap();
        assert_eq!(m.log(), records);
    }

    #[test]
    fn sync_all_uses_one_ff_record_and_apply() {
        let (m, mut s) = session(0x03);
        let t0 = Instant::now();
        let mut d = Desired {
            led_counts: [30, 30],
            headers: [HeaderOut::Direct(frame(30, [7, 8, 9])), HeaderOut::Direct(frame(30, [7, 8, 9]))],
            singles: vec![(1, SingleOut::Soft([7, 8, 9])), (4, SingleOut::Soft([7, 8, 9]))],
            sync_all: Some([7, 8, 9]),
            ..Default::default()
        };
        assert_eq!(d.packets_per_frame(), 2);
        s.apply(&d, t0).unwrap();
        let mut want = vec![];
        for reg in 0x20..=0x27 {
            want.push(Op::Send(pkt(&[reg])));
        }
        want.push(Op::Send(pkt(&[0x28, 0xFF, 0])));
        want.push(Op::Sleep(25));
        want.push(Op::Send(pkt(&[0x20, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0xFF, 0x00, 9, 8, 7, 0])));
        want.push(Op::Send(pkt(&[0x28, 0xFF, 0])));
        assert_eq!(m.log(), want);
        // Colour change: exactly 2 packets.
        m.clear_log();
        d.sync_all = Some([1, 1, 1]);
        s.apply(&d, t0 + Duration::from_millis(25)).unwrap();
        assert_eq!(m.sent().len(), 2);
        assert_eq!(m.sent()[0][1], 0x20);
        // Same colour: keep-alive only after 1 s.
        m.clear_log();
        s.apply(&d, t0 + Duration::from_millis(500)).unwrap();
        assert!(m.log().is_empty());
        s.apply(&d, t0 + Duration::from_millis(1100)).unwrap();
        assert_eq!(m.sent().len(), 2);
        // Back to per-zone output: registers are released first.
        m.clear_log();
        d.sync_all = None;
        s.apply(&d, t0 + Duration::from_millis(1200)).unwrap();
        let log = m.log();
        assert_eq!(log[0], Op::Send(pkt(&[0x20])));
        assert_eq!(log[9], Op::Sleep(100));
        assert!(log.contains(&Op::Send(pkt(&[0x32, 0x03]))));
    }

    #[test]
    fn whole_board_wave_matches_rgb_re_m2() {
        let (m, mut s) = session(0x03);
        let w = HwEffect::new(HwMode::Wave, [0, 0, 0], 0xB4, 9);
        let d = Desired { led_counts: [32, 32], whole_wave: Some(w), ..Default::default() };
        s.apply(&d, Instant::now()).unwrap();
        let mut want = vec![];
        for reg in 0x20..=0x27 {
            want.push(Op::Send(pkt(&[reg])));
        }
        want.push(Op::Send(pkt(&[0x28, 0xFF, 0])));
        want.push(Op::Sleep(100));
        let mut rec = vec![0x20, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0x06, 0xB4, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        rec.extend_from_slice(&[30, 0, 0, 0, 0, 0, 0, 0, 0x07, 0x01]);
        want.push(Op::Send(pkt(&rec)));
        want.push(Op::Send(pkt(&[0x28, 0x01])));
        assert_eq!(m.log(), want);
        m.clear_log();
        s.apply(&d, Instant::now()).unwrap();
        assert!(m.log().is_empty());
    }

    #[test]
    fn persist_plans() {
        let soft = Desired {
            led_counts: [30, 30],
            headers: [HeaderOut::Direct(frame(3, [5, 5, 5])), HeaderOut::Direct(vec![[1, 0, 0], [0, 1, 0]])],
            singles: vec![(1, SingleOut::Soft([5, 5, 5])), (4, SingleOut::Hw(HwEffect::off()))],
            ..Default::default()
        };
        let cyc = HwEffect::new(HwMode::ColorCycle, [255, 255, 255], 200, 4);
        let p = persist_plan(&soft, ExitLighting::HardwareCycle, 200);
        assert_eq!(p.headers, [HeaderOut::Hw(cyc), HeaderOut::Hw(cyc)]);
        assert_eq!(p.singles, vec![(1, SingleOut::Hw(cyc)), (4, SingleOut::Hw(HwEffect::off()))]);
        let p = persist_plan(&soft, ExitLighting::Off, 200);
        assert_eq!(p.headers, [HeaderOut::Hw(HwEffect::off()), HeaderOut::Hw(HwEffect::off())]);
        let p = persist_plan(&soft, ExitLighting::KeepLast, 200);
        assert_eq!(p.headers[0], HeaderOut::Hw(HwEffect::solid([5, 5, 5])));
        assert!(matches!(p.headers[1], HeaderOut::Direct(_)), "a moving pattern stays in direct mode");
        assert_eq!(p.singles[0], (1, SingleOut::Hw(HwEffect::solid([5, 5, 5]))));
        let mut sync = soft.clone();
        sync.sync_all = Some([5, 5, 5]);
        assert_eq!(persist_plan(&sync, ExitLighting::KeepLast, 200), sync);
        assert_eq!(persist_plan(&sync, ExitLighting::HardwareCycle, 200).sync_all, None);
    }

    #[test]
    fn persist_sends_0x47_01_after_the_hardware_effects() {
        let (m, mut s) = session(0x03);
        let t0 = Instant::now();
        let d = Desired {
            led_counts: [10, 10],
            headers: [HeaderOut::Direct(frame(10, [1, 2, 3])), HeaderOut::Unused],
            ..Default::default()
        };
        s.apply(&d, t0).unwrap();
        m.clear_log();
        let plan = persist_plan(&d, ExitLighting::HardwareCycle, 255);
        assert!(s.persist(&plan, t0).unwrap());
        let sent = m.sent();
        assert_eq!(sent[0], pkt(&[0x32, 0x00]), "direct mode off first");
        assert_eq!(sent[sent.len() - 2], pkt(&[0x28, 0x20, 0, 0, 0]));
        assert_eq!(sent[sent.len() - 1], pkt(&[0x47, 0x01]), "keep LED settings last");
        assert!(!sent.iter().any(|p| p[1] == 0x5E), "no flash write (F9)");
    }

    #[test]
    fn resume_reinitialises_and_resends_everything() {
        let (m, mut s) = session(0x03);
        let t0 = Instant::now();
        let st = HwEffect::new(HwMode::Pulse, [0, 0, 255], 255, 3);
        let d = Desired {
            led_counts: [10, 10],
            headers: [HeaderOut::Direct(frame(10, [1, 2, 3])), HeaderOut::Hw(st)],
            singles: vec![(1, SingleOut::Hw(st))],
            ..Default::default()
        };
        s.apply(&d, t0).unwrap();
        m.clear_log();
        s.reinit().unwrap();
        s.apply(&d, t0).unwrap();
        let sent = m.sent();
        let pos = |p: &Vec<u8>| sent.iter().position(|x| x == p).unwrap();
        assert_eq!(sent[0], pkt(&[0x60, 0]), "re-reads the device info");
        assert!(pos(&pkt(&[0x47, 0x00])) < pos(&pkt(&[0x32, 0x01])), "47 00 before direct mode");
        assert!(sent.contains(&effect_packet(6, &st, 0x40).to_vec()), "hardware effects re-sent");
        assert!(sent.contains(&effect_packet(1, &st, 0x02).to_vec()));
        assert!(sent.iter().any(|p| p[1] == 0x58), "strip re-sent");
    }

    #[test]
    fn write_errors_propagate_with_codes() {
        let (m, mut s) = session(0x03);
        m.fail_next_send(1167);
        let d = Desired {
            led_counts: [10, 10],
            headers: [HeaderOut::Direct(frame(10, [1, 2, 3])), HeaderOut::Unused],
            ..Default::default()
        };
        let e = s.apply(&d, Instant::now()).unwrap_err();
        assert_eq!(crate::error::code_of(&e), crate::error::ErrorCode::RgbDisconnected);
    }

    #[test]
    fn packet_budget() {
        let d = Desired {
            headers: [HeaderOut::Direct(frame(1024, [0; 3])), HeaderOut::Direct(frame(1024, [0; 3]))],
            singles: vec![(1, SingleOut::Soft([0; 3]))],
            ..Default::default()
        };
        assert_eq!(d.packets_per_frame(), 54 * 2 + 2);
        assert!(d.applies_per_frame());
    }
}
