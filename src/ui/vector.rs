//! The Android app's icons (VectorDrawable path data, `android_icons`) drawn
//! on Windows: each path is parsed (SVG path syntax), flattened and
//! rasterised once per icon and pixel size with 4 x 4 supersampling (filled
//! paths by their fill rule, stroked paths by distance), into a white alpha
//! texture that the painter tints. No icon font or extra dependency.
use super::android_icons::{VectorIcon, VectorPath};
use egui::{Color32, ColorImage, Painter, Pos2, Rect, TextureHandle, TextureOptions, pos2, vec2};

/// One flattened subpath in viewport units.
struct Subpath {
    points: Vec<Pos2>,
    closed: bool,
}

/// Parses SVG path data (M L H V C S Q T A Z, absolute and relative) into
/// flattened subpaths.
fn flatten(data: &str) -> Vec<Subpath> {
    let tokens = tokenize(data);
    let mut out: Vec<Subpath> = Vec::new();
    let mut i = 0;
    let mut cmd = 'M';
    let (mut cur, mut start) = (pos2(0.0, 0.0), pos2(0.0, 0.0));
    let mut last_ctrl: Option<(char, Pos2)> = None;
    let mut current: Vec<Pos2> = Vec::new();
    let finish = |current: &mut Vec<Pos2>, out: &mut Vec<Subpath>, closed: bool| {
        if current.len() > 1 {
            out.push(Subpath { points: std::mem::take(current), closed });
        } else {
            current.clear();
        }
    };
    while i < tokens.len() {
        if let Token::Cmd(c) = tokens[i] {
            cmd = c;
            i += 1;
            if c == 'Z' || c == 'z' {
                finish(&mut current, &mut out, true);
                cur = start;
                last_ctrl = None;
                continue;
            }
        }
        let rel = cmd.is_ascii_lowercase();
        let base = if rel { cur } else { pos2(0.0, 0.0) };
        match cmd.to_ascii_uppercase() {
            'M' => {
                let p = base + vec2(take(&tokens, &mut i), take(&tokens, &mut i));
                finish(&mut current, &mut out, false);
                current.push(p);
                cur = p;
                start = p;
                // Further pairs are line-tos.
                cmd = if rel { 'l' } else { 'L' };
                last_ctrl = None;
            }
            'L' => {
                let p = base + vec2(take(&tokens, &mut i), take(&tokens, &mut i));
                current.push(p);
                cur = p;
                last_ctrl = None;
            }
            'H' => {
                let x = take(&tokens, &mut i);
                let p = pos2(if rel { cur.x + x } else { x }, cur.y);
                current.push(p);
                cur = p;
                last_ctrl = None;
            }
            'V' => {
                let y = take(&tokens, &mut i);
                let p = pos2(cur.x, if rel { cur.y + y } else { y });
                current.push(p);
                cur = p;
                last_ctrl = None;
            }
            'C' | 'S' => {
                let smooth = cmd.eq_ignore_ascii_case(&'S');
                let c1 = if smooth {
                    match last_ctrl {
                        Some(('C', c)) => cur + (cur - c),
                        _ => cur,
                    }
                } else {
                    base + vec2(take(&tokens, &mut i), take(&tokens, &mut i))
                };
                let c2 = base + vec2(take(&tokens, &mut i), take(&tokens, &mut i));
                let p = base + vec2(take(&tokens, &mut i), take(&tokens, &mut i));
                for k in 1..=12 {
                    let t = k as f32 / 12.0;
                    let u = 1.0 - t;
                    current.push(pos2(
                        u * u * u * cur.x + 3.0 * u * u * t * c1.x + 3.0 * u * t * t * c2.x + t * t * t * p.x,
                        u * u * u * cur.y + 3.0 * u * u * t * c1.y + 3.0 * u * t * t * c2.y + t * t * t * p.y,
                    ));
                }
                last_ctrl = Some(('C', c2));
                cur = p;
            }
            'Q' | 'T' => {
                let smooth = cmd.eq_ignore_ascii_case(&'T');
                let c = if smooth {
                    match last_ctrl {
                        Some(('Q', c)) => cur + (cur - c),
                        _ => cur,
                    }
                } else {
                    base + vec2(take(&tokens, &mut i), take(&tokens, &mut i))
                };
                let p = base + vec2(take(&tokens, &mut i), take(&tokens, &mut i));
                for k in 1..=8 {
                    let t = k as f32 / 8.0;
                    let u = 1.0 - t;
                    current.push(pos2(u * u * cur.x + 2.0 * u * t * c.x + t * t * p.x, u * u * cur.y + 2.0 * u * t * c.y + t * t * p.y));
                }
                last_ctrl = Some(('Q', c));
                cur = p;
            }
            'A' => {
                let (rx, ry, rot, large, sweep) = (take(&tokens, &mut i), take(&tokens, &mut i), take(&tokens, &mut i), take(&tokens, &mut i) != 0.0, take(&tokens, &mut i) != 0.0);
                let p = base + vec2(take(&tokens, &mut i), take(&tokens, &mut i));
                arc(&mut current, cur, p, rx, ry, rot, large, sweep);
                cur = p;
                last_ctrl = None;
            }
            _ => {
                i += 1;
            }
        }
    }
    finish(&mut current, &mut out, false);
    out
}

