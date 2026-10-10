//! Colours, fonts and the handful of custom widgets the design uses.
//!
//! Everything else is stock egui, styled once in [`setup`].

use std::sync::Arc;

use eframe::egui::{
    self, Button, Color32, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily, FontId,
    Frame, Margin, Response, RichText, Sense, Stroke, TextStyle, Theme, Ui, Vec2,
};

pub const BG: Color32 = Color32::from_rgb(0x0F, 0x11, 0x13);
pub const PANEL: Color32 = Color32::from_rgb(0x15, 0x18, 0x1B);
pub const SURFACE: Color32 = Color32::from_rgb(0x1B, 0x1F, 0x23);
pub const BORDER: Color32 = Color32::from_rgb(0x2F, 0x35, 0x3C);
pub const TEXT: Color32 = Color32::from_rgb(0xEC, 0xEE, 0xF0);
pub const MUTED: Color32 = Color32::from_rgb(0xA3, 0xAC, 0xB6);
pub const ACCENT: Color32 = Color32::from_rgb(0x4C, 0xC9, 0xC0);
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x08, 0x20, 0x1E);
pub const DANGER: Color32 = Color32::from_rgb(0xD2, 0x45, 0x2A);
pub const DANGER_TEXT: Color32 = Color32::from_rgb(0xFF, 0x8A, 0x6E);
pub const WARN: Color32 = Color32::from_rgb(0xF2, 0xB8, 0x4B);
pub const OK: Color32 = Color32::from_rgb(0x5F, 0xD3, 0x9B);
pub const BLUE: Color32 = Color32::from_rgb(0x6E, 0xA8, 0xFF);

/// Buttons, chips, bars and inputs.
pub const SMALL_R: u8 = 4;
/// Cards, notices and dialogs.
pub const LARGE_R: u8 = 8;

// Symbols from egui's bundled NotoEmoji and emoji-icon fonts, so no SVG support
// is needed. Checked against the fonts' character maps (fc-query): `has_glyph`
// cannot tell, as it reports NotoEmoji's glyphs as missing.
pub const ICON_DRIVE: &str = "🖴";
pub const ICON_FOLDER: &str = "🗁";
pub const ICON_FILE: &str = "🗋";
pub const ICON_BACKUP: &str = "⬆";
pub const ICON_WRITE: &str = "⬇";
pub const ICON_CLONE: &str = "🗐";
pub const ICON_ARROW: &str = "➡";
pub const ICON_REFRESH: &str = "⟳";
pub const ICON_WARN: &str = "⚠";
pub const ICON_CHECK: &str = "✔";
pub const ICON_CROSS: &str = "✖";
pub const ICON_LOCK: &str = "🔒";
pub const ICON_CURRENT: &str = "⏵";

/// `c` at the given opacity, for tinted backgrounds and borders.
pub fn tint(c: Color32, alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (alpha * 255.0) as u8)
}

/// Colour that stands for a filesystem in partition bars and tables.
pub fn fs_color(fstype: Option<&str>) -> Color32 {
    match fstype.unwrap_or("") {
        "vfat" | "fat" | "fat16" | "fat32" | "exfat" => Color32::from_rgb(0x9B, 0x8C, 0xFF),
        "ext2" | "ext3" | "ext4" => ACCENT,
        "ntfs" => BLUE,
        "swap" => Color32::from_rgb(0x5C, 0x66, 0x70),
        "crypto_LUKS" => Color32::from_rgb(0xD5, 0x8B, 0xD8),
        "btrfs" | "xfs" | "f2fs" => Color32::from_rgb(0xE0, 0xA4, 0x58),
        _ => Color32::from_rgb(0x8A, 0x94, 0xA0),
    }
}

