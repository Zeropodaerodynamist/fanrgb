//! Painter-drawn vector icons on a 20 pt grid (2 pt live-area padding), 1.5 pt strokes with round caps.

use std::f32::consts::{PI, TAU};

use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, pos2, vec2};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
// A complete glyph set (design system); not every glyph is used by every page.
#[allow(dead_code)]
pub enum Icon {
    // navigation
    Overview,
    Cooling,
    Lighting,
    Intelligence,
    Sensors,
    Diagnostics,
    Settings,
    // status
    Info,
    Warning,
    Error,
    Critical,
    Check,
    CheckCircle,
    Lock,
    Usb,
    Chip,
    Plug,
    Shield,
    // actions
    Plus,
    Minus,
    Close,
    ChevronDown,
    ChevronRight,
    ChevronLeft,
    ChevronUp,
    More,
    Copy,
    ExternalLink,
    Refresh,
    Undo,
    Redo,
    Search,
    Pencil,
    Trash,
    Play,
    Pause,
    Eye,
    Power,
    Keyboard,
    Sun,
    Moon,
    Drag,
    Folder,
    Report,
    Bolt,
    Board,
    Led,
    Dot,
    Download,
    /// Arrow into a tray (app update).
    Update,
    /// Silent mode.
    Leaf,
    /// Balanced mode (scales).
    Balance,
    /// Full speed (gauge at max).
    Gauge,
    /// Auto (AI): a line running into forecast dashes.
    Forecast,
    /// BIOS (board with chip).
    Bios,
    Wifi,
    Bluetooth,
    Lan,
    Audio,
    Gpu,
    /// Four squares (Windows settings).
    Windows,
    Restart,
    Pipette,
}

struct G<'a> {
    p: &'a Painter,
    o: Pos2,
    s: f32,
    stroke: Stroke,
    color: Color32,
}

impl G<'_> {
    fn pt(&self, x: f32, y: f32) -> Pos2 {
        self.o + vec2(x * self.s, y * self.s)
    }
    fn caps(&self, a: Pos2, b: Pos2) {
        let r = self.stroke.width * 0.5;
        if r >= 0.7 {
            self.p.circle_filled(a, r, self.color);
            self.p.circle_filled(b, r, self.color);
        }
    }
    /// Open polyline with round caps.
    fn line(&self, pts: &[(f32, f32)]) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        if v.len() < 2 {
            return;
        }
        self.caps(v[0], v[v.len() - 1]);
        self.p.add(Shape::line(v, self.stroke));
    }
    fn closed(&self, pts: &[(f32, f32)]) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        self.p.add(Shape::closed_line(v, self.stroke));
    }
    fn filled(&self, pts: &[(f32, f32)]) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        self.p.add(Shape::convex_polygon(v, self.color, Stroke::NONE));
    }
    fn circle(&self, x: f32, y: f32, r: f32) {
        self.p.circle_stroke(self.pt(x, y), r * self.s, self.stroke);
    }
    fn dot(&self, x: f32, y: f32, r: f32) {
        self.p.circle_filled(self.pt(x, y), r * self.s, self.color);
    }
    fn rect(&self, x0: f32, y0: f32, x1: f32, y1: f32, rad: f32) {
        let r = Rect::from_min_max(self.pt(x0, y0), self.pt(x1, y1));
        self.p.rect_stroke(r, rad * self.s, self.stroke, egui::StrokeKind::Middle);
    }
    fn arc_pts(&self, cx: f32, cy: f32, r: f32, a0: f32, a1: f32) -> Vec<Pos2> {
        let n = (((a1 - a0).abs() / 0.12).ceil() as usize).max(4);
        (0..=n)
            .map(|i| {
                let a = a0 + (a1 - a0) * i as f32 / n as f32;
                self.pt(cx + a.cos() * r, cy + a.sin() * r)
            })
            .collect()
    }
    fn arc(&self, cx: f32, cy: f32, r: f32, a0: f32, a1: f32) {
        let v = self.arc_pts(cx, cy, r, a0, a1);
        self.caps(v[0], v[v.len() - 1]);
        self.p.add(Shape::line(v, self.stroke));
    }
}

