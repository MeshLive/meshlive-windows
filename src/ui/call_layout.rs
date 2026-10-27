//! Call screen geometry (pure, in points): where the other person's picture
//! sits in the video area and where your own camera's tile goes. Android's
//! receiver is the reference (`VideoSurfaceTransform.calculateMatrixValues`
//! FIT and `CallPipLayout`): the sender's display rotation is a clockwise
//! quarter turn count, the picture is scaled uniformly (never stretched) and
//! centred, and the self-view rests in a corner inside the video area.
use egui::{Rect, Vec2, pos2, vec2};

/// The picture's size as shown, after `turns` clockwise quarter turns.
pub fn shown_size(size: [u32; 2], turns: u8) -> Vec2 {
    let (w, h) = (size[0].max(1) as f32, size[1].max(1) as f32);
    if turns % 2 == 1 { vec2(h, w) } else { vec2(w, h) }
}

/// FIT: the largest rectangle with the shown picture's aspect ratio inside
/// `area`, centred (black bars where the ratios differ). Returns the shown
/// rectangle and the unrotated one to draw before turning it about the centre.
pub fn fit(area: Rect, size: [u32; 2], turns: u8) -> (Rect, Rect) {
    let shown = shown_size(size, turns);
    let scale = (area.width() / shown.x).min(area.height() / shown.y).max(0.0);
    let on_screen = Rect::from_center_size(area.center(), shown * scale);
    let unrotated = Rect::from_center_size(area.center(), vec2(size[0].max(1) as f32, size[1].max(1) as f32) * scale);
    (on_screen, unrotated)
}

/// Gap between the video area's edge and the self-view tile.
pub const PIP_MARGIN: f32 = 12.0;
/// The self-view tile's width: a share of the video area, within bounds.
pub const PIP_WIDTH_FRACTION: f32 = 0.18;
pub const PIP_MIN_WIDTH: f32 = 96.0;
pub const PIP_MAX_WIDTH: f32 = 240.0;
/// The tile never takes more than this share of the area's height or width.
pub const PIP_MAX_SHARE: f32 = 0.42;

/// The self-view tile: the camera's own aspect ratio (`aspect` = width /
/// height; 4:3 until the first picture), bottom right inside `area`. It
/// shrinks with a small window but always stays inside the area, so it is
/// never pushed off screen.
pub fn pip(area: Rect, aspect: f32) -> Rect {
    let aspect = if aspect.is_finite() && aspect > 0.1 { aspect.clamp(0.4, 2.5) } else { 4.0 / 3.0 };
    let room = vec2((area.width() - 2.0 * PIP_MARGIN).max(0.0), (area.height() - 2.0 * PIP_MARGIN).max(0.0));
    let mut width = (area.width() * PIP_WIDTH_FRACTION).clamp(PIP_MIN_WIDTH, PIP_MAX_WIDTH);
    width = width.min(area.width() * PIP_MAX_SHARE).min(area.height() * PIP_MAX_SHARE * aspect);
    let size = vec2(width, width / aspect);
    // Never larger than the room inside the margins (a tiny window).
    let fitted = size * (room.x / size.x).min(room.y / size.y).min(1.0).max(0.0);
    let min = pos2(area.right() - PIP_MARGIN - fitted.x, area.bottom() - PIP_MARGIN - fitted.y);
    Rect::from_min_size(pos2(min.x.max(area.left()), min.y.max(area.top())), fitted)
}