/// Fonts and the dark theme. Called once at start-up.
pub fn setup(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_theme(Theme::Dark);
    ctx.style_mut_of(Theme::Dark, |style| {
        style.text_styles = [
            (TextStyle::Heading, FontId::proportional(26.0)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(14.0)),
            (TextStyle::Small, FontId::proportional(12.0)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);
        style.spacing.interact_size.y = 28.0;

        let v = &mut style.visuals;
        v.panel_fill = BG;
        v.window_fill = SURFACE;
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_corner_radius = CornerRadius::same(LARGE_R);
        v.menu_corner_radius = CornerRadius::same(SMALL_R);
        v.extreme_bg_color = BG;
        v.faint_bg_color = tint(MUTED, 0.04);
        v.code_bg_color = SURFACE;
        v.hyperlink_color = ACCENT;
        v.warn_fg_color = WARN;
        v.error_fg_color = DANGER_TEXT;
        v.selection.bg_fill = tint(ACCENT, 0.35);
        v.selection.stroke = Stroke::new(1.0, ACCENT);

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = SURFACE;
        w.noninteractive.weak_bg_fill = SURFACE;
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        for (state, fill) in [
            (&mut w.inactive, Color32::TRANSPARENT),
            (&mut w.hovered, tint(MUTED, 0.10)),
            (&mut w.active, tint(MUTED, 0.16)),
            (&mut w.open, tint(MUTED, 0.10)),
        ] {
            state.weak_bg_fill = fill;
            state.bg_fill = BG;
            state.bg_stroke = Stroke::new(1.0, BORDER);
            state.fg_stroke = Stroke::new(1.0, TEXT);
            state.corner_radius = CornerRadius::same(SMALL_R);
            state.expansion = 0.0;
        }
        w.hovered.bg_stroke = Stroke::new(1.0, MUTED);
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    });
}

fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    for (name, bytes, family) in [
        (
            "plex-sans",
            &include_bytes!("../../../assets/fonts/IBMPlexSans-Regular.ttf")[..],
            FontFamily::Proportional,
        ),
        (
            "plex-mono",
            &include_bytes!("../../../assets/fonts/IBMPlexMono-Regular.ttf")[..],
            FontFamily::Monospace,
        ),
    ] {
        fonts
            .font_data
            .insert(name.into(), Arc::new(FontData::from_static(bytes)));
        // In front of egui's own fonts, which stay as fallbacks for the icons.
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, name.into());
    }
    fonts
}

pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).color(MUTED)
}

pub fn small(text: impl Into<String>) -> RichText {
    RichText::new(text).color(MUTED).size(12.0)
}

pub fn mono(text: impl Into<String>) -> RichText {
    RichText::new(text).monospace()
}

/// The frame of a content card.
pub fn card() -> Frame {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(LARGE_R)
        .inner_margin(16)
}

/// A tinted notice: icon, coloured title, then whatever `body` adds.
pub fn callout(ui: &mut Ui, color: Color32, icon: &str, title: &str, body: impl FnOnce(&mut Ui)) {
    Frame::new()
        .fill(tint(color, 0.10))
        .stroke(Stroke::new(1.0, tint(color, 0.4)))
        .corner_radius(LARGE_R)
        .inner_margin(Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(icon).color(color).size(18.0));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    ui.label(RichText::new(title).color(color));
                    body(ui);
                });
            });
        });
}

/// A small tinted tag.
pub fn chip(ui: &mut Ui, text: &str, color: Color32) {
    Frame::new()
        .fill(tint(color, 0.15))
        .corner_radius(SMALL_R)
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(color).size(12.0))
        });
}

pub fn primary_button(text: &str) -> Button<'static> {
    Button::new(RichText::new(text.to_string()).color(ON_ACCENT))
        .fill(ACCENT)
        .stroke(Stroke::NONE)
        .min_size(Vec2::new(0.0, 40.0))
}

pub fn danger_button(text: &str) -> Button<'static> {
    Button::new(RichText::new(text.to_string()).color(Color32::WHITE))
        .fill(DANGER)
        .stroke(Stroke::NONE)
        .min_size(Vec2::new(0.0, 40.0))
}

pub fn quiet_button(text: &str) -> Button<'static> {
    Button::new(RichText::new(text.to_string()).size(13.0))
}

