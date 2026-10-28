//! The shared Windows component set. Every screen builds from these (no
//! one-off styling), as the iOS `Components.swift`: pill buttons, the red Go
//! live pill, round call controls, plain list rows with dividers (no cards),
//! section headers, switch rows, chips, badges, the empty state, top bar,
//! segmented control, navigation items, text fields and dialogs.
//!
//! Every control is a real egui widget with an AccessKit name, reachable with
//! Tab, and draws a visible focus ring only while the keyboard is in use (the
//! Windows convention).
#![allow(dead_code)] // The shared API: screens use a growing subset.
use super::icons::{Icon, draw_icon};
use super::theme::{self, Type};
use egui::{Align2, Color32, CornerRadius, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2, WidgetInfo, WidgetType, vec2};
use std::sync::Arc;

// ----------------------------------------------------------------- helpers

fn radius(r: f32) -> CornerRadius {
    CornerRadius::same(r.round().clamp(0.0, 255.0) as u8)
}

/// True while the user moves focus with the keyboard (Tab / arrows), false
/// after a pointer press: focus rings show only for keyboard users.
pub fn keyboard_mode(ctx: &egui::Context) -> bool {
    let id = egui::Id::new("meshlive-keyboard-focus");
    let (key, pointer) = ctx.input(|i| {
        let key = i.events.iter().any(|e| {
            matches!(
                e,
                egui::Event::Key { key: egui::Key::Tab | egui::Key::ArrowDown | egui::Key::ArrowUp | egui::Key::ArrowLeft | egui::Key::ArrowRight, pressed: true, .. }
            )
        });
        (key, i.pointer.any_pressed())
    });
    ctx.data_mut(|d| {
        let mode = d.get_temp_mut_or(id, false);
        if key {
            *mode = true;
        } else if pointer {
            *mode = false;
        }
        *mode
    })
}

/// Draws the keyboard focus ring around `rect` when `response` has focus.
pub fn focus_ring(ui: &Ui, response: &Response, rect: Rect, corner: f32) {
    if response.has_focus() && keyboard_mode(ui.ctx()) {
        ui.painter().rect_stroke(rect.expand(3.0), radius(corner + 3.0), theme::focus_stroke(), StrokeKind::Outside);
    }
}

/// One line of text cut with an ellipsis at `max_width`.
pub fn single_line(ui: &Ui, text: &str, font: egui::FontId, color: Color32, max_width: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text.to_owned(), egui::TextFormat::simple(font, color));
    job.wrap.max_width = max_width.max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('\u{2026}');
    ui.painter().layout_job(job)
}

/// `text` wrapped at word boundaries to `max_width` (as many lines as needed).
pub fn wrapped(ui: &Ui, text: &str, font: egui::FontId, color: Color32, max_width: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text.to_owned(), egui::TextFormat::simple(font, color));
    job.wrap.max_width = max_width.max(1.0);
    ui.painter().layout_job(job)
}

// ------------------------------------------------------- card row dividers

thread_local! {
    /// Rows of the open `settings_card`s, innermost last: (row top, text x).
    static CARD_ROWS: std::cell::RefCell<Vec<Vec<(f32, f32)>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A row tells the innermost open card where it starts and where its text
/// begins. True inside a card: the card draws the dividers between its rows
/// (every row, nested groups included), so the row draws none itself.
pub fn card_row(top: f32, text_x: f32) -> bool {
    CARD_ROWS.with(|cards| {
        let mut cards = cards.borrow_mut();
        match cards.last_mut() {
            Some(rows) => {
                rows.push((top, text_x));
                true
            }
            None => false,
        }
    })
}

/// The colours of each button kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Kind {
    /// White fill, black text: the screen's main action.
    #[default]
    Primary,
    /// Raised grey fill, white text.
    Secondary,
    /// Solid red: Go live, End broadcast, hang up.
    Live,
    /// Solid red: Delete everything, Forget, Decline.
    Destructive,
    /// Solid green: Answer only.
    Positive,
    /// Text only until hovered (Cancel, Back).
    Quiet,
}

impl Kind {
    /// (fill, content) for a state.
    pub fn colors(self, hovered: bool, pressed: bool, enabled: bool) -> (Color32, Color32) {
        if !enabled {
            return match self {
                Kind::Quiet => (Color32::TRANSPARENT, theme::TEXT_DISABLED),
                _ => (theme::RAISED, theme::TEXT_DISABLED),
            };
        }
        let pick = |rest: Color32, hover: Color32, down: Color32| {
            if pressed {
                down
            } else if hovered {
                hover
            } else {
                rest
            }
        };
        match self {
            Kind::Primary => (pick(theme::TEXT, theme::PRIMARY_HOVER, Color32::from_gray(0xCC)), Color32::BLACK),
            Kind::Secondary => (pick(theme::RAISED, theme::RAISED_HOVER, Color32::from_gray(0x36)), theme::TEXT),
            Kind::Live | Kind::Destructive => {
                (pick(theme::LIVE, theme::LIVE_HOVER, Color32::from_rgb(0xD9, 0x30, 0x27)), Color32::WHITE)
            }
            Kind::Positive => {
                (pick(theme::POSITIVE, theme::POSITIVE_HOVER, Color32::from_rgb(0x2A, 0xA8, 0x4A)), Color32::WHITE)
            }
            Kind::Quiet => (pick(Color32::TRANSPARENT, theme::RAISED, theme::RAISED_HOVER), theme::TEXT),
        }
    }
}

// ----------------------------------------------------------------- buttons

/// A fully rounded pill button.
///
/// `Pill::new("Save").show(ui)`; `.kind(Kind::Live)`, `.icon(Icon::Broadcast)`,
/// `.fill_width()`, `.large()`, `.small()`, `.enabled(false)`, `.a11y("...")`.
#[must_use]
pub struct Pill<'a> {
    label: &'a str,
    kind: Kind,
    icon: Option<Icon>,
    enabled: bool,
    fill_width: bool,
    min_width: f32,
    height: f32,
    a11y: Option<String>,
    icon_tint: Option<Color32>,
}

impl<'a> Pill<'a> {
    pub fn new(label: &'a str) -> Self {
        Self {
            label,
            kind: Kind::Primary,
            icon: None,
            enabled: true,
            fill_width: false,
            min_width: 96.0,
            height: theme::BUTTON_HEIGHT,
            a11y: None,
            icon_tint: None,
        }
    }
    /// The icon in its own colour (Android `iconTint`: the red live mark on
    /// the white Go live button).
    pub fn icon_tint(mut self, color: Color32) -> Self {
        self.icon_tint = Some(color);
        self
    }
    /// What the button looks like (for tests): kind, icon, icon tint.
    pub fn style(&self) -> (Kind, Option<Icon>, Option<Color32>) {
        (self.kind, self.icon, self.icon_tint)
    }
    pub fn kind(mut self, kind: Kind) -> Self {
        self.kind = kind;
        self
    }
    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    /// As wide as the space it is in.
    pub fn fill_width(mut self) -> Self {
        self.fill_width = true;
        self
    }
    pub fn min_width(mut self, width: f32) -> Self {
        self.min_width = width;
        self
    }
    /// A height of its own (Android text buttons are 48).
    pub fn height(mut self, height: f32) -> Self {
        self.height = height;
        self
    }
    /// The pinned screen action (Go live, Add contact).
    pub fn large(mut self) -> Self {
        self.height = theme::BUTTON_HEIGHT_LARGE;
        self
    }
    /// Inline actions inside rows and notices.
    pub fn small(mut self) -> Self {
        self.height = theme::BUTTON_HEIGHT_SMALL;
        self.min_width = 0.0;
        self
    }
    /// The accessible name when it differs from the visible label.
    pub fn a11y(mut self, name: impl Into<String>) -> Self {
        self.a11y = Some(name.into());
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        ui.add_enabled_ui(self.enabled, |ui| self.paint(ui)).inner
    }

