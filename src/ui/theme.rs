//! MeshLive design tokens for Windows: the same monochrome dark system as
//! Android and iOS (design/ui-spec.md, Design/design-tokens.json), adapted to a
//! desktop window: Segoe UI Variable, mouse-and-keyboard density, a visible
//! keyboard focus ring and DPI scaling (all sizes are logical points).
//!
//! Colour rule: pure black ground, white text, grey secondary. Red only for
//! live, recording, hang up, decline and destructive actions; green only for
//! answer and verified. Nothing else is coloured.
#![allow(dead_code)] // The shared API: screens use a growing subset.
use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId, RichText, Stroke};

// ------------------------------------------------------------------ colour

/// Window ground.
pub const BG: Color32 = Color32::BLACK;
/// Sheets, dialogs, text fields.
pub const SURFACE: Color32 = Color32::from_rgb(0x12, 0x12, 0x12);
/// Secondary buttons, chips, call controls, avatars, the selected tab pill.
pub const RAISED: Color32 = Color32::from_rgb(0x1E, 0x1E, 0x1E);
/// Hover on a raised control.
pub const RAISED_HOVER: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x2A);
/// Hairline dividers.
pub const DIVIDER: Color32 = Color32::from_rgb(0x26, 0x26, 0x26);
/// Hover / pressed wash over plain rows on black.
pub const HOVER: Color32 = Color32::from_rgb(0x0F, 0x0F, 0x0F);
pub const PRESSED: Color32 = Color32::from_rgb(0x17, 0x17, 0x17);
pub const TEXT: Color32 = Color32::WHITE;
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(0xA3, 0xA3, 0xA3);
pub const TEXT_DISABLED: Color32 = Color32::from_rgb(0x5C, 0x5C, 0x5C);
/// Live, record, hang up, decline, destructive.
pub const LIVE: Color32 = Color32::from_rgb(0xFF, 0x3B, 0x30);
pub const LIVE_HOVER: Color32 = Color32::from_rgb(0xFF, 0x55, 0x4B);
/// Answer and the verified check only.
pub const POSITIVE: Color32 = Color32::from_rgb(0x34, 0xC7, 0x59);
pub const POSITIVE_HOVER: Color32 = Color32::from_rgb(0x4C, 0xD4, 0x6E);
/// Primary (white) button hover.
pub const PRIMARY_HOVER: Color32 = Color32::from_rgb(0xE6, 0xE6, 0xE6);
/// Switch track when off.
pub const TRACK_OFF: Color32 = Color32::from_rgb(0x3A, 0x3A, 0x3A);
/// Keyboard focus ring (Windows draws a visible 2 px ring on focus).
pub const FOCUS: Color32 = Color32::WHITE;
/// Dim scrim behind dialogs.
pub const SCRIM: Color32 = Color32::from_black_alpha(190);

// ------------------------------------------------------------------- shape

/// Screen side padding (narrow / normal / large window).
pub const GUTTER_COMPACT: f32 = 20.0;
pub const GUTTER: f32 = 32.0;
pub const GUTTER_LARGE: f32 = 48.0;
/// From this window width the gutters grow to `GUTTER_LARGE`.
pub const LARGE_FROM: f32 = 1600.0;

/// Side padding for a window `width` points wide.
pub fn gutter(width: f32) -> f32 {
    if width < COMPACT_BELOW {
        GUTTER_COMPACT
    } else if width < LARGE_FROM {
        GUTTER
    } else {
        GUTTER_LARGE
    }
}
/// Content column on wide windows: it grows with the window (a maximised or
/// full-screen window uses the space) but stops before rows read badly.
/// Only very wide monitors reach this cap; the column is centred there.
pub const CONTENT_MAX_WIDTH: f32 = 1520.0;

