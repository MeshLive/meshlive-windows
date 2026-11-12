//! Android's round control rows (`LiveControlSlots` and the call screen's
//! `activeCallControls`): one row of equal columns with End in the middle. A
//! control with nothing to do keeps its column (disabled, or hidden where
//! Android hides it), so End never moves.
use super::{
    components::{Kind, focus_ring},
    icons::{Icon, draw_icon},
    theme::{self, Type},
};
use egui::{Rect, Response, Sense, Ui, Vec2, WidgetInfo, WidgetType, pos2, vec2};

/// One column of a control row.
pub(super) struct Control<'a> {
    pub icon: Icon,
    /// The word under the button.
    pub caption: &'a str,
    /// What Narrator reads (Android's contentDescription).
    pub name: &'a str,
    pub kind: Kind,
    pub enabled: bool,
    /// False: the column stays empty (Android hides the button, keeps its place).
    pub shown: bool,
    /// The larger End button.
    pub large: bool,
}
impl<'a> Control<'a> {
    pub fn new(icon: Icon, caption: &'a str, kind: Kind) -> Self {
        Self { icon, caption, name: caption, kind, enabled: true, shown: true, large: false }
    }
    pub fn name(mut self, name: &'a str) -> Self {
        self.name = name;
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn shown(mut self, shown: bool) -> Self {
        self.shown = shown;
        self
    }
    pub fn large(mut self) -> Self {
        self.large = true;
        self
    }
}

/// A column's widest share of the row.
pub(super) const COLUMN: f32 = 104.0;
/// The row's height: the large button, the gap and the caption.
pub(super) const HEIGHT: f32 = theme::ROUND_CONTROL_LARGE + 8.0 + 20.0;

/// Draws the row centred in the available width. One response per control
/// (None for a hidden one), in order. The ids stay with the column, so a
/// control whose caption changes while it is held keeps its press.
pub(super) fn row(ui: &mut Ui, id: &str, controls: &[Control]) -> Vec<Option<Response>> {
    let full = ui.available_rect_before_wrap();
    let count = controls.len().max(1) as f32;
    let column = (full.width() / count).min(COLUMN);
    let start = full.center().x - column * count / 2.0;
    let mut out = Vec::with_capacity(controls.len());
    for (index, control) in controls.iter().enumerate() {
        if !control.shown {
            out.push(None);
            continue;
        }
        let cell = Rect::from_min_size(pos2(start + column * index as f32, full.top()), vec2(column, HEIGHT));
        let response = ui.add_enabled_ui(control.enabled, |ui| button(ui, cell, control, ui.id().with((id, index)))).inner;
        out.push(Some(response));
    }
    ui.advance_cursor_after_rect(Rect::from_min_size(full.min, vec2(full.width(), HEIGHT)));
    out
}

fn button(ui: &mut Ui, cell: Rect, control: &Control, id: egui::Id) -> Response {
    let size = if control.large { theme::ROUND_CONTROL_LARGE } else { theme::ROUND_CONTROL };
    let disc = Rect::from_center_size(pos2(cell.center().x, cell.top() + theme::ROUND_CONTROL_LARGE * 0.5), Vec2::splat(size));
    let response = ui.interact(disc, id, Sense::click_and_drag());
    let enabled = response.enabled();
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, control.name));
    if ui.is_rect_visible(cell) {
        let (fill, ink) = control.kind.colors(response.hovered(), response.is_pointer_button_down_on(), enabled);
        ui.painter().circle_filled(disc.center(), size * 0.5, fill);
        draw_icon(ui.painter(), Rect::from_center_size(disc.center(), Vec2::splat(size * 0.4)), control.icon, ink);
        let caption = ui.painter().layout_no_wrap(control.caption.to_owned(), Type::Caption.font(), theme::TEXT);
        let text = if enabled { theme::TEXT } else { theme::TEXT_DISABLED };
        let top = cell.top() + theme::ROUND_CONTROL_LARGE + 8.0;
        ui.painter().galley_with_override_text_color(pos2(cell.center().x - caption.size().x * 0.5, top), caption, text);
        focus_ring(ui, &response, disc, size * 0.5);
    }
    response
}

/// Android's level meter (a thin bar): `level` 0..1, white on the off track.
pub(super) fn level_meter(ui: &mut Ui, name: &str, level: f32) -> Response {
    let width = ui.available_width().min(320.0);
    let (rect, response) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::ProgressIndicator, true, name));
    ui.painter().rect_filled(rect, 3.0, theme::TRACK_OFF);
    let level = if level.is_finite() { level.clamp(0.0, 1.0) } else { 0.0 };
    if level > 0.0 {
        let filled = Rect::from_min_size(rect.min, vec2(rect.width() * level, rect.height()));
        ui.painter().rect_filled(filled, 3.0, theme::TEXT);
    }
    response
}