    fn paint(self, ui: &mut Ui) -> Response {
        // Android Button: 17 semibold; Live: 17 bold; small inline: 14.
        let font = if self.height < theme::BUTTON_HEIGHT {
            egui::FontId::new(14.0, theme::semibold_family())
        } else if matches!(self.kind, Kind::Live | Kind::Destructive) {
            egui::FontId::new(17.0, theme::bold_family())
        } else {
            Type::Button.font()
        };
        let galley = ui.painter().layout_no_wrap(self.label.to_owned(), font.clone(), Color32::WHITE);
        // Android icons on 56 dp buttons are 24 dp with a 10 dp (Live) or
        // 8 dp gap; small inline buttons keep 16.
        let icon_size = if self.height < theme::BUTTON_HEIGHT { theme::ICON_SMALL } else { 24.0 };
        let gap = if matches!(self.kind, Kind::Live | Kind::Destructive) { 10.0 } else { 8.0 };
        let icon_w = if self.icon.is_some() { icon_size + gap } else { 0.0 };
        let pad = (self.height * 0.5).max(14.0);
        let natural = galley.size().x + icon_w + 2.0 * pad;
        let width = if self.fill_width { ui.available_width() } else { natural.max(self.min_width) };
        let (rect, response) = ui.allocate_exact_size(vec2(width, self.height), Sense::click());
        let enabled = response.enabled();
        let name = self.a11y.clone().unwrap_or_else(|| self.label.to_owned());
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, &name));
        if ui.is_rect_visible(rect) {
            let (fill, ink) = self.kind.colors(response.hovered(), response.is_pointer_button_down_on(), enabled);
            let corner = self.height * 0.5;
            ui.painter().rect_filled(rect, radius(corner), fill);
            let content_w = galley.size().x + icon_w;
            let mut x = rect.center().x - content_w * 0.5;
            if let Some(icon) = self.icon {
                let r = Rect::from_center_size(egui::pos2(x + icon_size * 0.5, rect.center().y), Vec2::splat(icon_size));
                let tint = if enabled { self.icon_tint.unwrap_or(ink) } else { ink };
                draw_icon(ui.painter(), r, icon, tint);
                x += icon_w;
            }
            let galley = single_line(ui, self.label, font, ink, rect.width() - 2.0 * pad);
            ui.painter().galley(egui::pos2(x, rect.center().y - galley.size().y * 0.5), galley, ink);
            focus_ring(ui, &response, rect, corner);
        }
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    }
}

/// White full-width pill: the screen's main action.
pub fn primary_button(ui: &mut Ui, label: &str) -> Response {
    Pill::new(label).fill_width().show(ui)
}
/// Raised grey pill of natural width.
pub fn secondary_button(ui: &mut Ui, label: &str) -> Response {
    Pill::new(label).kind(Kind::Secondary).show(ui)
}
/// Solid red pill of natural width (Delete everything, Forget).
pub fn destructive_button(ui: &mut Ui, label: &str) -> Response {
    Pill::new(label).kind(Kind::Destructive).show(ui)
}
/// "Go live": the full-width solid red pill with the broadcast mark.
pub fn live_button(ui: &mut Ui, label: &str, enabled: bool) -> Response {
    Pill::new(label).kind(Kind::Live).icon(Icon::Broadcast).fill_width().large().enabled(enabled).show(ui)
}

/// A round icon button (trailing call buttons in rows, top-bar actions).
pub fn icon_button(ui: &mut Ui, icon: Icon, name: &str, kind: Kind) -> Response {
    icon_button_sized(ui, icon, name, kind, theme::ICON_BUTTON)
}
pub fn icon_button_sized(ui: &mut Ui, icon: Icon, name: &str, kind: Kind, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let enabled = response.enabled();
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, name));
    if ui.is_rect_visible(rect) {
        let (fill, ink) = kind.colors(response.hovered(), response.is_pointer_button_down_on(), enabled);
        ui.painter().circle_filled(rect.center(), size * 0.5, fill);
        let glyph = (size * 0.5).min(theme::ICON);
        draw_icon(ui.painter(), Rect::from_center_size(rect.center(), Vec2::splat(glyph)), icon, ink);
        focus_ring(ui, &response, rect, size * 0.5);
    }
    response.on_hover_text(name)
}

/// A round call control (Mute, Flip, End, Answer, Decline) with its label
/// underneath. `Kind::Primary` shows a control that is on (white disc).
pub fn round_button(ui: &mut Ui, icon: Icon, label: &str, kind: Kind) -> Response {
    round_button_sized(ui, icon, label, kind, theme::ROUND_CONTROL)
}
pub fn round_button_sized(ui: &mut Ui, icon: Icon, label: &str, kind: Kind, size: f32) -> Response {
    let caption = ui.painter().layout_no_wrap(label.to_owned(), Type::Caption.font(), theme::TEXT);
    let width = size.max(caption.size().x);
    let (rect, response) = ui.allocate_exact_size(vec2(width, size + 8.0 + caption.size().y), Sense::click());
    let enabled = response.enabled();
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    if ui.is_rect_visible(rect) {
        let disc = Rect::from_center_size(egui::pos2(rect.center().x, rect.top() + size * 0.5), Vec2::splat(size));
        let (fill, ink) = kind.colors(response.hovered(), response.is_pointer_button_down_on(), enabled);
        ui.painter().circle_filled(disc.center(), size * 0.5, fill);
        draw_icon(ui.painter(), Rect::from_center_size(disc.center(), Vec2::splat(size * 0.4)), icon, ink);
        let text = if enabled { theme::TEXT } else { theme::TEXT_DISABLED };
        ui.painter().galley_with_override_text_color(
            egui::pos2(rect.center().x - caption.size().x * 0.5, disc.bottom() + 8.0),
            caption,
            text,
        );
        focus_ring(ui, &response, disc, size * 0.5);
    }
    response
}

// ------------------------------------------------------------------- rows

/// What leads a list row.
#[derive(Clone, Debug, PartialEq)]
pub enum Leading {
    None,
    /// A line icon on a raised disc.
    Icon(Icon),
    /// A person: their initial on a raised disc (a person glyph when empty).
    Avatar(String),
    /// A live stream: the broadcast mark on a red disc.
    Live,
}

/// What ends a list row (painted, not a separate control).
#[derive(Clone, Debug, PartialEq)]
pub enum Trailing {
    None,
    Chevron,
    /// A current value in secondary text ("Auto", "Not set").
    Value(String),
    /// A value followed by a chevron.
    ValueChevron(String),
}

/// A plain list row on black: leading disc, title (+ verified check),
/// optional subtitle, trailing value/chevron or custom trailing controls,
/// and an optional inset divider. No card.
#[must_use]
pub struct ListRow<'a> {
    title: &'a str,
    subtitle: Option<&'a str>,
    leading: Leading,
    trailing: Trailing,
    verified: bool,
    clickable: bool,
    divider: bool,
    a11y: Option<String>,
    title_color: Color32,
}