fn polar(c: (f32, f32), r: f32, a: f32) -> (f32, f32) {
    (c.0 + a.cos() * r, c.1 + a.sin() * r)
}

fn quad(p0: (f32, f32), c: (f32, f32), p1: (f32, f32), n: usize, out: &mut Vec<(f32, f32)>) {
    for i in 0..=n {
        let t = i as f32 / n as f32;
        let u = 1.0 - t;
        out.push((u * u * p0.0 + 2.0 * u * t * c.0 + t * t * p1.0, u * u * p0.1 + 2.0 * u * t * c.1 + t * t * p1.1));
    }
}

fn octagon(c: (f32, f32), r: f32) -> Vec<(f32, f32)> {
    (0..8).map(|k| polar(c, r, PI / 8.0 + k as f32 * TAU / 8.0)).collect()
}

/// Paints `icon` centered in `rect` (square area used).
pub fn paint(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let side = rect.width().min(rect.height());
    let o = rect.center() - vec2(side, side) * 0.5;
    let s = side / 20.0;
    let w = if side >= 24.0 { 1.75 * s } else { (1.5 * s).max(1.15) };
    let g = G { p, o, s, stroke: Stroke::new(w, color), color };
    match icon {
        Icon::Overview => {
            for (x, y) in [(3.0, 3.0), (11.0, 3.0), (3.0, 11.0), (11.0, 11.0)] {
                g.rect(x, y, x + 6.0, y + 6.0, 1.6);
            }
        }
        Icon::Cooling => fan(&g, 0.0),
        Icon::Lighting => {
            let pts = g.arc_pts(10.0, 8.5, 4.6, 0.75 * PI, 2.25 * PI);
            let mut v: Vec<Pos2> = vec![g.pt(8.0, 14.0)];
            v.extend(pts);
            v.push(g.pt(12.0, 14.0));
            g.p.add(Shape::line(v, g.stroke));
            g.line(&[(8.0, 14.0), (12.0, 14.0)]);
            g.line(&[(8.5, 16.8), (11.5, 16.8)]);
            for a in [-PI / 2.0, -PI / 2.0 - 0.95, -PI / 2.0 + 0.95] {
                let a0 = polar((10.0, 8.5), 6.6, a);
                let a1 = polar((10.0, 8.5), 8.0, a);
                g.line(&[a0, a1]);
            }
        }
        Icon::Intelligence => {
            g.line(&[(3.0, 3.0), (3.0, 17.0), (17.0, 17.0)]);
            g.line(&[(5.5, 14.0), (8.5, 10.0), (11.0, 12.0)]);
            let d = [(11.0, 12.0), (14.0, 7.5), (17.0, 5.0)];
            let v: Vec<Pos2> = d.iter().map(|&(x, y)| g.pt(x, y)).collect();
            g.p.extend(Shape::dashed_line(&v, Stroke::new(w, color), 2.0 * s, 1.8 * s));
            g.dot(11.0, 12.0, 1.5);
        }
        Icon::Sensors => {
            g.line(&[(8.5, 11.8), (8.5, 4.5)]);
            g.line(&[(11.5, 11.8), (11.5, 4.5)]);
            g.arc(10.0, 4.5, 1.5, PI, TAU);
            g.arc(10.0, 14.3, 3.2, -PI / 2.0 + 0.5, 1.5 * PI - 0.5);
            g.dot(10.0, 14.3, 1.5);
            g.line(&[(10.0, 8.0), (10.0, 13.0)]);
            g.line(&[(14.0, 5.0), (16.0, 5.0)]);
            g.line(&[(14.0, 8.0), (16.0, 8.0)]);
        }
        Icon::Diagnostics => {
            g.line(&[(2.5, 10.5), (6.0, 10.5), (8.0, 5.5), (11.0, 15.5), (13.2, 10.5), (17.5, 10.5)]);
        }
        Icon::Settings => {
            let c = (10.0, 10.0);
            let mut pts = Vec::new();
            for k in 0..8 {
                let a = k as f32 * TAU / 8.0;
                pts.push(polar(c, 6.2, a - 0.30));
                pts.push(polar(c, 8.2, a - 0.17));
                pts.push(polar(c, 8.2, a + 0.17));
                pts.push(polar(c, 6.2, a + 0.30));
            }
            g.closed(&pts);
            g.circle(10.0, 10.0, 2.6);
        }
        Icon::Info => {
            g.circle(10.0, 10.0, 8.0);
            g.dot(10.0, 6.4, 1.1);
            g.line(&[(10.0, 9.2), (10.0, 14.2)]);
        }
        Icon::Warning => {
            g.closed(&[(10.0, 2.6), (18.0, 16.6), (2.0, 16.6)]);
            g.line(&[(10.0, 7.6), (10.0, 11.4)]);
            g.dot(10.0, 14.0, 1.1);
        }
        Icon::Error => {
            g.closed(&octagon((10.0, 10.0), 8.2));
            g.line(&[(7.2, 7.2), (12.8, 12.8)]);
            g.line(&[(12.8, 7.2), (7.2, 12.8)]);
        }
        Icon::Critical => {
            g.filled(&octagon((10.0, 10.0), 8.6));
            let mark = Stroke::new(w * 1.2, Color32::WHITE);
            g.p.line_segment([g.pt(10.0, 5.6), g.pt(10.0, 11.0)], mark);
            g.p.circle_filled(g.pt(10.0, 5.6), mark.width * 0.5, Color32::WHITE);
            g.p.circle_filled(g.pt(10.0, 11.0), mark.width * 0.5, Color32::WHITE);
            g.p.circle_filled(g.pt(10.0, 14.2), 1.3 * s, Color32::WHITE);
        }
        Icon::Check => g.line(&[(4.0, 10.5), (8.3, 14.8), (16.2, 5.6)]),
        Icon::CheckCircle => {
            g.circle(10.0, 10.0, 8.0);
            g.line(&[(6.3, 10.3), (9.0, 13.0), (13.9, 7.4)]);
        }
        Icon::Lock => {
            g.rect(4.5, 9.0, 15.5, 17.0, 2.0);
            g.line(&[(6.7, 9.0), (6.7, 6.5)]);
            g.line(&[(13.3, 9.0), (13.3, 6.5)]);
            g.arc(10.0, 6.5, 3.3, PI, TAU);
            g.dot(10.0, 13.0, 1.2);
        }
        Icon::Usb => {
            g.line(&[(10.0, 17.5), (10.0, 3.5)]);
            g.line(&[(8.2, 5.4), (10.0, 3.2), (11.8, 5.4)]);
            g.line(&[(10.0, 12.0), (6.0, 9.5), (6.0, 7.6)]);
            g.circle(6.0, 6.6, 1.1);
            g.line(&[(10.0, 14.0), (14.0, 11.0), (14.0, 9.0)]);
            g.rect(12.8, 6.4, 15.2, 8.8, 0.3);
        }
        Icon::Chip => {
            g.rect(5.0, 5.0, 15.0, 15.0, 1.8);
            g.rect(8.0, 8.0, 12.0, 12.0, 0.8);
            for k in [7.5, 10.0, 12.5] {
                g.line(&[(k, 2.5), (k, 4.6)]);
                g.line(&[(k, 15.4), (k, 17.5)]);
                g.line(&[(2.5, k), (4.6, k)]);
                g.line(&[(15.4, k), (17.5, k)]);
            }
        }
        Icon::Plug => {
            g.line(&[(7.5, 2.5), (7.5, 6.0)]);
            g.line(&[(12.5, 2.5), (12.5, 6.0)]);
            g.closed(&[(5.0, 6.0), (15.0, 6.0), (15.0, 9.5), (12.0, 13.0), (8.0, 13.0), (5.0, 9.5)]);
            g.line(&[(10.0, 13.0), (10.0, 17.5)]);
        }
        Icon::Shield => {
            let mut v = vec![(10.0, 2.5), (16.5, 5.0), (16.5, 9.5)];
            quad((16.5, 9.5), (16.0, 15.0), (10.0, 17.6), 8, &mut v);
            quad((10.0, 17.6), (4.0, 15.0), (3.5, 9.5), 8, &mut v);
            v.push((3.5, 5.0));
            g.closed(&v);
        }
        Icon::Plus => {
            g.line(&[(10.0, 4.0), (10.0, 16.0)]);
            g.line(&[(4.0, 10.0), (16.0, 10.0)]);
        }
        Icon::Minus => g.line(&[(4.0, 10.0), (16.0, 10.0)]),
        Icon::Close => {
            g.line(&[(5.0, 5.0), (15.0, 15.0)]);
            g.line(&[(15.0, 5.0), (5.0, 15.0)]);
        }
        Icon::ChevronDown => g.line(&[(5.5, 8.0), (10.0, 12.5), (14.5, 8.0)]),
        Icon::ChevronUp => g.line(&[(5.5, 12.0), (10.0, 7.5), (14.5, 12.0)]),
        Icon::ChevronRight => g.line(&[(8.0, 5.5), (12.5, 10.0), (8.0, 14.5)]),
        Icon::ChevronLeft => g.line(&[(12.0, 5.5), (7.5, 10.0), (12.0, 14.5)]),
        Icon::More => {
            for x in [4.5, 10.0, 15.5] {
                g.dot(x, 10.0, 1.5);
            }
        }
        Icon::Copy => {
            g.rect(7.0, 7.0, 16.5, 16.5, 2.0);
            g.line(&[
                (13.0, 7.0),
                (13.0, 4.8),
                (12.0, 3.5),
                (4.8, 3.5),
                (3.5, 4.8),
                (3.5, 12.0),
                (4.8, 13.0),
                (7.0, 13.0),
            ]);
        }
        Icon::ExternalLink => {
            g.line(&[
                (10.5, 4.0),
                (5.0, 4.0),
                (4.0, 5.0),
                (4.0, 15.0),
                (5.0, 16.0),
                (15.0, 16.0),
                (16.0, 15.0),
                (16.0, 9.5),
            ]);
            g.line(&[(9.5, 10.5), (16.5, 3.5)]);
            g.line(&[(12.0, 3.5), (16.5, 3.5), (16.5, 8.0)]);
        }
        Icon::Refresh => {
            g.arc(10.0, 10.0, 6.5, -PI / 3.0, 1.45 * PI);
            let end = polar((10.0, 10.0), 6.5, -PI / 3.0);
            g.line(&[(end.0 - 3.6, end.1 - 0.4), end, (end.0 + 0.2, end.1 - 3.8)]);
        }
        Icon::Undo => {
            let mut v = vec![(5.0, 7.5), (12.0, 7.5)];
            for i in 0..=8 {
                let a = -PI / 2.0 + PI * i as f32 / 8.0;
                v.push(polar((12.0, 11.25), 3.75, a));
            }
            v.push((7.0, 15.0));
            g.line(&v);
            g.line(&[(8.0, 4.5), (5.0, 7.5), (8.0, 10.5)]);
        }
        Icon::Redo => {
            let mut v = vec![(15.0, 7.5), (8.0, 7.5)];
            for i in 0..=8 {
                let a = -PI / 2.0 - PI * i as f32 / 8.0;
                v.push(polar((8.0, 11.25), 3.75, a));
            }
            v.push((13.0, 15.0));
            g.line(&v);
            g.line(&[(12.0, 4.5), (15.0, 7.5), (12.0, 10.5)]);
        }
        Icon::Search => {
            g.circle(8.6, 8.6, 5.4);
            g.line(&[(12.6, 12.6), (16.8, 16.8)]);
        }
        Icon::Pencil => {
            g.closed(&[(4.0, 16.0), (4.6, 12.6), (13.0, 4.2), (15.8, 7.0), (7.4, 15.4)]);
            g.line(&[(11.2, 6.0), (14.0, 8.8)]);
        }
        Icon::Trash => {
            g.line(&[(3.5, 5.5), (16.5, 5.5)]);
            g.line(&[(7.5, 5.5), (8.0, 3.0), (12.0, 3.0), (12.5, 5.5)]);
            g.line(&[(5.0, 5.5), (6.0, 17.0), (14.0, 17.0), (15.0, 5.5)]);
            g.line(&[(8.5, 8.5), (8.5, 14.0)]);
            g.line(&[(11.5, 8.5), (11.5, 14.0)]);
        }
        Icon::Play => g.closed(&[(6.0, 4.0), (16.0, 10.0), (6.0, 16.0)]),
        Icon::Pause => {
            g.line(&[(7.0, 4.5), (7.0, 15.5)]);
            g.line(&[(13.0, 4.5), (13.0, 15.5)]);
        }
        Icon::Eye => {
            let mut v = Vec::new();
            quad((2.0, 10.0), (10.0, 2.0), (18.0, 10.0), 12, &mut v);
            quad((18.0, 10.0), (10.0, 18.0), (2.0, 10.0), 12, &mut v);
            g.closed(&v);
            g.circle(10.0, 10.0, 2.6);
        }
        Icon::Power => {
            g.arc(10.0, 11.0, 6.2, -PI / 2.0 + 0.75, 1.5 * PI - 0.75);
            g.line(&[(10.0, 2.8), (10.0, 9.8)]);
        }
        Icon::Keyboard => {
            g.rect(2.5, 5.5, 17.5, 14.5, 2.0);
            for x in [5.6, 8.5, 11.5, 14.4] {
                g.dot(x, 8.6, 0.95);
            }
            g.line(&[(7.0, 11.6), (13.0, 11.6)]);
        }
        Icon::Sun => {
            g.circle(10.0, 10.0, 3.4);
            for k in 0..8 {
                let a = k as f32 * TAU / 8.0;
                g.line(&[polar((10.0, 10.0), 6.0, a), polar((10.0, 10.0), 7.8, a)]);
            }
        }
        Icon::Moon => {
            // Crescent: outer circle minus an offset inner circle.
            let (co, ro) = ((10.0f32, 10.0f32), 7.0f32);
            let (ci, ri) = ((13.4f32, 6.6f32), 5.6f32);
            let start = (ci.1 - co.1).atan2(ci.0 - co.0);
            let mut v = Vec::new();
            for i in 0..=120 {
                let a = start + TAU * i as f32 / 120.0;
                let q = polar(co, ro, a);
                if ((q.0 - ci.0).powi(2) + (q.1 - ci.1).powi(2)).sqrt() >= ri {
                    v.push(q);
                }
            }
            let back = (co.1 - ci.1).atan2(co.0 - ci.0);
            let mut inner = Vec::new();
            for i in 0..=120 {
                let a = back + PI - TAU * i as f32 / 120.0;
                let q = polar(ci, ri, a);
                if ((q.0 - co.0).powi(2) + (q.1 - co.1).powi(2)).sqrt() <= ro {
                    inner.push(q);
                }
            }
            v.extend(inner);
            g.closed(&v);
        }
        Icon::Drag => {
            for y in [5.0, 10.0, 15.0] {
                g.dot(7.5, y, 1.3);
                g.dot(12.5, y, 1.3);
            }
        }
        Icon::Folder => g.closed(&[(2.5, 5.0), (8.0, 5.0), (9.5, 7.0), (17.5, 7.0), (17.5, 16.0), (2.5, 16.0)]),
        Icon::Report => {
            g.closed(&[(5.0, 2.5), (12.0, 2.5), (15.5, 6.0), (15.5, 17.5), (5.0, 17.5)]);
            g.line(&[(12.0, 2.5), (12.0, 6.0), (15.5, 6.0)]);
            g.line(&[(7.5, 10.0), (13.0, 10.0)]);
            g.line(&[(7.5, 13.5), (13.0, 13.5)]);
        }
        Icon::Bolt => g.closed(&[(11.0, 2.0), (4.8, 11.0), (9.4, 11.0), (8.6, 18.0), (15.2, 8.6), (10.6, 8.6)]),
        Icon::Board => {
            g.rect(3.0, 3.0, 17.0, 17.0, 1.6);
            g.rect(6.5, 6.0, 11.5, 11.0, 0.6);
            g.line(&[(14.0, 5.5), (14.0, 11.5)]);
            g.line(&[(6.0, 14.0), (14.0, 14.0)]);
        }
        Icon::Led => {
            g.rect(2.0, 7.5, 18.0, 12.5, 2.5);
            for x in [5.2, 8.4, 11.6, 14.8] {
                g.dot(x, 10.0, 1.15);
            }
        }
        Icon::Dot => g.dot(10.0, 10.0, 3.0),
        Icon::Download => {
            g.line(&[(10.0, 3.0), (10.0, 12.5)]);
            g.line(&[(6.0, 8.8), (10.0, 12.8), (14.0, 8.8)]);
            g.line(&[(4.0, 13.5), (4.0, 16.5), (16.0, 16.5), (16.0, 13.5)]);
        }
        Icon::Update => {
            g.line(&[(10.0, 3.0), (10.0, 12.0)]);
            g.line(&[(6.0, 8.5), (10.0, 12.5), (14.0, 8.5)]);
            g.line(&[(3.0, 13.0), (3.0, 17.0), (17.0, 17.0), (17.0, 13.0)]);
        }
        Icon::Leaf => {
            g.closed(&[(4.0, 16.0), (5.0, 9.0), (9.0, 5.0), (16.0, 4.0), (15.0, 11.0), (11.0, 15.0)]);
            g.line(&[(4.0, 16.0), (12.0, 8.0)]);
        }
        Icon::Balance => {
            g.line(&[(10.0, 3.0), (10.0, 17.0)]);
            g.line(&[(4.0, 6.0), (16.0, 6.0)]);
            g.line(&[(6.0, 17.0), (14.0, 17.0)]);
            g.arc(4.0, 10.0, 2.6, 0.0, PI);
            g.arc(16.0, 10.0, 2.6, 0.0, PI);
            g.line(&[(1.6, 10.0), (4.0, 6.0), (6.4, 10.0)]);
            g.line(&[(13.6, 10.0), (16.0, 6.0), (18.4, 10.0)]);
        }
        Icon::Gauge => {
            g.arc(10.0, 11.0, 7.0, 150f32.to_radians(), 390f32.to_radians());
            g.line(&[(10.0, 11.0), (15.0, 6.0)]);
            g.dot(10.0, 11.0, 1.2);
        }
        Icon::Forecast => {
            g.line(&[(3.0, 14.0), (7.0, 10.0), (10.0, 12.0)]);
            g.line(&[(11.5, 10.5), (13.0, 9.0)]);
            g.line(&[(14.5, 7.5), (16.0, 6.0)]);
            g.dot(17.0, 5.0, 1.3);
        }
        Icon::Bios => {
            g.rect(3.0, 3.0, 17.0, 17.0, 2.0);
            g.rect(6.0, 6.0, 11.0, 11.0, 1.0);
            g.line(&[(13.0, 6.0), (14.0, 6.0)]);
            g.line(&[(13.0, 9.0), (14.0, 9.0)]);
            g.line(&[(6.0, 14.0), (14.0, 14.0)]);
        }
        Icon::Wifi => {
            for r in [3.0, 6.5, 10.0] {
                g.arc(10.0, 16.0, r, 225f32.to_radians(), 315f32.to_radians());
            }
            g.dot(10.0, 16.0, 1.1);
        }
        Icon::Bluetooth => g.line(&[(6.0, 6.0), (14.0, 13.0), (10.0, 17.0), (10.0, 3.0), (14.0, 7.0), (6.0, 14.0)]),
        Icon::Lan => {
            g.rect(3.0, 5.0, 17.0, 16.0, 1.5);
            g.line(&[(7.0, 16.0), (7.0, 12.0), (13.0, 12.0), (13.0, 16.0)]);
            for x in [7.0, 9.0, 11.0, 13.0] {
                g.line(&[(x, 5.0), (x, 7.5)]);
            }
        }
        Icon::Audio => {
            g.closed(&[(3.0, 8.0), (6.0, 8.0), (10.0, 4.0), (10.0, 16.0), (6.0, 12.0), (3.0, 12.0)]);
            g.arc(11.0, 10.0, 3.5, -50f32.to_radians(), 50f32.to_radians());
            g.arc(11.0, 10.0, 6.5, -50f32.to_radians(), 50f32.to_radians());
        }
        Icon::Gpu => {
            g.rect(2.0, 5.0, 18.0, 15.0, 1.5);
            g.circle(7.5, 10.0, 2.6);
            g.circle(13.5, 10.0, 2.6);
            g.line(&[(4.0, 15.0), (4.0, 18.0)]);
        }
        Icon::Windows => {
            for (x, y) in [(3.0, 3.0), (10.5, 3.0), (3.0, 10.5), (10.5, 10.5)] {
                let r = Rect::from_min_max(g.pt(x, y), g.pt(x + 6.5, y + 6.5));
                g.p.rect_filled(r, 0.6 * g.s, g.color);
            }
        }
        Icon::Restart => {
            g.arc(10.0, 10.0, 6.5, 300f32.to_radians(), 590f32.to_radians());
            g.line(&[(13.5, 2.5), (14.8, 5.5), (11.8, 6.8)]);
        }
        Icon::Pipette => {
            g.line(&[(13.0, 3.0), (17.0, 7.0)]);
            g.line(&[(15.0, 5.0), (5.0, 15.0), (3.0, 17.0)]);
            g.line(&[(11.0, 5.0), (15.0, 9.0)]);
        }
    }
}