fn take(tokens: &[Token], i: &mut usize) -> f32 {
    match tokens.get(*i) {
        Some(Token::Num(v)) => {
            *i += 1;
            *v
        }
        _ => {
            *i = tokens.len();
            0.0
        }
    }
}

/// SVG elliptical arc (endpoint form) appended as line segments.
#[allow(clippy::too_many_arguments)]
fn arc(points: &mut Vec<Pos2>, from: Pos2, to: Pos2, rx: f32, ry: f32, rot_deg: f32, large: bool, sweep: bool) {
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    if rx == 0.0 || ry == 0.0 || from == to {
        points.push(to);
        return;
    }
    let phi = rot_deg.to_radians();
    let (c, s) = (phi.cos(), phi.sin());
    let dx = (from.x - to.x) / 2.0;
    let dy = (from.y - to.y) / 2.0;
    let x1 = c * dx + s * dy;
    let y1 = -s * dx + c * dy;
    let lambda = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry);
    if lambda > 1.0 {
        rx *= lambda.sqrt();
        ry *= lambda.sqrt();
    }
    let num = rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1;
    let den = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    let mut coef = (num / den).max(0.0).sqrt();
    if large == sweep {
        coef = -coef;
    }
    let cx1 = coef * rx * y1 / ry;
    let cy1 = -coef * ry * x1 / rx;
    let cx = c * cx1 - s * cy1 + (from.x + to.x) / 2.0;
    let cy = s * cx1 + c * cy1 + (from.y + to.y) / 2.0;
    let angle = |ux: f32, uy: f32, vx: f32, vy: f32| {
        let a = (ux * vy - uy * vx).atan2(ux * vx + uy * vy);
        a
    };
    let theta1 = angle(1.0, 0.0, (x1 - cx1) / rx, (y1 - cy1) / ry);
    let mut delta = angle((x1 - cx1) / rx, (y1 - cy1) / ry, (-x1 - cx1) / rx, (-y1 - cy1) / ry);
    if !sweep && delta > 0.0 {
        delta -= std::f32::consts::TAU;
    } else if sweep && delta < 0.0 {
        delta += std::f32::consts::TAU;
    }
    let steps = ((delta.abs() / (std::f32::consts::PI / 16.0)).ceil() as usize).max(2);
    for k in 1..=steps {
        let t = theta1 + delta * k as f32 / steps as f32;
        let (ex, ey) = (rx * t.cos(), ry * t.sin());
        points.push(pos2(c * ex - s * ey + cx, s * ex + c * ey + cy));
    }
}

