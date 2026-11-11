use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui::epaint::{Vertex, WHITE_UV};
use eframe::egui::{
    self, Color32, FontId, Mesh, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui,
};

use super::theme::{self, Ratings};
use super::Intent;
use crate::format;
use crate::scan::{NodeId, Tree, ROOT};
use crate::treemap::{self, Area, Tile};

const TILE_GAP: f32 = 0.5;
const LABEL_MIN_WIDTH: f32 = 96.0;
const LABEL_MIN_HEIGHT: f32 = 36.0;

struct Cache {
    zoom: NodeId,
    canvas: Rect,
    content_generation: u64,
    tiles: Vec<Tile>,
    tile_of_node: HashMap<NodeId, usize>,
    mesh: Arc<Mesh>,
}

pub struct TreemapView {
    zoom: NodeId,
    cache: Option<Cache>,
}

impl TreemapView {
    pub fn new() -> Self {
        Self { zoom: ROOT, cache: None }
    }

    pub fn zoom(&self) -> NodeId {
        self.zoom
    }

    pub fn set_zoom(&mut self, node: NodeId) {
        self.zoom = node;
    }

    /// `content_generation` changes whenever the tree or the clean-up ratings do.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        tree: &Tree,
        focused: Option<NodeId>,
        ratings: &Ratings,
        content_generation: u64,
    ) -> Vec<Intent> {
        let mut intents = Vec::new();
        self.show_breadcrumb(ui, tree, &mut intents);

        let (canvas, response) = ui.allocate_exact_size(ui.available_size(), Sense::click());
        let is_current = self.cache.as_ref().is_some_and(|cache| {
            cache.zoom == self.zoom
                && cache.canvas == canvas
                && cache.content_generation == content_generation
        });
        if !is_current {
            self.cache = Some(build_cache(tree, self.zoom, canvas, ratings, content_generation));
        }
        let cache = self.cache.as_ref().unwrap();

        let painter = ui.painter_at(canvas);
        painter.rect_filled(canvas, 0.0, theme::BACKGROUND);
        painter.add(egui::Shape::mesh(Arc::clone(&cache.mesh)));
        paint_labels(&painter, tree, cache);

        if let Some(tile) = focused.and_then(|node| nearest_tile(tree, cache, node)) {
            let outline = Stroke::new(2.0, Color32::WHITE);
            painter.rect_stroke(to_rect(tile.area), 0.0, outline, StrokeKind::Inside);
        }

        let hovered = response
            .hover_pos()
            .and_then(|at| cache.tiles.iter().rev().find(|tile| tile.area.contains(at.x, at.y)));
        if let Some(tile) = hovered {
            let outline = Stroke::new(1.0, Color32::from_white_alpha(200));
            painter.rect_stroke(to_rect(tile.area), 0.0, outline, StrokeKind::Inside);
            let node = tile.node;
            response.clone().on_hover_ui_at_pointer(|ui| describe(ui, tree, node, ratings));
            if response.double_clicked() {
                if let Some(folder) = child_of_zoom_containing(tree, self.zoom, node) {
                    intents.push(Intent::Zoom(folder));
                }
            } else if response.clicked() {
                intents.push(Intent::Focus(node));
            }
        }
        if response.secondary_clicked() && self.zoom != ROOT {
            intents.push(Intent::Zoom(tree.node(self.zoom).parent));
        }
        intents
    }

    fn show_breadcrumb(&self, ui: &mut Ui, tree: &Tree, intents: &mut Vec<Intent>) {
        let mut trail = vec![self.zoom];
        while *trail.last().unwrap() != ROOT {
            trail.push(tree.node(*trail.last().unwrap()).parent);
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.add_space(8.0);
            for (position, &node) in trail.iter().rev().enumerate() {
                if position > 0 {
                    ui.label(RichText::new("›").color(theme::FAINT));
                }
                let is_current = node == self.zoom;
                let color = if is_current { theme::TEXT } else { theme::MUTED };
                let name = RichText::new(tree.name(node)).size(12.5).color(color);
                if ui.add(egui::Button::new(name).frame(false)).clicked() && !is_current {
                    intents.push(Intent::Zoom(node));
                }
            }
            ui.add_space(12.0);
            let hint = "click to locate · double-click to zoom in · right-click to zoom out";
            ui.label(RichText::new(hint).size(11.0).color(theme::FAINT));
        });
    }
}