impl<'a> ListRow<'a> {
    pub fn new(title: &'a str) -> Self {
        Self {
            title,
            subtitle: None,
            leading: Leading::None,
            trailing: Trailing::None,
            verified: false,
            clickable: true,
            divider: false,
            a11y: None,
            title_color: theme::TEXT,
        }
    }
    pub fn subtitle(mut self, subtitle: &'a str) -> Self {
        self.subtitle = (!subtitle.is_empty()).then_some(subtitle);
        self
    }
    pub fn leading(mut self, leading: Leading) -> Self {
        self.leading = leading;
        self
    }
    pub fn icon(self, icon: Icon) -> Self {
        self.leading(Leading::Icon(icon))
    }
    pub fn trailing(mut self, trailing: Trailing) -> Self {
        self.trailing = trailing;
        self
    }
    pub fn chevron(self) -> Self {
        self.trailing(Trailing::Chevron)
    }
    /// The green verified check after the title.
    pub fn verified(mut self, verified: bool) -> Self {
        self.verified = verified;
        self
    }
    /// Rows are clickable by default; a row with its own trailing buttons
    /// can be plain text.
    pub fn clickable(mut self, clickable: bool) -> Self {
        self.clickable = clickable;
        self
    }
    /// A hairline under the row, inset to the text.
    pub fn divider(mut self, divider: bool) -> Self {
        self.divider = divider;
        self
    }
    /// The accessible name (default: "title, subtitle").
    pub fn a11y(mut self, name: impl Into<String>) -> Self {
        self.a11y = Some(name.into());
        self
    }
    /// Destructive rows ("Delete everything") use red text.
    pub fn danger(mut self) -> Self {
        self.title_color = theme::LIVE;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        self.show_with(ui, |_| ()).0
    }

    /// `trailing` lays out controls right to left at the row's end (call
    /// buttons, a switch); they take clicks before the row does.
    pub fn show_with<R>(self, ui: &mut Ui, trailing: impl FnOnce(&mut Ui) -> R) -> (Response, R) {
        let width = ui.available_width();
        // A value that doesn't fit beside the title moves under it (Android
        // settings value rows), wrapped, with the chevron still at the end.
        let inset = if self.leading != Leading::None { theme::ROW_TEXT_INSET } else { 0.0 };
        let chevron_w = if matches!(self.trailing, Trailing::Chevron | Trailing::ValueChevron(_)) { theme::ICON_SMALL + 10.0 } else { 0.0 };
        let below = match &self.trailing {
            Trailing::Value(value) | Trailing::ValueChevron(value) if self.subtitle.is_none() => {
                let check = if self.verified { theme::ICON_SMALL + 6.0 } else { 0.0 };
                let title_w = ui.painter().layout_no_wrap(self.title.to_owned(), Type::RowTitle.font(), theme::TEXT).size().x + check;
                let value_w = ui.painter().layout_no_wrap(value.clone(), Type::Subtitle.font(), theme::TEXT_SECONDARY).size().x;
                let room = width - inset - chevron_w;
                // Beside the title a value gets at most 40 % of the row (it is
                // drawn cut there), so a longer one also goes under the title.
                (title_w + 24.0 + value_w > room || value_w > width * 0.4).then(|| {
                    let dim = if ui.is_enabled() { theme::TEXT_SECONDARY } else { theme::TEXT_DISABLED };
                    wrapped(ui, value, Type::Subtitle.font(), dim, (room - 12.0).max(40.0))
                })
            }
            _ => None,
        };
        let height = match (&below, self.subtitle) {
            (Some(value), _) => {
                let title_h = ui.painter().layout_no_wrap("Ag".into(), Type::RowTitle.font(), theme::TEXT).size().y;
                (title_h + theme::LINE_GAP + value.size().y + 2.0 * theme::ROW_PAD).max(theme::ROW_HEIGHT_SUBTITLE)
            }
            (None, Some(_)) => theme::ROW_HEIGHT_SUBTITLE,
            (None, None) => theme::ROW_HEIGHT,
        };
        let sense = if self.clickable { Sense::click() } else { Sense::hover() };
        let (rect, response) = ui.allocate_exact_size(vec2(width, height), sense);
        let in_card = card_row(rect.top(), rect.left() + inset);
        let enabled = response.enabled();
        let name = self.a11y.clone().unwrap_or_else(|| match self.subtitle {
            Some(sub) => format!("{}, {sub}", self.title),
            None => self.title.to_owned(),
        });
        let kind = if self.clickable { WidgetType::Button } else { WidgetType::Label };
        response.widget_info(|| WidgetInfo::labeled(kind, enabled, &name));
        let painter = ui.painter().clone();
        // Hover wash, a little wider than the text column.
        let wash = rect.expand2(vec2(8.0, 0.0));
        if self.clickable && enabled && (response.hovered() || response.is_pointer_button_down_on()) {
            let fill = if response.is_pointer_button_down_on() { theme::PRESSED } else { theme::HOVER };
            painter.rect_filled(wash, radius(10.0), fill);
        }
        let ink = if enabled { self.title_color } else { theme::TEXT_DISABLED };
        let dim = if enabled { theme::TEXT_SECONDARY } else { theme::TEXT_DISABLED };
        // Leading disc.
        let mut text_x = rect.left();
        if self.leading != Leading::None {
            let disc = Rect::from_center_size(
                egui::pos2(rect.left() + theme::AVATAR * 0.5, rect.center().y),
                Vec2::splat(theme::AVATAR),
            );
            paint_leading(&painter, disc, &self.leading, enabled);
            text_x = rect.left() + theme::ROW_TEXT_INSET;
        }
        // Trailing controls first, so the text knows the room left.
        let trail_rect = Rect::from_min_max(egui::pos2(rect.center().x, rect.top()), rect.right_bottom());
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(trail_rect)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        child.spacing_mut().item_spacing.x = 8.0;
        let mut right = rect.right();
        match &self.trailing {
            Trailing::None => {}
            Trailing::Chevron | Trailing::ValueChevron(_) => {
                let r = Rect::from_center_size(egui::pos2(right - 8.0, rect.center().y), Vec2::splat(theme::ICON_SMALL));
                draw_icon(&painter, r, Icon::Chevron, dim);
                right -= theme::ICON_SMALL + 10.0;
                child.add_space(theme::ICON_SMALL + 10.0);
            }
            Trailing::Value(_) => {}
        }
        if let (Trailing::Value(value) | Trailing::ValueChevron(value), None) = (&self.trailing, &below) {
            let galley = single_line(ui, value, Type::Subtitle.font(), dim, rect.width() * 0.4);
            let w = galley.size().x;
            painter.galley(egui::pos2(right - w, rect.center().y - galley.size().y * 0.5), galley, dim);
            right -= w + 12.0;
            child.add_space(w + 12.0);
        }
        let inner = trailing(&mut child);
        let used = child.min_rect();
        if used.width() > 0.5 && used.left() < right {
            right = used.left() - 12.0;
        }
        // Title (+ check) and subtitle.
        let text_w = (right - text_x).max(24.0);
        let check = if self.verified { theme::ICON_SMALL + 6.0 } else { 0.0 };
        let title = single_line(ui, self.title, Type::RowTitle.font(), ink, text_w - check);
        let sub = below.or_else(|| self.subtitle.map(|s| single_line(ui, s, Type::Subtitle.font(), dim, text_w)));
        let block_h = title.size().y + sub.as_ref().map_or(0.0, |g| g.size().y + theme::LINE_GAP);
        let mut y = rect.center().y - block_h * 0.5;
        let title_size = title.size();
        painter.galley(egui::pos2(text_x, y), title, ink);
        if self.verified {
            let r = Rect::from_center_size(
                egui::pos2(text_x + title_size.x + 6.0 + theme::ICON_SMALL * 0.5, y + title_size.y * 0.5),
                Vec2::splat(theme::ICON_SMALL),
            );
            draw_icon(&painter, r, Icon::Verified, theme::POSITIVE);
        }
        y += title_size.y + theme::LINE_GAP;
        if let Some(sub) = sub {
            painter.galley(egui::pos2(text_x, y), sub, dim);
        }
        if self.divider && !in_card {
            paint_divider(&painter, (rect.left() + inset)..=rect.right(), rect.bottom() - 0.5);
        }
        if self.clickable {
            focus_ring(ui, &response, wash, 10.0);
        }
        let response = if self.clickable { response.on_hover_cursor(egui::CursorIcon::PointingHand) } else { response };
        (response, inner)
    }
}