/// Fan rotor (4 swept blades + hub), rotated by `angle`; also the logo glyph.
fn fan(g: &G, angle: f32) {
    let c = (10.0, 10.0);
    for k in 0..4 {
        let a = angle + k as f32 * TAU / 4.0;
        let p0 = polar(c, 2.6, a);
        let tip = polar(c, 8.2, a + 0.55);
        let p1 = polar(c, 2.6, a + 1.25);
        let mut v = Vec::new();
        quad(p0, polar(c, 8.0, a - 0.15), tip, 6, &mut v);
        quad(tip, polar(c, 5.0, a + 1.15), p1, 6, &mut v);
        g.closed(&v);
    }
    g.circle(10.0, 10.0, 1.6);
}

/// Spinning fan glyph used in fan rows, the board map and the logo.
pub fn paint_fan(p: &Painter, rect: Rect, angle: f32, color: Color32) {
    let side = rect.width().min(rect.height());
    let o = rect.center() - vec2(side, side) * 0.5;
    let s = side / 20.0;
    let w = (1.5 * s).max(1.1);
    let g = G { p, o, s, stroke: Stroke::new(w, color), color };
    fan(&g, angle);
}

/// Small 4-pin connector glyph for header nodes on the board map.
pub fn paint_pins(p: &Painter, center: Pos2, pins: usize, pitch: f32, color: Color32) {
    let total = pitch * (pins as f32 - 1.0);
    for i in 0..pins {
        let c = pos2(center.x - total / 2.0 + pitch * i as f32, center.y);
        p.rect_filled(Rect::from_center_size(c, vec2(pitch * 0.55, pitch * 0.55)), 0.5, color);
    }
}
