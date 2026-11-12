//! Breakpoints and responsive row/grid helpers (replace `ui.columns`, which only does equal widths).

use egui::{Id, Layout, Rect, Ui, vec2};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Breakpoint {
    Narrow,
    Medium,
    Wide,
}

impl Breakpoint {
    /// From the window width in points.
    pub fn from_window(width: f32) -> Self {
        if width >= 1200.0 {
            Breakpoint::Wide
        } else if width >= 1000.0 {
            Breakpoint::Medium
        } else {
            Breakpoint::Narrow
        }
    }

    pub fn rail(self) -> bool {
        self == Breakpoint::Narrow
    }

    pub fn sidebar_width(self) -> f32 {
        if self.rail() { 64.0 } else { 224.0 }
    }

    pub fn gutter(self) -> f32 {
        if self == Breakpoint::Narrow { 16.0 } else { 24.0 }
    }

    pub fn gap(self) -> f32 {
        if self == Breakpoint::Narrow { 12.0 } else { 16.0 }
    }
}

/// Splits `width` into columns by `weights`, separated by `gap`. Pure (unit tested).
pub fn split_widths(width: f32, weights: &[f32], gap: f32) -> Vec<f32> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }
    let usable = (width - gap * (n as f32 - 1.0)).max(0.0);
    let total: f32 = weights.iter().map(|w| w.max(0.0)).sum::<f32>().max(1e-6);
    weights.iter().map(|w| usable * w.max(0.0) / total).collect()
}

/// Number of tiles per row for a tile grid.
pub fn grid_columns(width: f32, min_tile: f32, gap: f32, min_cols: usize) -> usize {
    (((width + gap) / (min_tile + gap)).floor() as usize).max(min_cols).max(1)
}

/// A row of columns with the given weights, top-aligned. Each column closure gets the row's target
/// height (the tallest *natural* column height of the previous frame) and returns its own natural
/// height, so cards can stretch to equal heights without ever growing from their own stretching.
pub fn row(ui: &mut Ui, id: Id, weights: &[f32], gap: f32, mut add: impl FnMut(usize, &mut Ui, f32) -> f32) {
    let widths = split_widths(ui.available_width(), weights, gap);
    let key = id.with("row_h");
    let prev_h: f32 = ui.data(|d| d.get_temp(key)).unwrap_or(0.0);
    let mut natural: f32 = 0.0;
    ui.push_id(id, |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (i, w) in widths.iter().enumerate() {
                let r = ui.allocate_ui_with_layout(vec2(*w, 0.0), Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(*w);
                    add(i, ui, prev_h)
                });
                natural = natural.max(r.inner);
            }
        });
    });
    if (natural - prev_h).abs() > 0.5 {
        ui.data_mut(|d| d.insert_temp(key, natural));
        ui.ctx().request_repaint();
    }
}

/// Stacks the columns vertically instead (narrow layouts).
pub fn stack(ui: &mut Ui, n: usize, gap: f32, mut add: impl FnMut(usize, &mut Ui)) {
    for i in 0..n {
        if i > 0 {
            ui.add_space(gap);
        }
        add(i, ui);
    }
}

/// Responsive tile grid: `count` tiles of height `tile_h`, at least `min_w` wide.
pub fn tile_grid(
    ui: &mut Ui,
    count: usize,
    min_w: f32,
    tile_h: f32,
    gap: f32,
    mut add: impl FnMut(usize, &mut Ui, Rect),
) {
    let width = ui.available_width();
    let cols = grid_columns(width, min_w, gap, 2);
    let tile_w = (width - gap * (cols as f32 - 1.0)) / cols as f32;
    let top = ui.cursor().min;
    let rows = count.div_ceil(cols);
    for i in 0..count {
        let (r, c) = (i / cols, i % cols);
        let rect = Rect::from_min_size(
            egui::pos2(top.x + c as f32 * (tile_w + gap), top.y + r as f32 * (tile_h + gap)),
            vec2(tile_w, tile_h),
        );
        add(i, ui, rect);
    }
    let h = rows as f32 * tile_h + rows.saturating_sub(1) as f32 * gap;
    ui.advance_cursor_after_rect(Rect::from_min_size(top, vec2(width, h)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakpoints() {
        assert_eq!(Breakpoint::from_window(900.0), Breakpoint::Narrow);
        assert_eq!(Breakpoint::from_window(999.0), Breakpoint::Narrow);
        assert_eq!(Breakpoint::from_window(1000.0), Breakpoint::Medium);
        assert_eq!(Breakpoint::from_window(1199.0), Breakpoint::Medium);
        assert_eq!(Breakpoint::from_window(1280.0), Breakpoint::Wide);
        assert!(Breakpoint::Narrow.rail());
        assert_eq!(Breakpoint::Wide.sidebar_width(), 224.0);
        assert_eq!(Breakpoint::Narrow.sidebar_width(), 64.0);
    }

    #[test]
    fn split_widths_respects_gap_and_ratio() {
        let w = split_widths(1000.0, &[1.0, 1.0, 2.0], 20.0);
        assert_eq!(w.len(), 3);
        assert!((w.iter().sum::<f32>() + 40.0 - 1000.0).abs() < 0.01);
        assert!((w[2] - 2.0 * w[0]).abs() < 0.01);
        assert!(split_widths(10.0, &[1.0, 1.0], 50.0).iter().all(|w| *w >= 0.0));
    }

    #[test]
    fn grid_columns_minimum() {
        assert_eq!(grid_columns(700.0, 156.0, 12.0, 2), 4);
        assert_eq!(grid_columns(200.0, 156.0, 12.0, 2), 2);
        // Minimum window (900 - 64 rail - gutters) still fits 2 gallery columns.
        assert!(grid_columns(900.0 - 64.0 - 32.0 - 360.0, 136.0, 12.0, 2) >= 2);
    }
}