fn paint_leading(painter: &egui::Painter, disc: Rect, leading: &Leading, enabled: bool) {
    let ink = if enabled { theme::TEXT } else { theme::TEXT_DISABLED };
    match leading {
        Leading::None => {}
        Leading::Icon(icon) => {
            painter.circle_filled(disc.center(), disc.width() * 0.5, theme::RAISED);
            draw_icon(painter, Rect::from_center_size(disc.center(), Vec2::splat(theme::ICON)), *icon, ink);
        }
        Leading::Live => {
            painter.circle_filled(disc.center(), disc.width() * 0.5, if enabled { theme::LIVE } else { theme::RAISED });
            draw_icon(painter, Rect::from_center_size(disc.center(), Vec2::splat(theme::ICON)), Icon::Broadcast, ink);
        }
        Leading::Avatar(name) => paint_avatar(painter, disc, name, ink),
    }
}

fn paint_avatar(painter: &egui::Painter, disc: Rect, name: &str, ink: Color32) {
    painter.circle_filled(disc.center(), disc.width() * 0.5, theme::RAISED);
    let initial: String = name
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default();
    if initial.is_empty() {
        draw_icon(painter, Rect::from_center_size(disc.center(), Vec2::splat(disc.width() * 0.5)), Icon::Person, ink);
    } else {
        painter.text(
            disc.center(),
            Align2::CENTER_CENTER,
            initial,
            egui::FontId::new(disc.width() * 0.42, theme::semibold_family()),
            ink,
        );
    }
}

/// A person's disc on its own (call screens): initial, or a person glyph.
pub fn avatar(ui: &mut Ui, name: &str, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    paint_avatar(ui.painter(), rect, name, theme::TEXT);
    response
}

/// A hairline across the column, starting `inset` points in.
pub fn divider(ui: &mut Ui, inset: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    paint_divider(ui.painter(), (rect.left() + inset)..=rect.right(), rect.center().y);
}

/// A divider line snapped to whole screen pixels. A 1-point line at a
/// fractional pixel position (125 % or 150 % scale) is blended over two pixel
/// rows at half strength, which on #121212 is nearly invisible: Settings
/// rows looked undivided. Android draws 1 dp solid.
pub fn paint_divider(painter: &egui::Painter, x: std::ops::RangeInclusive<f32>, y: f32) {
    let (y, width) = divider_geometry(y, painter.pixels_per_point());
    painter.hline(x, y, Stroke::new(width, theme::DIVIDER));
}
/// (y, width) in points: whole physical pixels, at least one, centred on a
/// pixel row (odd widths) or a pixel edge (even widths).
pub fn divider_geometry(y: f32, pixels_per_point: f32) -> (f32, f32) {
    let px = pixels_per_point.round().max(1.0);
    let physical = y * pixels_per_point;
    let snapped = if px as i32 % 2 == 1 { physical.floor() + 0.5 } else { physical.round() };
    (snapped / pixels_per_point, px / pixels_per_point)
}

/// The grey header above a list (Android SectionHeader: 15 semibold).
pub fn section_header(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(theme::text(Type::Section, text));
    ui.add_space(8.0);
}

/// The small grey label above a Settings group card (Android SectionLabel:
/// 13 regular, 4 dp start padding).
pub fn settings_label(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(theme::text(Type::SectionLabel, text));
    });
    ui.add_space(8.0);
}

/// One rounded group of Settings rows (Android Widget.App.SettingsCard:
/// #121212, radius 16, 16 dp start and 12 dp end padding).
/// The card draws a divider above every row but the first (Android: a line
/// between every row of a settings card, nested groups included); text that
/// is not a row (a reason, an expanded explanation) belongs to the row above.
pub fn settings_card<R>(ui: &mut Ui, content: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(theme::SURFACE)
        .corner_radius(16)
        .inner_margin(egui::Margin { left: 16, right: 12, top: 0, bottom: 0 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            CARD_ROWS.with(|cards| cards.borrow_mut().push(Vec::new()));
            let result = content(ui);
            let rows = CARD_ROWS.with(|cards| cards.borrow_mut().pop()).unwrap_or_default();
            let right = ui.max_rect().right();
            for &(top, text_x) in rows.iter().skip(1) {
                paint_divider(ui.painter(), text_x..=right, top);
            }
            result
        })
        .inner
}

// ---------------------------------------------------------------- switches

/// The switch itself: white track with a black knob when on, grey when off.
pub fn paint_switch(painter: &egui::Painter, rect: Rect, on: bool, enabled: bool, hovered: bool) {
    let (track, knob) = match (on, enabled) {
        (true, true) => (if hovered { theme::PRIMARY_HOVER } else { theme::TEXT }, Color32::BLACK),
        (false, true) => (if hovered { Color32::from_gray(0x48) } else { theme::TRACK_OFF }, theme::TEXT),
        (true, false) => (Color32::from_gray(0x70), Color32::from_gray(0x2A)),
        (false, false) => (theme::RAISED, theme::TEXT_DISABLED),
    };
    let r = rect.height() * 0.5;
    painter.rect_filled(rect, radius(r), track);
    let x = if on { rect.right() - r } else { rect.left() + r };
    painter.circle_filled(egui::pos2(x, rect.center().y), r - 3.0, knob);
}

pub const SWITCH_SIZE: Vec2 = vec2(40.0, 22.0);

/// A labelled slider as Android lays it out: the name on the left and the
/// value on the right of one line, the slider under them across the whole
/// width. Nothing in the row depends on the value's text, so the slider
/// never changes length or moves while it is dragged.
pub fn value_slider(ui: &mut Ui, label: &str, value: &mut u32, range: std::ops::RangeInclusive<u32>, step: u32, unit: &str) -> Response {
    let width = ui.available_width();
    let font = Type::Body.font();
    let shown = format!("{value} {unit}");
    let (line, _) = ui.allocate_exact_size(vec2(width, 22.0), Sense::hover());
    ui.painter().text(egui::pos2(line.left(), line.center().y), egui::Align2::LEFT_CENTER, label, font.clone(), theme::TEXT);
    let value_rect = ui.painter().text(egui::pos2(line.right(), line.center().y), egui::Align2::RIGHT_CENTER, &shown, font, theme::TEXT_SECONDARY);
    // The value as its own accessible label (the slider also reports it).
    ui.interact(value_rect, ui.id().with((label, "value")), Sense::hover())
        .widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &shown));
    ui.add_space(4.0);
    let slider = ui
        .scope(|ui| {
            ui.spacing_mut().slider_width = (width - 4.0).max(40.0);
            // egui ends the filled part at the handle plus the rail's corner
            // radius; the theme's pill radius made it run far past the handle
            // (it grew as the value grew). A rail-sized radius keeps it at the handle.
            let rail = egui::CornerRadius::same((ui.spacing().slider_rail_height / 2.0).round().clamp(0.0, 255.0) as u8);
            ui.visuals_mut().widgets.inactive.corner_radius = rail;
            ui.add(
                egui::Slider::new(value, range)
                    .step_by(f64::from(step))
                    .show_value(false)
                    .text(""),
            )
        })
        .inner;
    slider.widget_info(|| WidgetInfo::slider(true, f64::from(*value), label));
    slider
}

/// A short label and a switch on one line (no row of its own), e.g. the
/// Calls page's "Reachable". The whole thing toggles; `hint` is its tooltip.
/// `label` may be empty (the switch alone); `a11y` is its accessible name.
pub fn compact_switch(ui: &mut Ui, label: &str, a11y: &str, hint: &str, value: &mut bool, enabled: bool) -> Response {
    let font = egui::FontId::new(14.0, theme::semibold_family());
    let ink = if enabled { theme::TEXT } else { theme::TEXT_DISABLED };
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, ink);
    let gap = if label.is_empty() { 0.0 } else { 10.0 };
    let size = vec2(galley.size().x + gap + SWITCH_SIZE.x, SWITCH_SIZE.y.max(galley.size().y) + 8.0);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, mut response) = ui.allocate_exact_size(size, sense);
    if enabled && response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    let on = *value;
    response.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, enabled, on, a11y));
    let text_pos = egui::pos2(rect.left(), rect.center().y - galley.size().y * 0.5);
    ui.painter().galley(text_pos, galley, ink);
    let switch = Rect::from_min_size(egui::pos2(rect.right() - SWITCH_SIZE.x, rect.center().y - SWITCH_SIZE.y * 0.5), SWITCH_SIZE);
    paint_switch(ui.painter(), switch, on, enabled, response.hovered());
    focus_ring(ui, &response, switch, SWITCH_SIZE.y * 0.5);
    response.on_hover_text(hint)
}
/// The (i) button after a label: its size and the gap before it.
const INFO_SIZE: f32 = 28.0;
const INFO_GAP: f32 = 4.0;