fn build_cache(
    tree: &Tree,
    zoom: NodeId,
    canvas: Rect,
    ratings: &Ratings,
    content_generation: u64,
) -> Cache {
    let area = Area {
        left: canvas.left(),
        top: canvas.top(),
        width: canvas.width(),
        height: canvas.height(),
    };
    let tiles = treemap::layout(tree, zoom, area);
    let mut mesh = Mesh::default();
    for tile in tiles.iter().filter(|tile| tile.is_leaf) {
        let rating = theme::rating_of(tree, ratings, tile.node);
        add_cushion(
            &mut mesh,
            to_rect(tile.area).shrink(TILE_GAP),
            theme::tile_color(tree, tile.node, rating),
        );
    }
    let tile_of_node = tiles.iter().enumerate().map(|(index, tile)| (tile.node, index)).collect();
    Cache { zoom, canvas, content_generation, tiles, tile_of_node, mesh: Arc::new(mesh) }
}

/// Lit from the top left, which is what lets the eye separate thousands of flat tiles.
fn add_cushion(mesh: &mut Mesh, rect: Rect, color: Color32) {
    let first = mesh.vertices.len() as u32;
    let corners = [
        (rect.left_top(), 1.24),
        (rect.right_top(), 1.0),
        (rect.right_bottom(), 0.7),
        (rect.left_bottom(), 1.0),
    ];
    for (pos, light) in corners {
        mesh.vertices.push(Vertex { pos, uv: WHITE_UV, color: theme::scaled(color, light) });
    }
    mesh.indices.extend([first, first + 1, first + 2, first, first + 2, first + 3]);
}

fn paint_labels(painter: &egui::Painter, tree: &Tree, cache: &Cache) {
    let labelled = cache.tiles.iter().filter(|tile| {
        tile.depth == 1
            && tile.area.width >= LABEL_MIN_WIDTH
            && tile.area.height >= LABEL_MIN_HEIGHT
    });
    for tile in labelled {
        let rect = to_rect(tile.area);
        let text =
            format!("{}  {}", tree.name(tile.node), format::bytes(tree.node(tile.node).size));
        let galley = painter.layout_no_wrap(text, FontId::proportional(12.0), Color32::WHITE);
        let plate = Rect::from_min_size(
            rect.min + egui::vec2(3.0, 3.0),
            (galley.size() + egui::vec2(10.0, 5.0)).min(rect.size() - egui::vec2(6.0, 6.0)),
        );
        let clipped = painter.with_clip_rect(plate);
        clipped.rect_filled(plate, 3.0, Color32::from_black_alpha(170));
        clipped.galley(plate.min + egui::vec2(5.0, 2.0), galley, Color32::WHITE);
    }
}

fn nearest_tile<'a>(tree: &Tree, cache: &'a Cache, node: NodeId) -> Option<&'a Tile> {
    let mut current = node;
    loop {
        if let Some(&index) = cache.tile_of_node.get(&current) {
            return Some(&cache.tiles[index]);
        }
        if current == ROOT {
            return None;
        }
        current = tree.node(current).parent;
    }
}

fn child_of_zoom_containing(tree: &Tree, zoom: NodeId, node: NodeId) -> Option<NodeId> {
    let mut current = node;
    while current != ROOT && tree.node(current).parent != zoom {
        current = tree.node(current).parent;
    }
    (current != ROOT && tree.is_directory(current)).then_some(current)
}

fn describe(ui: &mut Ui, tree: &Tree, node: NodeId, ratings: &Ratings) {
    let details = tree.node(node);
    ui.label(RichText::new(tree.path(node).display().to_string()).strong());
    let mut facts =
        format!("{}  ·  frees {}", format::bytes(details.size), format::bytes(details.frees));
    if tree.is_directory(node) {
        facts.push_str(&format!("  ·  {} files", format::count(u64::from(details.file_count))));
    }
    ui.label(RichText::new(facts).monospace().color(theme::MUTED));
    if tree.is_hardlinked(node) {
        ui.label(
            RichText::new("hardlinked: other folders hold the same bytes").color(theme::REVIEW),
        );
    }
    if let Some(safety) = theme::rating_of(tree, ratings, node) {
        ui.label(RichText::new(theme::safety_label(safety)).color(theme::safety_color(safety)));
    }
}

fn to_rect(area: Area) -> Rect {
    Rect::from_min_size(Pos2::new(area.left, area.top), egui::vec2(area.width, area.height))
}
