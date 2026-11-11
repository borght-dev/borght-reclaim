use std::collections::HashMap;

use eframe::egui::{self, Color32, FontFamily, FontId, TextStyle};

use crate::plan::Safety;
use crate::scan::{NodeId, Tree, ROOT};

pub const BACKGROUND: Color32 = Color32::from_rgb(0x0e, 0x11, 0x16);
pub const PANEL: Color32 = Color32::from_rgb(0x14, 0x19, 0x20);
pub const RAISED: Color32 = Color32::from_rgb(0x1d, 0x24, 0x2e);
pub const BORDER: Color32 = Color32::from_rgb(0x2a, 0x33, 0x40);
pub const TEXT: Color32 = Color32::from_rgb(0xe7, 0xea, 0xee);
pub const MUTED: Color32 = Color32::from_rgb(0x93, 0x9d, 0xab);
pub const FAINT: Color32 = Color32::from_rgb(0x5f, 0x6a, 0x79);
pub const ACCENT: Color32 = Color32::from_rgb(0x6a, 0xa6, 0xff);
pub const SAFE: Color32 = Color32::from_rgb(0x3e, 0xcf, 0x6e);
pub const REVIEW: Color32 = Color32::from_rgb(0xe3, 0xa5, 0x3a);
pub const BLOCKED: Color32 = Color32::from_rgb(0xc9, 0x60, 0x5a);
pub const DANGER: Color32 = Color32::from_rgb(0xe5, 0x53, 0x4b);
pub const USED: Color32 = Color32::from_rgb(0x4a, 0x57, 0x69);

pub const ROW_HEIGHT: f32 = 22.0;

/// The palette is dark only, so the Windows light/dark setting must not swap egui's theme.
pub fn apply(context: &egui::Context) {
    context.set_theme(egui::ThemePreference::Dark);
    context.all_styles_mut(apply_style);
}

fn apply_style(style: &mut egui::Style) {
    style.text_styles = [
        (TextStyle::Heading, FontId::new(19.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);

    let visuals = &mut style.visuals;
    *visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL;
    visuals.window_fill = RAISED;
    visuals.extreme_bg_color = BACKGROUND;
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    visuals.widgets.noninteractive.bg_stroke.color = BORDER;
    visuals.widgets.inactive.bg_fill = RAISED;
    visuals.widgets.inactive.weak_bg_fill = RAISED;
    visuals.widgets.hovered.weak_bg_fill = BORDER;
    visuals.window_stroke.color = BORDER;
}

pub fn safety_color(safety: Safety) -> Color32 {
    match safety {
        Safety::Safe => SAFE,
        Safety::Review => REVIEW,
        Safety::Blocked => BLOCKED,
    }
}

pub fn safety_label(safety: Safety) -> &'static str {
    match safety {
        Safety::Safe => "Safe to remove",
        Safety::Review => "Your call",
        Safety::Blocked => "In use or unsaved",
    }
}

/// Clean-up rating of the nodes the plan targets; everything below a target inherits it.
pub type Ratings = HashMap<NodeId, Safety>;

pub fn rating_of(tree: &Tree, ratings: &Ratings, node: NodeId) -> Option<Safety> {
    let mut current = node;
    loop {
        if let Some(&safety) = ratings.get(&current) {
            return Some(safety);
        }
        if current == ROOT {
            return None;
        }
        current = tree.node(current).parent;
    }
}

pub fn tile_color(tree: &Tree, node: NodeId, rating: Option<Safety>) -> Color32 {
    match rating {
        Some(Safety::Safe) => Color32::from_rgb(0x2f, 0xa8, 0x58),
        Some(Safety::Review) => Color32::from_rgb(0xc2, 0x8a, 0x2c),
        Some(Safety::Blocked) => Color32::from_rgb(0x8e, 0x4f, 0x55),
        None if tree.is_directory(node) => Color32::from_rgb(0x3b, 0x47, 0x58),
        None => file_kind_color(tree.name(node)),
    }
}

fn file_kind_color(name: &str) -> Color32 {
    let extension =
        name.rsplit_once('.').map_or("", |(_, extension)| extension).to_ascii_lowercase();
    match extension.as_str() {
        "dll" | "exe" | "pdb" | "node" | "so" | "lib" | "wasm" => {
            Color32::from_rgb(0x4c, 0x6a, 0x92)
        }
        "pack" | "idx" | "zip" | "7z" | "gz" | "tgz" | "nupkg" | "bak" | "tar" | "cab" | "msi" => {
            Color32::from_rgb(0x6b, 0x5b, 0x95)
        }
        "mp4" | "mp3" | "wav" | "webm" | "png" | "jpg" | "jpeg" | "gif" | "mov" | "pptx" => {
            Color32::from_rgb(0x3f, 0x7f, 0x86)
        }
        "mdf" | "ldf" | "sqlite" | "sqlite3" | "db" | "bin" | "dbmdl" => {
            Color32::from_rgb(0x86, 0x62, 0x4a)
        }
        _ => Color32::from_rgb(0x46, 0x53, 0x66),
    }
}

pub fn scaled(color: Color32, factor: f32) -> Color32 {
    let channel = |value: u8| (f32::from(value) * factor).clamp(0.0, 255.0) as u8;
    Color32::from_rgb(channel(color.r()), channel(color.g()), channel(color.b()))
}
