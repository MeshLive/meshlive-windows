use egui::{Color32, Painter, Rect, Stroke, StrokeKind};

/// One line-icon set (rounded caps, one stroke weight), drawn as vectors so it
/// stays sharp at every DPI. No symbol font or icon dependency.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Broadcast,
    Phone,
    People,
    Settings,
    Camera,
    Radio,
    Person,
    Shield,
    Info,
    Chevron,
    /// Left arrow (back).
    Back,
    Close,
    Check,
    /// Filled disc with a check (verified people only; drawn green).
    Verified,
    Plus,
    Mic,
    MicOff,
    VideoOff,
    /// Switch camera.
    Flip,
    /// Square stop (end broadcast).
    Stop,
    /// Handset laid flat (hang up / decline).
    HangUp,
    Qr,
    Lock,
    Warning,
    Search,
    Speaker,
    ChevronDown,
    /// Trash can (wipe, forget).
    Trash,
    Pencil,
    Bluetooth,
    /// Two bars (pause a broadcast).
    Pause,
    /// Arrow pointing down-left (an incoming call in Recent).
    CallIncoming,
    /// Arrow pointing up-right (an outgoing call in Recent).
    CallOutgoing,
    /// A person with a plus (Add contact; Android ic_person_add).
    PersonAdd,
}

/// The Android app's drawing for each icon it has (Material Symbols Rounded
/// and the app's own radio), so Windows shows the phone's icons. Icons with
/// no Android counterpart keep the line drawings below.
pub fn android(icon: Icon) -> Option<&'static super::android_icons::VectorIcon> {
    use super::android_icons::*;
    Some(match icon {
        Icon::Broadcast => &IC_LIVE,
        Icon::Phone => &IC_CALL,
        Icon::People => &IC_GROUP,
        Icon::Settings => &IC_SETTINGS,
        Icon::Camera => &IC_VIDEOCAM,
        Icon::VideoOff => &IC_VIDEOCAM_OFF,
        Icon::Radio => &IC_RADIO,
        Icon::Person => &IC_PERSON,
        Icon::PersonAdd => &IC_PERSON_ADD,
        Icon::Shield => &IC_VERIFIED_USER,
        Icon::Info => &IC_INFO,
        Icon::Chevron => &IC_CHEVRON_RIGHT,
        Icon::Back => &IC_ARROW_BACK,
        Icon::Close => &IC_CLOSE,
        Icon::Check => &IC_CHECK,
        Icon::Verified => &IC_CHECK_CIRCLE,
        Icon::Mic => &IC_MIC,
        Icon::MicOff => &IC_MIC_OFF,
        Icon::Flip => &IC_CAMERA_SWITCH,
        Icon::Stop => &IC_STOP,
        Icon::HangUp => &IC_CALL_END,
        Icon::Lock => &IC_LOCK,
        Icon::Warning => &IC_WARNING,
        Icon::Speaker => &IC_VOLUME_UP,
        Icon::ChevronDown => &IC_EXPAND_MORE,
        Icon::Bluetooth => &IC_BLUETOOTH,
        Icon::Pause => &IC_PAUSE,
        Icon::Plus | Icon::Qr | Icon::Search | Icon::Trash | Icon::Pencil | Icon::CallIncoming | Icon::CallOutgoing => return None,
    })
}