/// Column width for `available` points (inside the gutters): one column
/// that stretches with the window, as Windows Settings does, capped only on
/// very wide monitors. Bigger windows mean wider rows, not more panes (user
/// feedback 2026-10-21); running text keeps a readable line length
/// (`READABLE_WIDTH`).
pub fn content_width(available: f32) -> f32 {
    available.min(CONTENT_MAX_WIDTH)
}
/// Experimental split panes (`Ui::split_panes`) start at this width.
pub const WIDE_SPLIT: f32 = 1000.0;
/// Paragraphs wrap at about this width (roughly 80 characters).
pub const READABLE_WIDTH: f32 = 680.0;
/// Gap between sections.
pub const SECTION_GAP: f32 = 24.0;
/// Plain list row heights (title only / title + subtitle).
pub const ROW_HEIGHT: f32 = 56.0;
pub const ROW_HEIGHT_SUBTITLE: f32 = 72.0;
/// Space above and below a row's text when it wraps or has two lines.
pub const ROW_PAD: f32 = 14.0;
/// Space between a row's title and its second line.
pub const LINE_GAP: f32 = 4.0;
/// Leading avatar / icon disc in a row.
pub const AVATAR: f32 = 40.0;
/// Text inset after the leading disc (divider inset uses the same).
pub const ROW_TEXT_INSET: f32 = AVATAR + 14.0;
/// Glyph size for icons in rows, nav and buttons.
pub const ICON: f32 = 20.0;
pub const ICON_SMALL: f32 = 16.0;
/// Pill buttons: regular, large (the pinned Go live / Add contact), small.
pub const BUTTON_HEIGHT: f32 = 56.0;
pub const BUTTON_HEIGHT_LARGE: f32 = 56.0;
pub const BUTTON_HEIGHT_SMALL: f32 = 40.0;
/// Round trailing icon buttons in rows.
pub const ICON_BUTTON: f32 = 44.0;
/// Call controls (round, label under) and accept/decline.
pub const ROUND_CONTROL: f32 = 64.0;
pub const ROUND_CONTROL_LARGE: f32 = 72.0;
/// Sheets and dialogs.
pub const SHEET_RADIUS: u8 = 24;
/// Text fields.
pub const FIELD_RADIUS: u8 = 16;
pub const FIELD_HEIGHT: f32 = 56.0;
/// Top bar height (title row of every screen).
pub const TOP_BAR_HEIGHT: f32 = 56.0;
/// Navigation rail.
pub const NAV_WIDTH: f32 = 220.0;
pub const NAV_WIDTH_COMPACT: f32 = 72.0;
pub const NAV_ITEM_HEIGHT: f32 = 40.0;
/// Window width below which the rail collapses to icons and gutters narrow.
pub const COMPACT_BELOW: f32 = 760.0;
/// Dialog width cap.
pub const DIALOG_WIDTH: f32 = 420.0;

pub fn focus_stroke() -> Stroke {
    Stroke::new(2.0, FOCUS)
}
pub fn divider_stroke() -> Stroke {
    Stroke::new(1.0, DIVIDER)
}

// -------------------------------------------------------------------- type

/// Font families: body text is Segoe UI Variable (Segoe UI before Windows 11);
/// semibold and bold are separate Segoe UI faces (egui cannot pick a weight
/// from a variable font). Bundled Noto Sans covers any missing glyph.
pub const SEMIBOLD: &str = "Semibold";
pub const MEDIUM: &str = "Medium";
pub const BOLD: &str = "Bold";
/// Older name kept for screens that ask for the heading face.
pub const HEADING: &str = "Heading";

pub fn semibold_family() -> FontFamily {
    FontFamily::Name(SEMIBOLD.into())
}
pub fn bold_family() -> FontFamily {
    FontFamily::Name(BOLD.into())
}
pub fn medium_family() -> FontFamily {
    FontFamily::Name(MEDIUM.into())
}

/// Android "Plain Signal" (app/src/main/res/values/themes.xml) as Windows
/// components, 1 dp = 1 px at 100 % scale:
///
/// | Android style | Windows |
/// | --- | --- |
/// | Widget.App.Button.Primary (56, radius 28, white, 17 semibold) | `Pill` `Kind::Primary` |
/// | Widget.App.Button.Secondary (56, #1E1E1E, white text) | `Pill` `Kind::Secondary` |
/// | Widget.App.Button.Live (56, #FF3B30, 17 bold, icon 24 + 10) | `Pill` `Kind::Live` |
/// | Live with backgroundTint danger (Emergency wipe) | `Pill` `Kind::Destructive` |
/// | Widget.App.Button.Text (48, text only) | `Pill` `Kind::Quiet`, dialog buttons |
/// | iconTint on a button (Go live: live_red on white) | `Pill::icon_tint` |
/// | Widget.App.RoundControl / .Large (64 / 72 circles) | `round_button_sized` |
/// | Widget.App.IconCircle / .Filled (44, raised / white) | `icon_button` Secondary / Primary |
/// | Widget.App.EditText (56, radius 16, #121212) | `text_field` |
/// | Widget.App.SettingsCard (#121212, radius 16, 16/12 padding) | `settings_card` |
/// | Widget.App.SectionLabel (13 regular, secondary) | `settings_label` |
/// | Widget.App.SectionHeader (15 semibold, secondary) | `section_header` |
/// | ThemeOverlay.App.Dialog (radius 24, #121212) | `dialog` |
/// | Widget.App.LiveBadge (12 bold, red pill) | `live_badge` |
/// | Typography (28/20/17/16/15/14/13/12, Manrope) | `Type` |
pub const ANDROID_STYLE_MAP: () = ();