/// How a [`row_button`] looks when selected.
pub enum Selected {
    /// Navigation: tinted fill.
    Tint,
    /// Drive list: surface fill with an accent outline.
    Outline,
}

/// A full-width clickable row with arbitrary contents, for navigation and the
/// drive list. A disabled row is dimmed and ignores clicks.
pub fn row_button(
    ui: &mut Ui,
    selected: bool,
    style: Selected,
    enabled: bool,
    margin: Margin,
    add: impl FnOnce(&mut Ui),
) -> Response {
    let mut prepared = Frame::new()
        .inner_margin(margin)
        .corner_radius(SMALL_R)
        .stroke(Stroke::new(1.0, Color32::TRANSPARENT))
        .begin(ui);
    {
        let content = &mut prepared.content_ui;
        content.set_width(content.available_width());
        if !enabled {
            content.disable();
        }
        add(content);
    }
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = prepared.allocate_space(ui).interact(sense);
    let hovered = enabled && response.hovered();
    (prepared.frame.fill, prepared.frame.stroke) = match (selected, style) {
        (true, Selected::Tint) => (tint(ACCENT, 0.15), Stroke::new(1.0, Color32::TRANSPARENT)),
        (true, Selected::Outline) => (SURFACE, Stroke::new(1.0, tint(ACCENT, 0.5))),
        (false, _) if hovered => (tint(MUTED, 0.08), Stroke::new(1.0, Color32::TRANSPARENT)),
        (false, _) => (Color32::TRANSPARENT, Stroke::new(1.0, Color32::TRANSPARENT)),
    };
    prepared.paint(ui);
    if enabled {
        response.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        response
    }
}

/// A horizontal bar of coloured segments, each `bytes` wide out of `total`.
/// Whatever the segments leave is drawn in the border colour.
pub fn segment_bar(ui: &mut Ui, segments: &[(u64, Color32)], total: u64, height: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let painter = ui.painter_at(rect);
    let r = CornerRadius::same(if height <= 6.0 { 2 } else { SMALL_R });
    painter.rect_filled(rect, r, BORDER);
    if total == 0 {
        return;
    }
    // Tiny partitions still get a sliver; a 2px gap separates neighbours.
    let gap = if height <= 6.0 { 1.5 } else { 2.0 };
    let mut x = rect.left();
    for (bytes, color) in segments {
        let w = (*bytes as f32 / total as f32 * rect.width()).max(4.0);
        let right = (x + w).min(rect.right());
        if right - gap > x {
            let seg = egui::Rect::from_min_max(
                egui::pos2(x, rect.top()),
                egui::pos2(right - gap, rect.bottom()),
            );
            painter.rect_filled(seg, r, *color);
        }
        x = right;
    }
}

/// Colour swatches with names, for explaining a [`segment_bar`].
/// Laid out right to left when placed in a right-aligned header, so it still
/// reads swatch-then-name, first item first.
pub fn legend(ui: &mut Ui, items: &[(String, Color32)]) {
    let rtl = ui.layout().prefer_right_to_left();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 14.0;
        let ordered: Vec<_> = if rtl {
            items.iter().rev().collect()
        } else {
            items.iter().collect()
        };
        for (name, color) in ordered {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let swatch = |ui: &mut Ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    ui.painter()
                        .rect_filled(rect, CornerRadius::same(2), *color);
                };
                if rtl {
                    ui.label(small(name.clone()));
                    swatch(ui);
                } else {
                    swatch(ui);
                    ui.label(small(name.clone()));
                }
            });
        }
    });
}

/// A plain accent progress bar; `egui::ProgressBar` animates and rounds more.
pub fn progress_bar(ui: &mut Ui, fraction: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 10.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(SMALL_R), BORDER);
    let mut done = rect;
    done.set_width(rect.width() * fraction.clamp(0.0, 1.0));
    painter.rect_filled(done, CornerRadius::same(SMALL_R), ACCENT);
}