/// A settings row with a switch at its end. The whole row toggles; its
/// accessible name is the title (a checkbox with its state). A long
/// explanation goes behind an (i) button that expands it under the row.
#[must_use]
pub struct ToggleRow<'a> {
    title: &'a str,
    subtitle: Option<&'a str>,
    info: Option<&'a str>,
    enabled: bool,
    divider: bool,
    icon: Option<Icon>,
}

impl<'a> ToggleRow<'a> {
    pub fn new(title: &'a str) -> Self {
        Self { title, subtitle: None, info: None, enabled: true, divider: false, icon: None }
    }
    /// A short current value or one-line note (no paragraphs).
    pub fn subtitle(mut self, subtitle: &'a str) -> Self {
        self.subtitle = (!subtitle.is_empty()).then_some(subtitle);
        self
    }
    /// The explanation shown by the (i) button.
    pub fn info(mut self, info: &'a str) -> Self {
        self.info = (!info.is_empty()).then_some(info);
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn divider(mut self, divider: bool) -> Self {
        self.divider = divider;
        self
    }
    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Returns the row's response; `.changed()` is true when it toggled.
    pub fn show(self, ui: &mut Ui, value: &mut bool) -> Response {
        ui.add_enabled_ui(self.enabled, |ui| self.paint(ui, value)).inner
    }

    fn paint(self, ui: &mut Ui, value: &mut bool) -> Response {
        let width = ui.available_width();
        let enabled_now = ui.is_enabled();
        let ink = if enabled_now { theme::TEXT } else { theme::TEXT_DISABLED };
        let dim = if enabled_now { theme::TEXT_SECONDARY } else { theme::TEXT_DISABLED };
        let inset = if self.icon.is_some() { theme::ROW_TEXT_INSET } else { 0.0 };
        // The label wraps; the (i) follows its last word on the same line
        // (Android: never pinned to the switch). When the last line has no
        // room for it, the label wraps a little narrower so it does.
        let text_w = (width - inset - SWITCH_SIZE.x - 16.0).max(24.0);
        let mut title = wrapped(ui, self.title, Type::RowLabel.font(), ink, text_w);
        if self.info.is_some() {
            let last = title.rows.last().map_or(0.0, |row| row.rect().right());
            if last + INFO_GAP + INFO_SIZE > text_w {
                title = wrapped(ui, self.title, Type::RowLabel.font(), ink, text_w - INFO_GAP - INFO_SIZE);
            }
        }
        let sub = self.subtitle.map(|s| wrapped(ui, s, Type::Subtitle.font(), dim, text_w));
        let block_h = title.size().y + sub.as_ref().map_or(0.0, |g| g.size().y + theme::LINE_GAP);
        let floor = if sub.is_some() { theme::ROW_HEIGHT_SUBTITLE } else { theme::ROW_HEIGHT };
        let height = (block_h + 2.0 * theme::ROW_PAD).max(floor);
        let (rect, mut response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
        let in_card = card_row(rect.top(), rect.left() + inset);
        let info_id = response.id.with("info-open");
        let enabled = response.enabled();
        if response.clicked() {
            *value = !*value;
            response.mark_changed();
        }
        let on = *value;
        response.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, enabled, on, self.title));
        let painter = ui.painter().clone();
        let wash = rect.expand2(vec2(8.0, 0.0));
        if enabled && response.hovered() {
            painter.rect_filled(wash, radius(10.0), theme::HOVER);
        }
        let switch = Rect::from_min_size(
            egui::pos2(rect.right() - SWITCH_SIZE.x, rect.center().y - SWITCH_SIZE.y * 0.5),
            SWITCH_SIZE,
        );
        paint_switch(&painter, switch, on, enabled, response.hovered());
        let text_x = rect.left() + inset;
        if let Some(icon) = self.icon {
            let disc = Rect::from_center_size(egui::pos2(rect.left() + theme::AVATAR * 0.5, rect.center().y), Vec2::splat(theme::AVATAR));
            paint_leading(&painter, disc, &Leading::Icon(icon), enabled);
        }
        let y = rect.center().y - block_h * 0.5;
        let title_h = title.size().y;
        let last_row = title.rows.last().map(|row| row.rect());
        painter.galley(egui::pos2(text_x, y), title, ink);
        if let Some(sub) = sub {
            painter.galley(egui::pos2(text_x, y + title_h + theme::LINE_GAP), sub, dim);
        }
        let mut open = ui.data(|d| d.get_temp::<bool>(info_id).unwrap_or(false));
        if let (Some(_info), Some(last)) = (self.info, last_row) {
            let center = egui::pos2(text_x + last.right() + INFO_GAP + INFO_SIZE * 0.5, y + last.center().y);
            let r = Rect::from_center_size(center, Vec2::splat(INFO_SIZE));
            let info = ui.interact(r, response.id.with("info"), Sense::click());
            let label = format!("About {}", self.title);
            info.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &label));
            // Quiet until wanted: dim, brighter while the row or the button is hovered.
            if info.hovered() {
                painter.circle_filled(r.center(), INFO_SIZE * 0.5, theme::RAISED);
            }
            let tint = if info.hovered() || open {
                theme::TEXT
            } else if response.hovered() || info.has_focus() {
                theme::TEXT_SECONDARY
            } else {
                theme::TEXT_DISABLED
            };
            draw_icon(&painter, r.shrink(7.0), Icon::Info, tint);
            focus_ring(ui, &info, r, INFO_SIZE * 0.5);
            if info.clicked() {
                open = !open;
                ui.data_mut(|d| d.insert_temp(info_id, open));
            }
        }
        focus_ring(ui, &response, wash, 10.0);
        if let (true, Some(info)) = (open, self.info) {
            ui.add_space(-4.0);
            ui.horizontal(|ui| {
                ui.add_space(inset);
                ui.set_max_width(width - inset - SWITCH_SIZE.x - 16.0);
                ui.label(theme::secondary(info));
            });
            ui.add_space(8.0);
        }
        if self.divider && !in_card {
            divider(ui, inset);
        }
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    }
}

/// A quiet line with an (i) right after its last word; the (i) opens `info`
/// under it (Android: the Calls page's "You appear as ..." line).
pub fn line_with_info(ui: &mut Ui, id: &str, text: &str, a11y: &str, info: &str) {
    let width = ui.available_width();
    let base = ui.id().with(id);
    let open_id = base.with("open");
    let mut open = ui.data(|d| d.get_temp::<bool>(open_id).unwrap_or(false));
    let font = Type::Subtitle.font();
    let mut galley = wrapped(ui, text, font.clone(), theme::TEXT_SECONDARY, width);
    if galley.rows.last().map_or(0.0, |row| row.rect().right()) + INFO_GAP + INFO_SIZE > width {
        galley = wrapped(ui, text, font, theme::TEXT_SECONDARY, width - INFO_GAP - INFO_SIZE);
    }
    let height = galley.size().y.max(INFO_SIZE);
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, text));
    let y = rect.top() + (height - galley.size().y) * 0.5;
    let last = galley.rows.last().map(|row| row.rect());
    ui.painter().galley(egui::pos2(rect.left(), y), galley, theme::TEXT_SECONDARY);
    if let Some(last) = last {
        let r = Rect::from_center_size(egui::pos2(rect.left() + last.right() + INFO_GAP + INFO_SIZE * 0.5, y + last.center().y), Vec2::splat(INFO_SIZE));
        let button = ui.interact(r, base.with("info"), Sense::click());
        button.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, a11y));
        if button.hovered() {
            ui.painter().circle_filled(r.center(), INFO_SIZE * 0.5, theme::RAISED);
        }
        let tint = if button.hovered() || open { theme::TEXT } else { theme::TEXT_SECONDARY };
        draw_icon(ui.painter(), r.shrink(7.0), Icon::Info, tint);
        focus_ring(ui, &button, r, INFO_SIZE * 0.5);
        if button.clicked() {
            open = !open;
            ui.data_mut(|d| d.insert_temp(open_id, open));
        }
        let _ = button.on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    if open {
        ui.add_space(4.0);
        ui.scope(|ui| {
            ui.set_max_width(width.min(theme::READABLE_WIDTH));
            ui.label(theme::secondary(info));
        });
    }
}