enum Token {
    Cmd(char),
    Num(f32),
}
fn tokenize(data: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let bytes = data.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if ch.is_ascii_alphabetic() && ch != 'e' && ch != 'E' {
            out.push(Token::Cmd(ch));
            i += 1;
        } else if ch == '-' || ch == '+' || ch == '.' || ch.is_ascii_digit() {
            let start = i;
            i += 1;
            let mut dot = ch == '.';
            while i < bytes.len() {
                let c = bytes[i] as char;
                if c.is_ascii_digit() {
                    i += 1;
                } else if c == '.' && !dot {
                    dot = true;
                    i += 1;
                } else if (c == 'e' || c == 'E') && i + 1 < bytes.len() {
                    i += 1;
                    if bytes[i] == b'-' || bytes[i] == b'+' {
                        i += 1;
                    }
                } else {
                    break;
                }
            }
            out.push(Token::Num(data[start..i].parse().unwrap_or(0.0)));
        } else {
            i += 1;
        }
    }
    out
}

/// Rasterises `icon` into a `size` x `size` coverage mask (0..=255).
pub fn rasterize(icon: &VectorIcon, size: usize) -> Vec<u8> {
    const SS: usize = 4;
    let scale = size as f32 / icon.viewport.0.max(icon.viewport.1);
    let to_px = |p: Pos2| pos2((p.x + icon.translate.0) * scale, (p.y + icon.translate.1) * scale);
    struct Prepared {
        edges: Vec<(Pos2, Pos2)>,
        segments: Vec<(Pos2, Pos2)>,
        fill: bool,
        even_odd: bool,
        half_width: f32,
    }
    let prepared: Vec<Prepared> = icon
        .paths
        .iter()
        .map(|path: &VectorPath| {
            let subpaths = flatten(path.data);
            let mut edges = Vec::new();
            let mut segments = Vec::new();
            for sub in &subpaths {
                let pts: Vec<Pos2> = sub.points.iter().map(|p| to_px(*p)).collect();
                for w in pts.windows(2) {
                    segments.push((w[0], w[1]));
                    edges.push((w[0], w[1]));
                }
                // Filling closes every subpath; a stroke only closed ones.
                if let (Some(&first), Some(&last)) = (pts.first(), pts.last()) {
                    edges.push((last, first));
                    if sub.closed {
                        segments.push((last, first));
                    }
                }
            }
            Prepared { edges, segments, fill: path.fill, even_odd: path.even_odd, half_width: path.stroke * scale * 0.5 }
        })
        .collect();
    let mut coverage = vec![0u16; size * size];
    let mut crossings: Vec<(f32, i32)> = Vec::new();
    for sy in 0..size * SS {
        let y = (sy as f32 + 0.5) / SS as f32;
        let row = sy / SS;
        let mut inside_row = vec![0u8; size * SS];
        for path in &prepared {
            if path.fill {
                crossings.clear();
                for &(a, b) in &path.edges {
                    if (a.y <= y && b.y > y) || (b.y <= y && a.y > y) {
                        let t = (y - a.y) / (b.y - a.y);
                        crossings.push((a.x + t * (b.x - a.x), if b.y > a.y { 1 } else { -1 }));
                    }
                }
                crossings.sort_by(|p, q| p.0.total_cmp(&q.0));
                let mut winding = 0;
                let mut k = 0;
                for sx in 0..size * SS {
                    let x = (sx as f32 + 0.5) / SS as f32;
                    while k < crossings.len() && crossings[k].0 < x {
                        winding += if path.even_odd { 1 } else { crossings[k].1 };
                        k += 1;
                    }
                    let inside = if path.even_odd { winding % 2 != 0 } else { winding != 0 };
                    if inside {
                        inside_row[sx] = 1;
                    }
                }
            }
            if path.half_width > 0.0 {
                let r = path.half_width;
                for &(a, b) in &path.segments {
                    if y < a.y.min(b.y) - r || y > a.y.max(b.y) + r {
                        continue;
                    }
                    let x0 = ((a.x.min(b.x) - r) * SS as f32).floor().max(0.0) as usize;
                    let x1 = (((a.x.max(b.x) + r) * SS as f32).ceil() as usize).min(size * SS);
                    for sx in x0..x1 {
                        if inside_row[sx] == 1 {
                            continue;
                        }
                        let p = pos2((sx as f32 + 0.5) / SS as f32, y);
                        if distance_to_segment(p, a, b) <= r {
                            inside_row[sx] = 1;
                        }
                    }
                }
            }
        }
        for (sx, &v) in inside_row.iter().enumerate() {
            coverage[row * size + sx / SS] += u16::from(v);
        }
    }
    coverage.iter().map(|&c| ((c as u32 * 255) / (SS * SS) as u32) as u8).collect()
}