/// Turns tight RGBA by `turns` clockwise quarter turns (the sender's display
/// rotation, as Android's receiver applies it), so the picture is uploaded
/// upright and drawn without a rotated mesh.
pub fn rotate_rgba(rgba: &[u8], width: usize, height: usize, turns: u8) -> (usize, usize, Vec<u8>) {
    let turns = turns % 4;
    if turns == 0 || rgba.len() < width * height * 4 {
        return (width, height, rgba.to_vec());
    }
    let (out_w, out_h) = if turns % 2 == 1 { (height, width) } else { (width, height) };
    let mut out = vec![0u8; out_w * out_h * 4];
    for y in 0..height {
        for x in 0..width {
            // Clockwise: (x, y) -> (H-1-y, x); half turn: (W-1-x, H-1-y);
            // three quarters: (y, W-1-x).
            let (nx, ny) = match turns {
                1 => (height - 1 - y, x),
                2 => (width - 1 - x, height - 1 - y),
                _ => (y, width - 1 - x),
            };
            let from = (y * width + x) * 4;
            let to = (ny * out_w + nx) * 4;
            out[to..to + 4].copy_from_slice(&rgba[from..from + 4]);
        }
    }
    (out_w, out_h, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inside(outer: Rect, inner: Rect) -> bool {
        inner.min.x >= outer.min.x - 0.01
            && inner.min.y >= outer.min.y - 0.01
            && inner.max.x <= outer.max.x + 0.01
            && inner.max.y <= outer.max.y + 0.01
    }

    #[test]
    fn rotation_turns_the_picture_clockwise_like_android() {
        // 2x1: red then green. One clockwise quarter turn: 1x2, red on top.
        let rgba = [255, 0, 0, 255, 0, 255, 0, 255];
        let (w, h, one) = rotate_rgba(&rgba, 2, 1, 1);
        assert_eq!((w, h), (1, 2));
        assert_eq!((&one[0..4], &one[4..8]), (&rgba[0..4], &rgba[4..8]), "clockwise: the left end goes to the top");
        let (w, h, three) = rotate_rgba(&rgba, 2, 1, 3);
        assert_eq!((w, h), (1, 2));
        assert_eq!((&three[0..4], &three[4..8]), (&rgba[4..8], &rgba[0..4]), "counter-clockwise: the right end on top");
        let (w, h, half) = rotate_rgba(&rgba, 2, 1, 2);
        assert_eq!((w, h, &half[0..4]), (2, 1, &rgba[4..8]));
        assert_eq!(rotate_rgba(&rgba, 2, 1, 4).2, rgba.to_vec(), "four turns is none");
        // A 352x288 frame with one turn is a 288x352 portrait.
        let frame = vec![7u8; 352 * 288 * 4];
        let (w, h, out) = rotate_rgba(&frame, 352, 288, 1);
        assert_eq!((w, h, out.len()), (288, 352, frame.len()));
    }

    #[test]
    fn a_portrait_phone_picture_keeps_its_shape_in_a_wide_area() {
        // The S24's 352x288 VP9 with one clockwise quarter turn (portrait),
        // in a wide call area (1536x864 logical: 1920x1080 at 125 %).
        let area = Rect::from_min_size(pos2(252.0, 220.0), vec2(1252.0, 420.0));
        let (shown, unrotated) = fit(area, [352, 288], 1);
        assert!((shown.height() - 420.0).abs() < 0.01, "fills the height: {shown:?}");
        assert!((shown.width() / shown.height() - 288.0 / 352.0).abs() < 0.001, "portrait 288:352, not stretched");
        assert!((shown.center() - area.center()).length() < 0.01, "centred");
        assert!(inside(area, shown));
        // The unrotated rectangle has the frame's own aspect; turning it
        // about the centre gives the shown one.
        assert!((unrotated.width() / unrotated.height() - 352.0 / 288.0).abs() < 0.001);
        assert!((unrotated.width() - shown.height()).abs() < 0.01 && (unrotated.height() - shown.width()).abs() < 0.01);
    }

    #[test]
    fn fit_letterboxes_and_pillarboxes_without_stretching() {
        let tall = Rect::from_min_size(pos2(0.0, 0.0), vec2(300.0, 900.0));
        let (shown, _) = fit(tall, [640, 360], 0);
        assert_eq!(shown.width(), 300.0);
        assert!((shown.height() - 168.75).abs() < 0.01, "16:9 bars above and below");
        for turns in 0..4u8 {
            let wide = Rect::from_min_size(pos2(10.0, 20.0), vec2(1000.0, 300.0));
            let (shown, _) = fit(wide, [352, 288], turns);
            let ratio = shown.width() / shown.height();
            let expect = if turns % 2 == 1 { 288.0 / 352.0 } else { 352.0 / 288.0 };
            assert!((ratio - expect).abs() < 0.001, "turns {turns}");
            assert!(inside(wide, shown), "turns {turns}");
        }
        // A degenerate area or size never panics or goes negative.
        let (shown, _) = fit(Rect::from_min_size(pos2(0.0, 0.0), vec2(0.0, 0.0)), [0, 0], 7);
        assert!(shown.width() >= 0.0 && shown.height() >= 0.0);
    }

    #[test]
    fn the_self_view_always_fits_inside_the_video_area() {
        for (w, h) in [(1252.0, 420.0), (1800.0, 900.0), (560.0, 200.0), (400.0, 120.0), (200.0, 60.0), (40.0, 30.0)] {
            for aspect in [4.0 / 3.0, 16.0 / 9.0, 352.0 / 288.0, 3.0 / 4.0] {
                let area = Rect::from_min_size(pos2(100.0, 50.0), vec2(w, h));
                let tile = pip(area, aspect);
                assert!(inside(area, tile), "{w}x{h} {aspect}: {tile:?}");
                assert!(tile.width() <= area.width() * PIP_MAX_SHARE + 0.01, "{w}x{h}: at most a share of the width");
                if tile.height() > 0.0 {
                    assert!((tile.width() / tile.height() - aspect).abs() < 0.01, "{w}x{h}: the camera's shape");
                }
                // Bottom right.
                assert!((area.right() - tile.right() - PIP_MARGIN).abs() < 0.01 || tile.width() == 0.0);
            }
        }
        // A normal window: a usable tile, not a sliver.
        let tile = pip(Rect::from_min_size(pos2(0.0, 0.0), vec2(1252.0, 420.0)), 4.0 / 3.0);
        assert!(tile.width() >= PIP_MIN_WIDTH && tile.width() <= PIP_MAX_WIDTH, "{tile:?}");
        // An unknown aspect falls back to 4:3.
        let tile = pip(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 600.0)), f32::NAN);
        assert!((tile.width() / tile.height() - 4.0 / 3.0).abs() < 0.01);
    }
}