// Resolution-independent line drawings in a unit square.
pub fn draw_icon(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    if let Some(vector) = android(icon) {
        super::vector::draw(painter, rect, vector, color);
        return;
    }
    let size = rect.size().min_elem();
    let origin = rect.center() - egui::vec2(size, size) * 0.5;
    let point = |x: f32, y: f32| origin + egui::vec2(x, y) * size;
    let width = (size / 12.0).clamp(1.4, 2.6);
    let stroke = Stroke::new(width, color);
    let line = |points: &[(f32, f32)]| {
        painter.add(egui::Shape::line(points.iter().map(|&(x, y)| point(x, y)).collect(), stroke));
        // Round joins and caps: a dot at every vertex.
        for &(x, y) in points {
            painter.circle_filled(point(x, y), width * 0.5, color);
        }
    };
    let circle = |x: f32, y: f32, radius: f32| {
        painter.circle_stroke(point(x, y), radius * size, stroke);
    };
    let arc = |cx: f32, cy: f32, r: f32, from: f32, to: f32| {
        let points: Vec<_> = (0..=16)
            .map(|i| {
                let a = from + (to - from) * i as f32 / 16.0;
                point(cx + r * a.cos(), cy + r * a.sin())
            })
            .collect();
        painter.add(egui::Shape::line(points, stroke));
    };
    let rounded = |a: (f32, f32), b: (f32, f32), r: f32| {
        painter.rect_stroke(
            Rect::from_two_pos(point(a.0, a.1), point(b.0, b.1)),
            r * size,
            stroke,
            StrokeKind::Middle,
        );
    };
    let handset = |turn: bool| {
        let pts: &[(f32, f32)] = &[
            (0.22, 0.12),
            (0.1, 0.22),
            (0.14, 0.4),
            (0.3, 0.64),
            (0.57, 0.84),
            (0.76, 0.89),
            (0.9, 0.74),
            (0.72, 0.56),
            (0.59, 0.66),
            (0.38, 0.48),
            (0.31, 0.34),
            (0.41, 0.23),
            (0.26, 0.1),
            (0.22, 0.12),
        ];
        if turn {
            // Rotated so it lies like a hung-up receiver.
            let (s, c) = (135f32.to_radians().sin(), 135f32.to_radians().cos());
            let rotated: Vec<(f32, f32)> = pts
                .iter()
                .map(|&(x, y)| {
                    let (dx, dy) = (x - 0.5, y - 0.5);
                    (0.5 + dx * c - dy * s, 0.55 + dx * s + dy * c)
                })
                .collect();
            line(&rotated);
        } else {
            line(pts);
        }
    };
    match icon {
        Icon::Broadcast => {
            painter.circle_filled(point(0.5, 0.5), size * 0.08, color);
            for radius in [0.23, 0.4] {
                for side in [-1.0, 1.0] {
                    let points = (0..=12)
                        .map(|i| {
                            let angle = -0.85 + i as f32 * 1.7 / 12.0;
                            point(0.5 + side * radius * angle.cos(), 0.5 + radius * angle.sin())
                        })
                        .collect();
                    painter.add(egui::Shape::line(points, stroke));
                }
            }
        }
        Icon::Phone => handset(false),
        Icon::HangUp => handset(true),
        Icon::Person | Icon::People | Icon::PersonAdd => {
            circle(0.46, 0.3, 0.15);
            arc(0.46, 0.92, 0.33, -std::f32::consts::PI, 0.0);
            if matches!(icon, Icon::People) {
                arc(0.72, 0.3, 0.13, -1.4, 1.4);
                arc(0.78, 0.92, 0.26, -1.2, -0.05);
            }
        }
        Icon::Camera => {
            rounded((0.06, 0.25), (0.66, 0.76), 0.1);
            line(&[(0.68, 0.42), (0.94, 0.27), (0.94, 0.74), (0.68, 0.59)]);
        }
        Icon::VideoOff => {
            rounded((0.06, 0.25), (0.66, 0.76), 0.1);
            line(&[(0.68, 0.42), (0.94, 0.27), (0.94, 0.74), (0.68, 0.59)]);
            line(&[(0.08, 0.08), (0.92, 0.92)]);
        }
        Icon::Radio => {
            rounded((0.08, 0.3), (0.92, 0.88), 0.08);
            line(&[(0.25, 0.1), (0.78, 0.29)]);
            circle(0.34, 0.6, 0.15);
            line(&[(0.62, 0.52), (0.8, 0.52)]);
            line(&[(0.62, 0.68), (0.8, 0.68)]);
        }
        Icon::Settings => {
            let points = (0..48)
                .map(|i| {
                    let angle = i as f32 * std::f32::consts::TAU / 48.0;
                    let radius = if (i / 3) % 2 == 0 { 0.43 } else { 0.35 };
                    point(0.5 + radius * angle.cos(), 0.5 + radius * angle.sin())
                })
                .collect();
            painter.add(egui::Shape::closed_line(points, stroke));
            circle(0.5, 0.5, 0.13);
        }
        Icon::Shield => {
            line(&[
                (0.5, 0.06),
                (0.87, 0.21),
                (0.83, 0.59),
                (0.7, 0.79),
                (0.5, 0.94),
                (0.3, 0.79),
                (0.17, 0.59),
                (0.13, 0.21),
                (0.5, 0.06),
            ]);
            line(&[(0.33, 0.5), (0.46, 0.63), (0.68, 0.37)]);
        }
        Icon::Info => {
            circle(0.5, 0.5, 0.42);
            line(&[(0.5, 0.46), (0.5, 0.72)]);
            painter.circle_filled(point(0.5, 0.31), width * 0.8, color);
        }
        Icon::Chevron => line(&[(0.38, 0.2), (0.66, 0.5), (0.38, 0.8)]),
        Icon::ChevronDown => line(&[(0.2, 0.38), (0.5, 0.66), (0.8, 0.38)]),
        Icon::Back => {
            line(&[(0.82, 0.5), (0.18, 0.5)]);
            line(&[(0.44, 0.22), (0.16, 0.5), (0.44, 0.78)]);
        }
        Icon::Close => {
            line(&[(0.22, 0.22), (0.78, 0.78)]);
            line(&[(0.78, 0.22), (0.22, 0.78)]);
        }
        Icon::Check => line(&[(0.18, 0.52), (0.4, 0.74), (0.82, 0.28)]),
        Icon::Verified => {
            painter.circle_filled(point(0.5, 0.5), size * 0.46, color);
            let ink = Stroke::new(width, Color32::BLACK);
            painter.add(egui::Shape::line(vec![point(0.29, 0.52), point(0.44, 0.67), point(0.72, 0.36)], ink));
        }
        Icon::Plus => {
            line(&[(0.5, 0.18), (0.5, 0.82)]);
            line(&[(0.18, 0.5), (0.82, 0.5)]);
        }
        Icon::Mic | Icon::MicOff => {
            rounded((0.37, 0.08), (0.63, 0.6), 0.13);
            arc(0.5, 0.42, 0.26, 0.0, std::f32::consts::PI);
            line(&[(0.5, 0.68), (0.5, 0.88)]);
            line(&[(0.34, 0.9), (0.66, 0.9)]);
            if matches!(icon, Icon::MicOff) {
                line(&[(0.14, 0.1), (0.86, 0.9)]);
            }
        }
        Icon::Flip => {
            rounded((0.08, 0.28), (0.92, 0.84), 0.1);
            line(&[(0.34, 0.28), (0.4, 0.16), (0.6, 0.16), (0.66, 0.28)]);
            arc(0.5, 0.56, 0.15, -2.6, 0.6);
            line(&[(0.6, 0.52), (0.63, 0.65), (0.75, 0.58)]);
        }
        Icon::Stop => {
            painter.rect_filled(Rect::from_two_pos(point(0.24, 0.24), point(0.76, 0.76)), size * 0.06, color);
        }
        Icon::Qr => {
            for (x, y) in [(0.1, 0.1), (0.58, 0.1), (0.1, 0.58)] {
                rounded((x, y), (x + 0.32, y + 0.32), 0.05);
                painter.rect_filled(Rect::from_two_pos(point(x + 0.11, y + 0.11), point(x + 0.21, y + 0.21)), 0.0, color);
            }
            for (x, y) in [(0.62, 0.62), (0.84, 0.62), (0.73, 0.75), (0.62, 0.88), (0.84, 0.88)] {
                painter.rect_filled(Rect::from_center_size(point(x, y), egui::vec2(0.09, 0.09) * size), 0.0, color);
            }
        }
        Icon::Lock => {
            rounded((0.18, 0.44), (0.82, 0.9), 0.08);
            arc(0.5, 0.44, 0.2, -std::f32::consts::PI, 0.0);
            line(&[(0.5, 0.62), (0.5, 0.72)]);
        }
        Icon::Warning => {
            line(&[(0.5, 0.1), (0.94, 0.88), (0.06, 0.88), (0.5, 0.1)]);
            line(&[(0.5, 0.4), (0.5, 0.62)]);
            painter.circle_filled(point(0.5, 0.75), width * 0.8, color);
        }
        Icon::Search => {
            circle(0.42, 0.42, 0.28);
            line(&[(0.63, 0.63), (0.88, 0.88)]);
        }
        Icon::Speaker => {
            line(&[(0.1, 0.38), (0.28, 0.38), (0.52, 0.16), (0.52, 0.84), (0.28, 0.62), (0.1, 0.62), (0.1, 0.38)]);
            arc(0.52, 0.5, 0.2, -0.9, 0.9);
            arc(0.52, 0.5, 0.36, -0.9, 0.9);
        }
        Icon::Trash => {
            line(&[(0.14, 0.24), (0.86, 0.24)]);
            line(&[(0.38, 0.24), (0.4, 0.1), (0.6, 0.1), (0.62, 0.24)]);
            line(&[(0.22, 0.24), (0.28, 0.9), (0.72, 0.9), (0.78, 0.24)]);
            line(&[(0.43, 0.42), (0.43, 0.72)]);
            line(&[(0.57, 0.42), (0.57, 0.72)]);
        }
        Icon::Pencil => {
            line(&[(0.16, 0.84), (0.2, 0.64), (0.66, 0.18), (0.82, 0.34), (0.36, 0.8), (0.16, 0.84)]);
            line(&[(0.56, 0.28), (0.72, 0.44)]);
        }
        Icon::Pause => {
            for x in [0.3, 0.58] {
                painter.rect_filled(Rect::from_two_pos(point(x, 0.2), point(x + 0.12, 0.8)), size * 0.04, color);
            }
        }
        Icon::CallIncoming => {
            line(&[(0.78, 0.22), (0.24, 0.76)]);
            line(&[(0.24, 0.4), (0.24, 0.76), (0.6, 0.76)]);
        }
        Icon::CallOutgoing => {
            line(&[(0.24, 0.76), (0.78, 0.22)]);
            line(&[(0.42, 0.22), (0.78, 0.22), (0.78, 0.58)]);
        }
        Icon::Bluetooth => line(&[(0.28, 0.3), (0.72, 0.7), (0.5, 0.9), (0.5, 0.1), (0.72, 0.3), (0.28, 0.7)]),
    }
}