/// One role per text use, as the iOS `Typo` and the spec's type scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    /// Screen title (28 bold).
    Title,
    /// Caller / stream name on the call and live screens (28 bold).
    CallName,
    /// Dialog titles and readouts (Android Readout: 20 bold).
    Headline,
    /// List row titles (Android Title: 17 semibold).
    RowTitle,
    /// Settings row labels (Android RowLabel: 16 medium).
    RowLabel,
    /// List row subtitles and explanations (Android Body: 14, secondary).
    Subtitle,
    /// Running text (Android BodyLarge: 16).
    Body,
    /// Buttons (Android Button: 17 semibold).
    Button,
    /// Section headers above a list (Android Section: 15 semibold, secondary).
    Section,
    /// Settings group labels (Android SectionLabel: 13, secondary).
    SectionLabel,
    /// Labels under round controls and chips (Android Label: 13 medium).
    Caption,
    /// Call / live timers (Android Readout: 20 bold, tabular figures).
    Timer,
}

impl Type {
    pub fn font(self) -> FontId {
        let (size, family) = match self {
            Type::Title => (28.0, bold_family()),
            Type::CallName => (28.0, bold_family()),
            Type::Headline => (20.0, bold_family()),
            Type::RowTitle => (17.0, semibold_family()),
            Type::RowLabel => (16.0, medium_family()),
            Type::Subtitle => (14.0, FontFamily::Proportional),
            Type::Body => (16.0, FontFamily::Proportional),
            Type::Button => (17.0, semibold_family()),
            Type::Section => (15.0, semibold_family()),
            Type::SectionLabel => (13.0, FontFamily::Proportional),
            Type::Caption => (13.0, medium_family()),
            Type::Timer => (20.0, bold_family()),
        };
        FontId::new(size, family)
    }
    /// The role's default colour.
    pub fn color(self) -> Color32 {
        match self {
            Type::Subtitle | Type::Section | Type::SectionLabel => TEXT_SECONDARY,
            _ => TEXT,
        }
    }
}

/// Text in a role, with the role's font and colour.
pub fn text(role: Type, s: impl Into<String>) -> RichText {
    RichText::new(s).font(role.font()).color(role.color())
}
pub fn title(s: impl Into<String>) -> RichText {
    text(Type::Title, s)
}
pub fn headline(s: impl Into<String>) -> RichText {
    text(Type::Headline, s)
}
pub fn body(s: impl Into<String>) -> RichText {
    text(Type::Body, s)
}
pub fn secondary(s: impl Into<String>) -> RichText {
    text(Type::Subtitle, s)
}
pub fn caption(s: impl Into<String>) -> RichText {
    text(Type::Caption, s).color(TEXT_SECONDARY)
}
pub fn strong(s: impl Into<String>) -> RichText {
    text(Type::RowTitle, s)
}

// ------------------------------------------------------------------- setup

fn windows_font(file: &str) -> Option<Vec<u8>> {
    let dir = std::env::var_os("WINDIR")?;
    std::fs::read(std::path::Path::new(&dir).join("Fonts").join(file)).ok()
}

fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    // egui's own emoji / symbol fallbacks stay at the end of every family.
    let fallbacks: Vec<String> = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    fonts.font_data.insert(
        "NotoSans".into(),
        FontData::from_static(include_bytes!("../../assets/fonts/NotoSans-Regular.ttf")).into(),
    );
    // Use the installed Windows faces, without redistributing OS font files.
    let mut load = |key: &str, files: &[&str]| -> Option<String> {
        files.iter().find_map(|file| windows_font(file)).map(|data| {
            fonts.font_data.insert(key.into(), FontData::from_owned(data).into());
            key.to_owned()
        })
    };
    let regular = load("SegoeRegular", &["SegUIVar.ttf", "segoeui.ttf"]);
    let semibold = load("SegoeSemibold", &["seguisb.ttf"]);
    let bold = load("SegoeBold", &["segoeuib.ttf", "seguisb.ttf"]);
    // Manrope, as the Android app (same files, SIL OFL 1.1; Latin, Greek and
    // Cyrillic): first in every family; other scripts fall back.
    let mut manrope = |key: &str, bytes: &'static [u8]| -> Option<String> {
        fonts.font_data.insert(key.into(), FontData::from_static(bytes).into());
        Some(key.to_owned())
    };
    let m_regular = manrope("ManropeRegular", include_bytes!("../../assets/fonts/Manrope-regular.ttf"));
    let m_medium = manrope("ManropeMedium", include_bytes!("../../assets/fonts/Manrope-medium.ttf"));
    let m_semibold = manrope("ManropeSemibold", include_bytes!("../../assets/fonts/Manrope-semibold.ttf"));
    let m_bold = manrope("ManropeBold", include_bytes!("../../assets/fonts/Manrope-bold.ttf"));
    let family = |first: &[&Option<String>]| -> Vec<String> {
        let mut list: Vec<String> = first.iter().filter_map(|f| (*f).clone()).collect();
        list.push("NotoSans".into());
        list.extend(fallbacks.iter().cloned());
        list.dedup();
        list
    };
    let body = family(&[&m_regular, &regular]);
    let medium = family(&[&m_medium, &regular]);
    let semi = family(&[&m_semibold, &semibold, &regular]);
    let heavy = family(&[&m_bold, &bold, &semibold, &regular]);
    fonts.families.insert(FontFamily::Proportional, body.clone());
    fonts.families.insert(FontFamily::Monospace, body);
    fonts.families.insert(medium_family(), medium);
    fonts.families.insert(semibold_family(), semi);
    fonts.families.insert(FontFamily::Name(HEADING.into()), heavy.clone());
    fonts.families.insert(bold_family(), heavy);
    fonts
}