// ------------------------------------------------------ chips, badges, misc

/// The meaning of a status chip's dot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Everything is fine: Android's green dot ("Ready").
    Ok,
    /// Works, but not as you may expect: a white dot and a word ("Hidden").
    Attention,
    /// One problem to fix: a white warning glyph ("Bluetooth off").
    Problem,
    /// On air: a red dot.
    Live,
    /// Verified: a green check.
    Verified,
}

/// A small status indicator: dot/glyph and a word, no box. Clickable (opens
/// the status sheet); hover shows a raised pill.
pub fn status_chip(ui: &mut Ui, text: &str, tone: Tone) -> Response {
    let font = egui::FontId::new(14.0, theme::semibold_family());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, theme::TEXT);
    let size = vec2(galley.size().x + 12.0 + 8.0 + 24.0, 32.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let enabled = response.enabled();
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, text));
    if response.hovered() {
        ui.painter().rect_filled(rect, radius(16.0), theme::RAISED);
    }
    let mark = egui::pos2(rect.left() + 12.0 + 6.0, rect.center().y);
    match tone {
        Tone::Ok => {
            ui.painter().circle_filled(mark, 4.5, theme::POSITIVE);
        }
        Tone::Attention => {
            ui.painter().circle_filled(mark, 4.5, theme::TEXT);
        }
        Tone::Live => {
            ui.painter().circle_filled(mark, 5.0, theme::LIVE);
        }
        Tone::Problem => draw_icon(ui.painter(), Rect::from_center_size(mark, Vec2::splat(14.0)), Icon::Warning, theme::TEXT),
        Tone::Verified => draw_icon(ui.painter(), Rect::from_center_size(mark, Vec2::splat(14.0)), Icon::Verified, theme::POSITIVE),
    }
    let ink = if tone == Tone::Ok { theme::TEXT_SECONDARY } else { theme::TEXT };
    ui.painter().galley_with_override_text_color(egui::pos2(mark.x + 6.0 + 8.0, rect.center().y - galley.size().y * 0.5), galley, ink);
    focus_ring(ui, &response, rect, 16.0);
    response
}

/// The red LIVE badge: white dot and text on a red capsule.
pub fn live_badge(ui: &mut Ui, text: &str) -> Response {
    let font = egui::FontId::new(12.0, theme::bold_family());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, Color32::WHITE);
    let size = vec2(galley.size().x + 10.0 + 6.0 + 6.0 + 10.0, 24.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, text));
    ui.painter().rect_filled(rect, radius(12.0), theme::LIVE);
    let dot = egui::pos2(rect.left() + 10.0 + 3.0, rect.center().y);
    ui.painter().circle_filled(dot, 3.0, Color32::WHITE);
    ui.painter().galley(egui::pos2(dot.x + 3.0 + 6.0, rect.center().y - galley.size().y * 0.5), galley, Color32::WHITE);
    response
}

/// A neutral capsule tag (rare: "Beta", a count).
pub fn tag(ui: &mut Ui, text: &str) -> Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), Type::Caption.font(), theme::TEXT);
    let size = vec2(galley.size().x + 16.0, 22.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, text));
    ui.painter().rect_filled(rect, radius(11.0), theme::RAISED);
    ui.painter().galley(rect.center() - galley.size() * 0.5, galley, theme::TEXT);
    response
}

/// Empty state: a grey glyph, a title and one line, centred.
pub fn empty_state(ui: &mut Ui, icon: Icon, title: &str, subtitle: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
        draw_icon(ui.painter(), rect, icon, theme::TEXT_SECONDARY);
        ui.add_space(14.0);
        ui.label(theme::headline(title));
        if !subtitle.is_empty() {
            ui.add_space(2.0);
            ui.label(theme::secondary(subtitle));
        }
    });
    ui.add_space(16.0);
}

/// A problem line (Bluetooth off, permissions): warning glyph, the text
/// wrapped to the column, and at most one action under it. No card.
/// Returns true when the action is used.
pub fn notice(ui: &mut Ui, text: &str, action: Option<&str>) -> bool {
    notice_actions(ui, text, action.as_slice()).is_some()
}

/// A notice with up to a few actions (the first is the main one, white).
/// Returns the index of the action used. Never wider than the column.
pub fn notice_actions(ui: &mut Ui, text: &str, actions: &[&str]) -> Option<usize> {
    let mut used = None;
    let width = ui.available_width();
    let indent = theme::ICON + 12.0;
    ui.allocate_ui_with_layout(vec2(width, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.set_max_width(width);
        let top = ui.cursor().min;
        let (rect, _) = ui.allocate_exact_size(vec2(theme::ICON, 0.0), Sense::hover());
        let glyph = Rect::from_center_size(egui::pos2(rect.center().x, top.y + 11.0), Vec2::splat(18.0));
        draw_icon(ui.painter(), glyph, Icon::Warning, theme::TEXT);
        let text_rect = Rect::from_min_size(egui::pos2(top.x + indent, top.y), vec2((width - indent).max(40.0), f32::INFINITY));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(text_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        child.add(egui::Label::new(theme::body(text)).wrap());
        if !actions.is_empty() {
            child.add_space(4.0);
            child.horizontal_wrapped(|ui| {
                for (index, action) in actions.iter().enumerate() {
                    let kind = if index == 0 { Kind::Primary } else { Kind::Secondary };
                    if Pill::new(action).kind(kind).small().show(ui).clicked() {
                        used = Some(index);
                    }
                }
            });
        }
        let used_rect = child.min_rect();
        ui.advance_cursor_after_rect(Rect::from_min_max(top, egui::pos2(top.x + width, used_rect.bottom())));
    });
    used
}

// ------------------------------------------------------------- navigation

/// The title row of a screen: optional back button, title, and trailing
/// controls (status chip, actions) at the right. Returns true when Back was
/// used (button, Escape is left to the screen).
pub fn top_bar(ui: &mut Ui, title: &str, back: Option<&str>, trailing: impl FnOnce(&mut Ui)) -> bool {
    let mut went_back = false;
    let width = ui.available_width();
    ui.allocate_ui_with_layout(vec2(width, theme::TOP_BAR_HEIGHT), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(vec2(width, theme::TOP_BAR_HEIGHT));
        if let Some(name) = back {
            went_back = icon_button(ui, Icon::Back, name, Kind::Quiet).clicked();
            ui.add_space(4.0);
        }
        let font = if back.is_some() { Type::Headline } else { Type::Title };
        let room = (ui.available_width() - 160.0).max(80.0);
        let galley = single_line(ui, title, font.font(), theme::TEXT, room);
        let (rect, response) = ui.allocate_exact_size(galley.size(), Sense::hover());
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, title));
        ui.painter().galley(rect.min, galley, theme::TEXT);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), trailing);
    });
    went_back
}