fn distance_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    let t = if len2 == 0.0 { 0.0 } else { ((p - a).dot(ab) / len2).clamp(0.0, 1.0) };
    (p - (a + ab * t)).length()
}

/// Draws `icon` into `rect`, tinted `color`, pixel-aligned at the screen's
/// scale (textures are made once per icon and size).
pub fn draw(painter: &Painter, rect: Rect, icon: &'static VectorIcon, color: Color32) {
    let ppp = painter.pixels_per_point();
    let side = rect.size().min_elem();
    let px = (side * ppp).round().max(4.0) as usize;
    let ctx = painter.ctx();
    let id = egui::Id::new(("android-icon", icon.name, px));
    let texture = ctx.data_mut(|d| d.get_temp::<TextureHandle>(id)).unwrap_or_else(|| {
        let mask = rasterize(icon, px);
        let image = ColorImage::new([px, px], mask.iter().map(|&a| Color32::from_white_alpha(a)).collect());
        let handle = ctx.load_texture(format!("icon-{}-{px}", icon.name), image, TextureOptions::NEAREST);
        ctx.data_mut(|d| d.insert_temp(id, handle.clone()));
        handle
    });
    let size = px as f32 / ppp;
    let min = {
        use egui::emath::GuiRounding;
        (rect.center() - vec2(size, size) * 0.5).round_to_pixels(ppp)
    };
    let target = Rect::from_min_size(min, vec2(size, size));
    painter.image(texture.id(), target, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::android_icons;

    #[test]
    fn every_android_icon_parses_and_paints_something_inside_its_box() {
        for icon in android_icons::ALL {
            let mask = rasterize(icon, 48);
            let ink: u32 = mask.iter().map(|&a| u32::from(a)).sum();
            let covered = mask.iter().filter(|&&a| a > 127).count();
            assert!(covered > 48 * 48 / 40, "{} paints too little ({covered} px)", icon.name);
            assert!(covered < 48 * 48 * 9 / 10, "{} fills its whole box ({covered} px)", icon.name);
            assert!(ink > 0);
        }
    }

    #[test]
    fn a_square_path_fills_exactly_its_square() {
        let icon = VectorIcon {
            name: "square",
            viewport: (24.0, 24.0),
            translate: (0.0, 0.0),
            paths: &[VectorPath { data: "M6,6h12v12h-12z", fill: true, even_odd: false, stroke: 0.0 }],
        };
        let mask = rasterize(&icon, 24);
        assert_eq!(mask[12 * 24 + 12], 255);
        assert_eq!(mask[2 * 24 + 2], 0);
        assert_eq!(mask.iter().filter(|&&a| a == 255).count(), 144);
    }

    #[test]
    fn the_live_icon_is_symmetric_like_the_android_drawing() {
        // ic_live ("sensors") is mirror-symmetric around its vertical axis.
        let mask = rasterize(&android_icons::IC_LIVE, 48);
        let mut diff = 0u32;
        for y in 0..48 {
            for x in 0..24 {
                diff += (i32::from(mask[y * 48 + x]) - i32::from(mask[y * 48 + 47 - x])).unsigned_abs();
            }
        }
        assert!(diff < 48 * 48 * 8, "mirror difference {diff}");
    }
}
