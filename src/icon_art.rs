// Procedural app icon: a fan rotor inside an RGB ring.
// Shared by build.rs (exe icon) and the app (window + tray icon), so it must stay dependency-free.

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h = (h.rem_euclid(1.0)) * 6.0;
    let i = h.floor();
    let f = h - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match i as i32 {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn wrap_angle(a: f32) -> f32 {
    let two_pi = std::f32::consts::TAU;
    let mut a = a.rem_euclid(two_pi);
    if a > std::f32::consts::PI {
        a -= two_pi;
    }
    a
}

/// Color (straight alpha, 0..1) of a single sample at normalized coordinates (-1..1).
fn sample(x: f32, y: f32) -> [f32; 4] {
    let r = (x * x + y * y).sqrt();
    let theta = y.atan2(x);

    // Rounded-square background.
    let corner = 0.42;
    let qx = (x.abs() - (1.0 - corner)).max(0.0);
    let qy = (y.abs() - (1.0 - corner)).max(0.0);
    let inside_bg = (qx * qx + qy * qy).sqrt() <= corner && x.abs() <= 1.0 && y.abs() <= 1.0;
    if !inside_bg {
        return [0.0, 0.0, 0.0, 0.0];
    }
    let mut col = [17.0 / 255.0, 21.0 / 255.0, 29.0 / 255.0];

    // RGB ring.
    if (0.76..=0.90).contains(&r) {
        let hue = (theta / std::f32::consts::TAU) + 0.55;
        col = hsv_to_rgb(hue, 0.80, 1.0);
    }

    // Swept fan blades.
    let (r0, r1) = (0.17_f32, 0.70_f32);
    if r >= r0 && r <= r1 {
        let t = (r - r0) / (r1 - r0);
        let mut half_width = 0.34 * (0.55 + 0.45 * t);
        let tip_start = 0.85;
        if t > tip_start {
            let k = (t - tip_start) / (1.0 - tip_start);
            half_width *= (1.0 - k * k).max(0.0).sqrt();
        }
        for blade in 0..5 {
            let center = blade as f32 * std::f32::consts::TAU / 5.0 + 1.25 * t;
            if wrap_angle(theta - center).abs() < half_width {
                // Iris accent of the UI palette (#7C83FF -> #A9AEFF).
                let iris = [124.0 / 255.0, 131.0 / 255.0, 1.0];
                let light = [169.0 / 255.0, 174.0 / 255.0, 1.0];
                col = lerp3(iris, light, t);
                break;
            }
        }
    }

    // Hub.
    if r < 0.15 {
        col = [0.92, 0.95, 0.98];
    }
    if r < 0.06 {
        col = [17.0 / 255.0, 21.0 / 255.0, 29.0 / 255.0];
    }
    [col[0], col[1], col[2], 1.0]
}

/// Renders the icon as straight-alpha RGBA8, `size * size * 4` bytes.
pub fn render_icon_rgba(size: u32) -> Vec<u8> {
    const SS: u32 = 4;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32 * 2.0 - 1.0;
                    let fy = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32 * 2.0 - 1.0;
                    let s = sample(fx, -fy);
                    acc[0] += s[0] * s[3];
                    acc[1] += s[1] * s[3];
                    acc[2] += s[2] * s[3];
                    acc[3] += s[3];
                }
            }
            let n = (SS * SS) as f32;
            let a = acc[3] / n;
            let (r, g, b) =
                if acc[3] > 0.0 { (acc[0] / acc[3], acc[1] / acc[3], acc[2] / acc[3]) } else { (0.0, 0.0, 0.0) };
            out.push((r * 255.0).round() as u8);
            out.push((g * 255.0).round() as u8);
            out.push((b * 255.0).round() as u8);
            out.push((a * 255.0).round() as u8);
        }
    }
    out
}

/// Icon with a status overlay for the tray: 0 = normal, 1 = warning dot (amber), 2 = critical ring (red).
#[allow(dead_code)]
pub fn render_icon_rgba_state(size: u32, state: u8) -> Vec<u8> {
    let mut out = render_icon_rgba(size);
    overlay_state(&mut out, size, state);
    out
}

/// Paints the tray status badge onto any `size`² RGBA icon (the rendered PNG icon or the procedural
/// one): 1 = amber dot bottom-right, 2 = red ring.
#[allow(dead_code)]
pub fn overlay_state(out: &mut [u8], size: u32, state: u8) {
    if state == 0 || out.len() < (size * size * 4) as usize {
        return;
    }
    let s = size as f32;
    for py in 0..size {
        for px in 0..size {
            let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
            let i = ((py * size + px) * 4) as usize;
            let paint = match state {
                1 => {
                    // amber dot, bottom-right, with a dark rim
                    let (cx, cy, r) = (s * 0.78, s * 0.78, s * 0.22);
                    let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
                    if d <= r - s * 0.05 {
                        Some([242u8, 177, 58, 255])
                    } else if d <= r {
                        Some([15, 16, 19, 255])
                    } else {
                        None
                    }
                }
                _ => {
                    // red ring around the edge
                    let (cx, cy) = (s / 2.0, s / 2.0);
                    let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
                    if d >= s * 0.40 && d <= s * 0.5 { Some([255u8, 92, 106, 255]) } else { None }
                }
            };
            if let Some(c) = paint {
                out[i..i + 4].copy_from_slice(&c);
            }
        }
    }
}