/// A segmented control ("Nearby | Recent"). Returns true when it changed.
pub fn segmented(ui: &mut Ui, options: &[&str], selected: &mut usize) -> bool {
    let font = egui::FontId::new(14.0, theme::semibold_family());
    let galleys: Vec<_> = options.iter().map(|o| ui.painter().layout_no_wrap((*o).to_owned(), font.clone(), theme::TEXT)).collect();
    let seg_w = galleys.iter().map(|g| g.size().x).fold(0.0, f32::max) + 36.0;
    let height = 36.0;
    let (track, _) = ui.allocate_exact_size(vec2(seg_w * options.len() as f32 + 6.0, height), Sense::hover());
    ui.painter().rect_filled(track, radius(height * 0.5), theme::RAISED);
    let mut changed = false;
    for (index, (label, galley)) in options.iter().zip(galleys).enumerate() {
        let rect = Rect::from_min_size(egui::pos2(track.left() + 3.0 + seg_w * index as f32, track.top() + 3.0), vec2(seg_w, height - 6.0));
        let response = ui.interact(rect, ui.id().with(("segment", label)), Sense::click());
        let on = *selected == index;
        let enabled = response.enabled();
        response.widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, enabled, on, *label));
        if response.clicked() && !on {
            *selected = index;
            changed = true;
        }
        let on = *selected == index;
        let (fill, _) = segment_colors(on, response.hovered());
        ui.painter().rect_filled(rect, radius(rect.height() * 0.5), fill);
        let (_, ink) = segment_colors(on, response.hovered());
        ui.painter().galley_with_override_text_color(rect.center() - galley.size() * 0.5, galley, ink);
        focus_ring(ui, &response, rect, rect.height() * 0.5);
    }
    changed
}

/// (fill, text) of one segment: the selected one is a white pill with black text.
pub fn segment_colors(selected: bool, hovered: bool) -> (Color32, Color32) {
    match (selected, hovered) {
        (true, _) => (theme::TEXT, Color32::BLACK),
        (false, true) => (theme::RAISED_HOVER, theme::TEXT),
        (false, false) => (Color32::TRANSPARENT, theme::TEXT_SECONDARY),
    }
}

/// One item of the navigation rail: icon and label; the selected item has a
/// raised pill. `compact` shows the icon only (the label is in the tooltip).
pub fn nav_item(ui: &mut Ui, label: &str, icon: Icon, selected: bool, compact: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), theme::NAV_ITEM_HEIGHT), Sense::click());
    let enabled = response.enabled();
    response.widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, enabled, selected, label));
    let fill = if selected {
        Some(theme::RAISED)
    } else if response.hovered() && enabled {
        Some(theme::HOVER)
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, radius(10.0), fill);
    }
    let ink = if !enabled {
        theme::TEXT_DISABLED
    } else if selected {
        theme::TEXT
    } else {
        theme::TEXT_SECONDARY
    };
    let icon_center = if compact { rect.center() } else { egui::pos2(rect.left() + 22.0, rect.center().y) };
    draw_icon(ui.painter(), Rect::from_center_size(icon_center, Vec2::splat(theme::ICON)), icon, ink);
    if !compact {
        let font = egui::FontId::new(15.0, if selected { theme::semibold_family() } else { egui::FontFamily::Proportional });
        ui.painter().text(egui::pos2(rect.left() + 46.0, rect.center().y), Align2::LEFT_CENTER, label, font, ink);
    }
    focus_ring(ui, &response, rect, 10.0);
    if compact { response.on_hover_text(label) } else { response }
}

// ------------------------------------------------------------- text fields

/// A filled, rounded single-line text field with an optional label above.
/// Screen readers and voice input can set its value (UI Automation SetValue).
pub fn text_field(ui: &mut Ui, text: &mut String, label: Option<&str>, hint: &str, limit: Option<usize>) -> Response {
    let label_id = label.map(|label| ui.label(theme::text(Type::Subtitle, label)).id);
    let mut edit = egui::TextEdit::singleline(text)
        .hint_text(theme::text(Type::Body, hint).color(theme::TEXT_DISABLED))
        .font(Type::Body.font())
        .frame(false)
        .margin(egui::Margin::symmetric(16, 17))
        .desired_width(f32::INFINITY);
    if let Some(n) = limit {
        edit = edit.char_limit(n);
    }
    let mut prepared = egui::Frame::new().fill(theme::RAISED).corner_radius(theme::FIELD_RADIUS).begin(ui);
    let mut response = prepared.content_ui.add(edit);
    let focused = response.has_focus();
    prepared.frame.stroke = if focused { Stroke::new(1.5, theme::TEXT) } else { Stroke::NONE };
    prepared.end(ui);
    if let Some(id) = label_id {
        response = response.labelled_by(id);
    } else if !hint.is_empty() {
        let hint = hint.to_owned();
        let value = text.clone();
        response.widget_info(|| WidgetInfo::text_edit(true, "", &value, &hint));
    }
    super::accessible_set_value(ui, &response, text, limit);
    response
}

// ----------------------------------------------------------------- dialogs

/// A modal dialog: dark sheet, rounded, dimmed backdrop, at most
/// `theme::DIALOG_WIDTH` wide (narrower on a small window).
pub fn dialog<R>(ctx: &egui::Context, id: &str, content: impl FnOnce(&mut Ui) -> R) -> egui::ModalResponse<R> {
    let width = (ctx.content_rect().width() - 80.0).clamp(200.0, theme::DIALOG_WIDTH);
    egui::Modal::new(egui::Id::new(id))
        .backdrop_color(theme::SCRIM)
        .frame(
            egui::Frame::new()
                .fill(theme::SURFACE)
                .stroke(Stroke::new(1.0, theme::DIVIDER))
                .corner_radius(theme::SHEET_RADIUS)
                .inner_margin(egui::Margin::same(24))
                .shadow(egui::Shadow { offset: [0, 16], blur: 48, spread: 0, color: Color32::from_black_alpha(200) }),
        )
        .show(ctx, |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 10.0;
            // A dialog taller than the window (a short window or large text)
            // scrolls instead of pushing its buttons off screen; one that
            // fits is laid out plainly.
            let max_height = (ctx.content_rect().height() - 96.0).max(120.0);
            let height_id = egui::Id::new(id).with("content-height");
            let last = ctx.data(|d| d.get_temp::<f32>(height_id).unwrap_or(0.0));
            let (inner, height) = if last > max_height {
                egui::ScrollArea::vertical()
                    .max_height(max_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.set_width(width);
                        let inner = content(ui);
                        (inner, ui.min_rect().height())
                    })
                    .inner
            } else {
                let top = ui.min_rect().top();
                let inner = content(ui);
                (inner, ui.min_rect().bottom() - top)
            };
            ctx.data_mut(|d| d.insert_temp(height_id, height));
            inner
        })
}

/// A dialog's title.
pub fn dialog_title(ui: &mut Ui, text: &str) {
    ui.label(theme::headline(text));
    ui.add_space(2.0);
}
/// A dialog's explanation (secondary, wraps).
pub fn dialog_text(ui: &mut Ui, text: &str) {
    ui.label(theme::text(Type::Body, text).color(theme::TEXT_SECONDARY));
}

/// The dialog's button row as Android's dialogs (ThemeOverlay.App.Dialog:
/// Widget.App.Button.Text, right-aligned, Cancel before the main action).
/// `kind` keeps the main action's meaning for screen readers and tests; the
/// look is the text button. Returns (main clicked, cancel clicked). Escape
/// cancels.
pub fn dialog_buttons(ui: &mut Ui, main: &str, kind: Kind, enabled: bool, cancel: Option<&str>) -> (bool, bool) {
    let _ = kind;
    ui.add_space(8.0);
    let mut out = (false, false);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        out.0 = Pill::new(main).kind(Kind::Quiet).min_width(64.0).height(48.0).enabled(enabled).show(ui).clicked();
        if let Some(cancel) = cancel {
            out.1 = Pill::new(cancel).kind(Kind::Quiet).min_width(64.0).height(48.0).show(ui).clicked();
        }
    });
    if cancel.is_some() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        out.1 = true;
    }
    out
}

