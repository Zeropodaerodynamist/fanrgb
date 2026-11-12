//! Physical header layout of supported boards, in board units (PCB = 100 × 92).
//!
//! Gigabyte B650M D3HP AX (rev 1.0/1.2/1.3): 1 CPU fan + 2 system fan headers, 2 ARGB (5 V, 3-pin)
//! and 1 RGB (12 V, 4-pin) LED headers. Header *coordinates* are approximations of the silkscreen
//! layout; verify against the rev 1.3 manual's "Motherboard Layout" page (one-line fixes below).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderKind {
    Fan,
    /// Addressable 5 V, 3-pin.
    Argb,
    /// 12 V RGB, 4-pin.
    Rgb12,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug)]
pub struct HeaderPos {
    /// Fan channel id ("mb.pwm0") or RGB zone id ("dled1").
    pub id: &'static str,
    pub label: &'static str,
    pub kind: HeaderKind,
    pub at: (f32, f32),
    pub label_side: Side,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landmark {
    Io,
    Socket,
    Dimm,
    Pcie,
    M2,
    Chipset,
    Power,
    Audio,
    Heatsink,
}

#[derive(Clone, Copy, Debug)]
pub struct Feature {
    pub kind: Landmark,
    pub label: &'static str,
    /// x0, y0, x1, y1
    pub rect: (f32, f32, f32, f32),
}

pub struct BoardMap {
    pub name: &'static str,
    pub width: f32,
    pub height: f32,
    pub headers: &'static [HeaderPos],
    /// Channels the Super I/O exposes but this board does not route (shown only with "show all").
    pub ghosts: &'static [HeaderPos],
    pub features: &'static [Feature],
}

const fn h(id: &'static str, label: &'static str, kind: HeaderKind, x: f32, y: f32, label_side: Side) -> HeaderPos {
    HeaderPos { id, label, kind, at: (x, y), label_side }
}

const fn f(kind: Landmark, label: &'static str, x0: f32, y0: f32, x1: f32, y1: f32) -> Feature {
    Feature { kind, label, rect: (x0, y0, x1, y1) }
}

pub const B650M_D3HP: BoardMap = BoardMap {
    name: "B650M D3HP AX",
    width: 100.0,
    height: 92.0,
    headers: &[
        h("mb.pwm0", "CPU_FAN", HeaderKind::Fan, 55.0, 6.0, Side::Right),
        h("mb.pwm1", "SYS_FAN1", HeaderKind::Fan, 91.0, 55.0, Side::Left),
        h("mb.pwm2", "SYS_FAN2", HeaderKind::Fan, 91.0, 73.0, Side::Left),
        h("ledc", "LED_C", HeaderKind::Rgb12, 14.0, 87.0, Side::Right),
        h("dled2", "D_LED2", HeaderKind::Argb, 38.0, 87.0, Side::Right),
        h("dled1", "D_LED1", HeaderKind::Argb, 62.0, 87.0, Side::Right),
    ],
    ghosts: &[
        h("mb.pwm3", "SYS_FAN3", HeaderKind::Fan, 0.0, 0.0, Side::Right),
        h("mb.pwm4", "SYS_FAN4/PUMP", HeaderKind::Fan, 0.0, 0.0, Side::Right),
        h("mb.pwm5", "CPU_OPT", HeaderKind::Fan, 0.0, 0.0, Side::Right),
    ],
    features: &[
        f(Landmark::Io, "I/O", 1.0, 5.0, 11.0, 44.0),
        f(Landmark::Power, "8-pin", 14.0, 1.0, 25.0, 5.0),
        f(Landmark::Heatsink, "", 13.0, 9.0, 21.0, 40.0),
        f(Landmark::Heatsink, "", 24.0, 8.0, 50.0, 13.0),
        f(Landmark::Socket, "AM5", 26.0, 16.0, 52.0, 42.0),
        f(Landmark::Dimm, "", 61.0, 9.0, 63.0, 52.0),
        f(Landmark::Dimm, "", 64.5, 9.0, 66.5, 52.0),
        f(Landmark::Dimm, "", 68.0, 9.0, 70.0, 52.0),
        f(Landmark::Dimm, "", 71.5, 9.0, 73.5, 52.0),
        f(Landmark::Power, "24-pin", 94.0, 18.0, 99.0, 46.0),
        f(Landmark::M2, "M.2", 22.0, 48.0, 56.0, 51.0),
        f(Landmark::Pcie, "PCIe x16", 12.0, 58.0, 78.0, 62.0),
        f(Landmark::Chipset, "B650", 46.0, 64.0, 57.0, 75.0),
        f(Landmark::Pcie, "PCIe x1", 12.0, 72.0, 40.0, 75.0),
        f(Landmark::Audio, "Audio", 1.0, 78.0, 10.0, 91.0),
    ],
};

/// Board map for `board` (product string), or `None` for the generic header strip.
pub fn for_board(board: &str, simulated: bool) -> Option<&'static BoardMap> {
    let b = board.to_ascii_uppercase();
    if b.contains("B650M D3HP") || b.contains("B650M-D3HP") || simulated { Some(&B650M_D3HP) } else { None }
}

/// Silkscreen label for a fan channel id.
pub fn fan_label(id: &str) -> Option<&'static str> {
    B650M_D3HP.headers.iter().chain(B650M_D3HP.ghosts.iter()).find(|h| h.id == id).map(|h| h.label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d3hp_routes_three_fans_and_three_led_headers() {
        let m = &B650M_D3HP;
        assert_eq!(m.headers.iter().filter(|h| h.kind == HeaderKind::Fan).count(), 3);
        assert_eq!(m.headers.iter().filter(|h| h.kind == HeaderKind::Argb).count(), 2);
        assert_eq!(m.headers.iter().filter(|h| h.kind == HeaderKind::Rgb12).count(), 1);
        assert!(m.ghosts.iter().all(|g| !m.headers.iter().any(|h| h.id == g.id)));
        for h in m.headers {
            assert!(h.at.0 >= 0.0 && h.at.0 <= m.width && h.at.1 >= 0.0 && h.at.1 <= m.height, "{}", h.label);
        }
        assert!(for_board("B650M D3HP AX", false).is_some());
        assert!(for_board("X670E AORUS MASTER", false).is_none());
    }
}
