//! LED strip preview: glowing dots (≤ 60 LEDs, binned to ≥ 6 px), a diffused gradient strip for
//! long strips, or one bar for 12 V single-color headers.

use egui::epaint::Mesh;
use egui::{Color32, Id, Painter, Rect, Response, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::ui::theme::{R_SM, pal, with_alpha};

fn c32(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

fn lum(c: [u8; 3]) -> f32 {
    (0.2126 * (c[0] as f32 / 255.0).powf(2.2)
        + 0.7152 * (c[1] as f32 / 255.0).powf(2.2)
        + 0.0722 * (c[2] as f32 / 255.0).powf(2.2))
    .powf(1.0 / 2.2)
}

/// Average colors into `bins` buckets (keeps dots ≥ 6 px on narrow strips).
pub fn bin(colors: &[[u8; 3]], bins: usize) -> Vec<[u8; 3]> {
    if bins == 0 || colors.len() <= bins {
        return colors.to_vec();
    }
    (0..bins)
        .map(|b| {
            let (s, e) = (b * colors.len() / bins, ((b + 1) * colors.len() / bins).max(b * colors.len() / bins + 1));
            let slice = &colors[s..e.min(colors.len())];
            let n = slice.len().max(1) as u32;
            let sum = slice.iter().fold([0u32; 3], |a, c| [a[0] + c[0] as u32, a[1] + c[1] as u32, a[2] + c[2] as u32]);
            [(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8]
        })
        .collect()
}

/// Paints LED dots with glow inside `rect` (no background).
pub fn paint_dots(painter: &Painter, rect: Rect, colors: &[[u8; 3]], off: Color32) {
    let n = colors.len().max(1);
    let step = rect.width() / n as f32;
    let r = (step * 0.36).min(rect.height() * 0.32).max(1.2);
    for (i, c) in colors.iter().enumerate() {
        let center = pos2(rect.left() + step * (i as f32 + 0.5), rect.center().y);
        let l = lum(*c);
        if l > 0.03 {
            painter.circle_filled(center, r * 2.4, with_alpha(c32(*c), 0.10 * l));
            painter.circle_filled(center, r * 1.6, with_alpha(c32(*c), 0.22 * l));
            painter.circle_filled(center, r, c32(*c));
        } else {
            painter.circle_filled(center, r, off);
        }
    }
}

/// Continuous gradient strip (diffused look).
pub fn paint_diffused(painter: &Painter, rect: Rect, colors: &[[u8; 3]]) {
    if colors.is_empty() {
        return;
    }
    let n = colors.len();
    let core = Rect::from_center_size(rect.center(), vec2(rect.width(), (rect.height() * 0.32).max(4.0)));
    let glow = core.expand2(vec2(0.0, rect.height() * 0.22));
    let mut mesh = Mesh::default();
    for (band, alpha) in [(glow, 0.0f32), (core, 1.0f32)] {
        for (i, c) in colors.iter().enumerate() {
            let x0 = band.left() + band.width() * i as f32 / n as f32;
            let x1 = band.left() + band.width() * (i + 1) as f32 / n as f32;
            let col = c32(*c);
            let base = mesh.vertices.len() as u32;
            if alpha < 1.0 {
                // soft halo: transparent edges, 40 % centre
                let mid = with_alpha(col, 0.4);
                let edge = with_alpha(col, 0.0);
                mesh.colored_vertex(pos2(x0, band.top()), edge);
                mesh.colored_vertex(pos2(x1, band.top()), edge);
                mesh.colored_vertex(pos2(x1, band.center().y), mid);
                mesh.colored_vertex(pos2(x0, band.center().y), mid);
                mesh.colored_vertex(pos2(x1, band.bottom()), edge);
                mesh.colored_vertex(pos2(x0, band.bottom()), edge);
                mesh.add_triangle(base, base + 1, base + 2);
                mesh.add_triangle(base, base + 2, base + 3);
                mesh.add_triangle(base + 3, base + 2, base + 4);
                mesh.add_triangle(base + 3, base + 4, base + 5);
            } else {
                mesh.colored_vertex(pos2(x0, band.top()), col);
                mesh.colored_vertex(pos2(x1, band.top()), col);
                mesh.colored_vertex(pos2(x1, band.bottom()), col);
                mesh.colored_vertex(pos2(x0, band.bottom()), col);
                mesh.add_triangle(base, base + 1, base + 2);
                mesh.add_triangle(base, base + 2, base + 3);
            }
        }
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// Full strip widget with bed, hover read-out (LED index + hex).
pub fn led_strip(ui: &mut Ui, id: Id, colors: &[[u8; 3]], height: f32, addressable: bool, diffused: bool) -> Response {
    let p = pal(ui);
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let resp = ui.interact(rect, id, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, R_SM, p.inset);
    painter.rect_stroke(rect, R_SM, Stroke::new(1.0, p.border_subtle), StrokeKind::Inside);
    let inner = rect.shrink2(vec2(10.0, 4.0));
    let off = if p.dark { Color32::from_rgb(26, 28, 34) } else { Color32::from_rgb(214, 217, 223) };
    if !addressable {
        let c = colors.first().copied().unwrap_or([0, 0, 0]);
        let bar = Rect::from_center_size(inner.center(), vec2(inner.width(), (height * 0.28).max(5.0)));
        if lum(c) > 0.03 {
            painter.rect_filled(bar.expand2(vec2(0.0, height * 0.2)), 8, with_alpha(c32(c), 0.12));
            painter.rect_filled(bar, 4, c32(c));
        } else {
            painter.rect_filled(bar, 4, off);
        }
    } else if diffused || colors.len() > 60 {
        paint_diffused(&painter, inner, colors);
    } else {
        let max_bins = (inner.width() / 6.0).floor() as usize;
        let shown = if colors.len() > max_bins { bin(colors, max_bins) } else { colors.to_vec() };
        paint_dots(&painter, inner, &shown, off);
    }
    if let (Some(h), true) = (resp.hover_pos(), addressable && !colors.is_empty()) {
        let i = (((h.x - inner.left()) / inner.width()) * colors.len() as f32)
            .floor()
            .clamp(0.0, colors.len() as f32 - 1.0) as usize;
        let c = colors[i];
        return resp.on_hover_text(format!("LED {} · {}", i + 1, super::color::to_hex(c)));
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binning_averages() {
        let c = vec![[0, 0, 0], [100, 100, 100], [200, 200, 200], [255, 255, 255]];
        let b = bin(&c, 2);
        assert_eq!(b, vec![[50, 50, 50], [227, 227, 227]]);
        assert_eq!(bin(&c, 8), c);
        assert_eq!(bin(&c, 1).len(), 1);
    }
}