/// Installs fonts, visuals and spacing. Dark always: the window never follows
/// a light system theme (the design has one look).
pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.options_mut(|options| options.theme_preference = egui::ThemePreference::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, visuals());
    ctx.style_mut_of(egui::Theme::Dark, style);
}

pub fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.dark_mode = true;
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.extreme_bg_color = RAISED; // text fields: visible on black
    v.faint_bg_color = SURFACE;
    v.code_bg_color = RAISED;
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = TEXT;
    v.warn_fg_color = TEXT;
    v.error_fg_color = LIVE;
    v.window_corner_radius = SHEET_RADIUS.into();
    v.menu_corner_radius = 12.into();
    v.window_stroke = Stroke::new(1.0, DIVIDER);
    v.window_shadow = egui::Shadow { offset: [0, 12], blur: 40, spread: 0, color: Color32::from_black_alpha(160) };
    v.popup_shadow = egui::Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(140) };
    v.selection.bg_fill = Color32::from_rgb(0x4A, 0x4A, 0x4A);
    v.selection.stroke = Stroke::new(1.0, TEXT);
    v.text_cursor.stroke = Stroke::new(2.0, TEXT);
    v.striped = false;
    v.indent_has_left_vline = false;
    v.collapsing_header_frame = false;
    v.button_frame = true;
    v.slider_trailing_fill = true;
    // Plain egui buttons are soft pills too (fields draw their own corners).
    let radius = egui::CornerRadius::same(255);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = BG;
    w.noninteractive.weak_bg_fill = BG;
    w.noninteractive.bg_stroke = Stroke::new(1.0, DIVIDER); // separators
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_SECONDARY);
    w.noninteractive.corner_radius = 8.into();
    for (state, fill) in [
        (&mut w.inactive, RAISED),
        (&mut w.hovered, RAISED_HOVER),
        (&mut w.active, Color32::from_rgb(0x36, 0x36, 0x36)),
        (&mut w.open, RAISED),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::NONE;
        state.fg_stroke = Stroke::new(1.5, TEXT);
        state.corner_radius = radius;
        state.expansion = 0.0;
    }
    // Keyboard focus shows as a ring (egui uses the hovered stroke for focus).
    w.hovered.bg_stroke = Stroke::NONE;
    v
}

pub fn style(style: &mut egui::Style) {
    use egui::TextStyle;
    style.text_styles = [
        (TextStyle::Heading, Type::Headline.font()),
        (TextStyle::Body, Type::Body.font()),
        (TextStyle::Button, Type::Button.font()),
        (TextStyle::Small, Type::Caption.font()),
        (TextStyle::Monospace, FontId::new(14.0, FontFamily::Monospace)),
    ]
    .into();
    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(12.0, 8.0);
    s.button_padding = egui::vec2(18.0, 10.0);
    s.interact_size = egui::vec2(40.0, 36.0);
    s.window_margin = egui::Margin::same(24);
    s.menu_margin = egui::Margin::same(8);
    s.indent = 16.0;
    s.icon_width = 18.0;
    s.icon_width_inner = 10.0;
    s.icon_spacing = 10.0;
    s.text_edit_width = 280.0;
    s.scroll = egui::style::ScrollStyle::floating();
    s.scroll.bar_width = 8.0;
    style.animation_time = 0.0;
    style.visuals = visuals();
}