/// The list pane's width in a list + detail split.
pub fn split_width(available: f32) -> f32 {
    (available * 0.36).clamp(340.0, 560.0)
}

/// Two panes side by side, as desktop apps use a wide window: the list
/// (`list_width`) on the left, a quiet divider, the detail on the right.
/// `content` gets both panes.
pub fn split(ui: &mut Ui, list_width: f32, content: impl FnOnce(&mut Ui, &mut Ui)) {
    let full = ui.available_rect_before_wrap();
    let gap = 32.0;
    let top = full.top();
    let bottom = if full.bottom().is_finite() { full.bottom() } else { top + 100_000.0 };
    let left = Rect::from_min_max(full.min, egui::pos2(full.left() + list_width, bottom));
    let right = Rect::from_min_max(egui::pos2(left.right() + gap, top), egui::pos2(full.right(), bottom));
    let mut list = ui.new_child(egui::UiBuilder::new().max_rect(left).layout(egui::Layout::top_down(egui::Align::Min)));
    let mut detail = ui.new_child(egui::UiBuilder::new().max_rect(right).layout(egui::Layout::top_down(egui::Align::Min)));
    content(&mut list, &mut detail);
    let height = list.min_rect().height().max(detail.min_rect().height()).max(1.0);
    let x = left.right() + gap * 0.5;
    ui.painter().vline(x, top..=top + height, theme::divider_stroke());
    ui.advance_cursor_after_rect(Rect::from_min_max(full.min, egui::pos2(full.right(), top + height)));
}

/// Centres a content column sized by `theme::content_width` (it grows with the window).
pub fn content_column<R>(ui: &mut Ui, content: impl FnOnce(&mut Ui) -> R) -> R {
    let full = ui.available_width();
    let width = theme::content_width(full);
    let side = ((full - width) * 0.5).max(0.0);
    ui.horizontal_top(|ui| {
        ui.add_space(side);
        ui.vertical(|ui| {
            ui.set_width(width);
            content(ui)
        })
        .inner
    })
    .inner
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_content_column_grows_with_the_window_and_stays_centred() {
        // One column: it takes the whole width up to a generous cap, and is
        // centred once the cap is reached.
        let mut last = 0.0;
        for width in [700.0f32, 1000.0, 1366.0, 1640.0, 2240.0, 3600.0] {
            let column = theme::content_width(width);
            assert!(column >= last && column <= width, "{width}: {column}");
            assert!(column >= width.min(theme::CONTENT_MAX_WIDTH), "{width}: rows stretch with the window");
            last = column;
            let mut rect = Rect::NOTHING;
            let _ = frame(width, |ui| {
                rect = content_column(ui, |ui| ui.allocate_space(vec2(ui.available_width(), 10.0)).1);
            });
            assert!((rect.width() - column).abs() < 1.0, "{width}: {rect:?}");
            assert!((rect.left() - (width - rect.right())).abs() < 1.0, "{width}: centred {rect:?}");
        }
    }
    use super::*;

    fn frame(width: f32, mut content: impl FnMut(&mut Ui)) -> egui::FullOutput {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(width, 600.0))),
            ..Default::default()
        };
        let _ = ctx.run(input(), |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| content(ui));
        });
        ctx.run(input(), |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| content(ui));
        })
    }

    /// Horizontal divider lines painted (their y), in paint order.
    fn divider_ys(output: &egui::FullOutput) -> Vec<f32> {
        output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::LineSegment { points, stroke } if stroke.color == theme::DIVIDER && points[0].y == points[1].y => Some(points[0].y),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn dividers_land_on_whole_screen_pixels_at_every_scale() {
        for ppp in [1.0f32, 1.25, 1.5, 1.75, 2.0, 2.5] {
            for y in [10.0f32, 10.3, 57.5, 113.8] {
                let (line, width) = divider_geometry(y, ppp);
                let physical_width = width * ppp;
                assert!((physical_width - physical_width.round()).abs() < 1e-4 && physical_width >= 1.0, "{ppp} {y}");
                // The line's top and bottom edges fall on pixel boundaries.
                let top = (line - width / 2.0) * ppp;
                assert!((top - top.round()).abs() < 1e-3, "{ppp} {y}: top {top}");
                assert!((line - y).abs() <= 1.0, "stays at the row edge");
            }
        }
    }

    /// Settings (Android SettingsCard): every row-to-row boundary has its
    /// divider, also between two rows that carry the (i) button.
    #[test]
    fn two_info_rows_in_a_settings_card_have_a_divider_between_them() {
        let (mut a, mut b) = (true, false);
        let mut rows = (Rect::NOTHING, Rect::NOTHING);
        let output = frame(600.0, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            settings_card(ui, |ui| {
                rows.0 = ToggleRow::new("First").info("About the first").divider(true).show(ui, &mut a).rect;
                rows.1 = ToggleRow::new("Second").info("About the second").show(ui, &mut b).rect;
            });
        });
        let ys = divider_ys(&output);
        assert!(
            ys.iter().any(|y| *y >= rows.0.bottom() - 1.0 && *y <= rows.1.top() + 1.0),
            "a divider between {:?} and {:?}; dividers at {ys:?}",
            rows.0,
            rows.1
        );
        // And it is not hidden under the second row's fill.
        let covering = output.shapes.iter().any(|clipped| match &clipped.shape {
            egui::Shape::Rect(r) => r.fill != Color32::TRANSPARENT && r.fill != theme::SURFACE && ys.iter().any(|y| r.rect.top() < *y && r.rect.bottom() > *y),
            _ => false,
        });
        assert!(!covering, "nothing paints over the divider");
    }

    /// The colour a text shape really paints with.
    fn painted_color(output: &egui::FullOutput, text: &str) -> Color32 {
        output
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Text(t) if t.galley.text() == text => {
                    Some(t.override_text_color.unwrap_or(t.galley.job.sections[0].format.color))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("{text:?} was not painted"))
    }

    #[test]
    fn the_selected_segment_label_is_painted_in_a_colour_that_differs_from_its_pill() {
        for selected in [0usize, 1] {
            let output = frame(600.0, |ui| {
                let mut index = selected;
                segmented(ui, &["Nearby", "Recent"], &mut index);
            });
            let (on_label, off_label) = if selected == 0 { ("Nearby", "Recent") } else { ("Recent", "Nearby") };
            let (pill, ink) = segment_colors(true, false);
            assert_eq!(painted_color(&output, on_label), ink, "selected label uses the selected ink");
            assert_ne!(ink, pill, "selected label is visible on its pill");
            assert_ne!(painted_color(&output, off_label), theme::RAISED, "unselected label is visible on the track");
        }
    }

    #[test]
    fn a_long_notice_wraps_inside_a_narrow_scrolling_column_and_keeps_its_action_on_screen() {
        let width = 320.0;
        let long = "This PC's Bluetooth adapter can't advertise, so phones can't connect to it. Calling a phone still works. A Bluetooth 5 USB adapter that supports peripheral mode fixes this.";
        let mut used = Rect::NOTHING;
        let mut after = Rect::NOTHING;
        let _ = frame(width, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let start = ui.min_rect();
                notice(ui, long, Some("Open Bluetooth settings"));
                used = ui.min_rect().union(start);
                ui.horizontal(|ui| {
                    after = Pill::new("Video call").show(ui).rect;
                });
            });
        });
        assert!(used.width() <= width + 0.5, "the notice stays in the column ({} > {width})", used.width());
        assert!(used.height() > 60.0, "the text wrapped onto more lines");
        assert!(after.right() <= width + 0.5, "the next row's controls stay on screen");
    }
}
