use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use eframe::egui::{self, Align2, FontId, Pos2, Rect, Sense, Shape, Stroke, Ui};

use super::theme::{self, Ratings, ROW_HEIGHT};
use super::Intent;
use crate::format;
use crate::scan::{NodeId, Tree, ROOT};

const INDENT: f32 = 14.0;
const SIZE_WIDTH: f32 = 82.0;
const FREES_WIDTH: f32 = 82.0;
const FILES_WIDTH: f32 = 74.0;
const MODIFIED_WIDTH: f32 = 62.0;
const SHARE_WIDTH: f32 = 72.0;
const ROWS_KEPT_ABOVE_REVEALED: f32 = 3.0;

struct Row {
    node: NodeId,
    depth: u16,
}

struct Columns {
    name: Rect,
    share: Rect,
    size: Rect,
    frees: Rect,
    files: Rect,
    modified: Rect,
}

impl Columns {
    fn split(row: Rect) -> Self {
        let mut right = row.right() - 8.0;
        let mut take = |width: f32| {
            let column = Rect::from_min_max(
                Pos2::new(right - width, row.top()),
                Pos2::new(right, row.bottom()),
            );
            right -= width;
            column
        };
        let modified = take(MODIFIED_WIDTH);
        let files = take(FILES_WIDTH);
        let frees = take(FREES_WIDTH);
        let size = take(SIZE_WIDTH);
        let share = take(SHARE_WIDTH);
        let name = Rect::from_min_max(row.left_top(), Pos2::new(right - 8.0, row.bottom()));
        Self { name, share, size, frees, files, modified }
    }
}

pub struct TreeView {
    expanded: HashSet<NodeId>,
    children_by_size: HashMap<NodeId, Vec<NodeId>>,
    rows: Vec<Row>,
    rows_are_stale: bool,
    reveal: Option<NodeId>,
}

impl TreeView {
    pub fn new() -> Self {
        Self {
            expanded: HashSet::from([ROOT]),
            children_by_size: HashMap::new(),
            rows: Vec::new(),
            rows_are_stale: true,
            reveal: None,
        }
    }

    pub fn expanded_paths(&self, tree: &Tree) -> Vec<PathBuf> {
        self.expanded.iter().map(|&node| tree.path(node)).collect()
    }

    /// Node ids change with every scan, so expansion is carried over by path.
    pub fn restore_expanded(&mut self, tree: &Tree, paths: &[PathBuf]) {
        self.expanded = paths.iter().filter_map(|path| tree.find(path)).collect();
        self.expanded.insert(ROOT);
        self.invalidate();
    }

    pub fn invalidate(&mut self) {
        self.children_by_size.clear();
        self.rows_are_stale = true;
    }

    pub fn reveal(&mut self, tree: &Tree, node: NodeId) {
        let mut ancestor = node;
        while ancestor != ROOT {
            ancestor = tree.node(ancestor).parent;
            self.expanded.insert(ancestor);
        }
        self.reveal = Some(node);
        self.rows_are_stale = true;
    }

    fn rebuild_rows(&mut self, tree: &Tree) {
        self.rows.clear();
        let mut pending = vec![Row { node: ROOT, depth: 0 }];
        while let Some(row) = pending.pop() {
            if self.expanded.contains(&row.node) {
                let children = self.children_by_size.entry(row.node).or_insert_with(|| {
                    let mut children: Vec<NodeId> =
                        tree.children(row.node).filter(|&child| !tree.is_removed(child)).collect();
                    children.sort_by_key(|&child| std::cmp::Reverse(tree.node(child).size));
                    children
                });
                let depth = row.depth + 1;
                pending.extend(children.iter().rev().map(|&node| Row { node, depth }));
            }
            self.rows.push(row);
        }
        self.rows_are_stale = false;
    }

    pub fn show(
        &mut self,
        ui: &mut Ui,
        tree: &Tree,
        focused: Option<NodeId>,
        ratings: &Ratings,
        now_unix: i64,
    ) -> Vec<Intent> {
        if self.rows_are_stale {
            self.rebuild_rows(tree);
        }
        let mut intents = Vec::new();
        ui.spacing_mut().item_spacing.y = 0.0;
        paint_header(ui);

        let mut scroll_area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(revealed) = self.reveal.take() {
            if let Some(index) = self.rows.iter().position(|row| row.node == revealed) {
                let offset = (index as f32 - ROWS_KEPT_ABOVE_REVEALED).max(0.0) * ROW_HEIGHT;
                scroll_area = scroll_area.vertical_scroll_offset(offset);
            }
        }

        let mut toggled = None;
        scroll_area.show_rows(ui, ROW_HEIGHT, self.rows.len(), |ui, visible| {
            for row in &self.rows[visible] {
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), ROW_HEIGHT),
                    Sense::click(),
                );
                let is_expanded = self.expanded.contains(&row.node);
                let toggle_area = paint_row(
                    ui,
                    rect,
                    tree,
                    row,
                    RowState {
                        is_expanded,
                        is_focused: focused == Some(row.node),
                        is_hovered: response.hovered(),
                    },
                    ratings,
                    now_unix,
                );
                let clicked_toggle = response.clicked()
                    && response.interact_pointer_pos().is_some_and(|at| toggle_area.contains(at));
                if clicked_toggle || response.double_clicked() {
                    toggled = Some(row.node);
                } else if response.clicked() {
                    intents.push(Intent::Focus(row.node));
                }
                response.context_menu(|ui| {
                    let path = tree.path(row.node);
                    if ui.button("Show in Explorer").clicked() {
                        let _ = std::process::Command::new("explorer")
                            .arg(format!("/select,{}", path.display()))
                            .spawn();
                        ui.close_menu();
                    }
                    if ui.button("Copy path").clicked() {
                        ui.ctx().copy_text(path.display().to_string());
                        ui.close_menu();
                    }
                    if tree.is_directory(row.node) && ui.button("Zoom treemap here").clicked() {
                        intents.push(Intent::Zoom(row.node));
                        ui.close_menu();
                    }
                    ui.separator();
                    if row.node != ROOT && ui.button("Delete permanently…").clicked() {
                        intents.push(Intent::Delete(row.node));
                        ui.close_menu();
                    }
                });
            }
        });

        if let Some(node) = toggled {
            if !self.expanded.remove(&node) {
                self.expanded.insert(node);
            }
            self.rows_are_stale = true;
        }
        intents
    }
}

fn paint_header(ui: &mut Ui) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_HEIGHT), Sense::hover());
    let columns = Columns::split(rect);
    let painter = ui.painter();
    let font = FontId::proportional(11.5);
    let left_title = |column: Rect, title: &str| {
        let at = Pos2::new(column.left() + 10.0, column.center().y);
        painter.text(at, Align2::LEFT_CENTER, title, font.clone(), theme::MUTED);
    };
    let right_title = |column: Rect, title: &str| {
        painter.text(
            column.right_center(),
            Align2::RIGHT_CENTER,
            title,
            font.clone(),
            theme::MUTED,
        );
    };
    left_title(columns.name, "NAME");
    right_title(columns.share, "OF PARENT");
    right_title(columns.size, "SIZE");
    right_title(columns.frees, "FREES");
    right_title(columns.files, "FILES");
    right_title(columns.modified, "CHANGED");
    painter.hline(rect.x_range(), rect.bottom(), Stroke::new(1.0_f32, theme::BORDER));
}

struct RowState {
    is_expanded: bool,
    is_focused: bool,
    is_hovered: bool,
}

/// Returns the area that toggles expansion when clicked.
fn paint_row(
    ui: &Ui,
    rect: Rect,
    tree: &Tree,
    row: &Row,
    state: RowState,
    ratings: &Ratings,
    now_unix: i64,
) -> Rect {
    let painter = ui.painter();
    if state.is_focused {
        painter.rect_filled(rect, 0.0, theme::ACCENT.gamma_multiply(0.22));
    } else if state.is_hovered {
        painter.rect_filled(rect, 0.0, theme::RAISED);
    }

    let node = tree.node(row.node);
    let columns = Columns::split(rect);
    let indent = columns.name.left() + 8.0 + f32::from(row.depth) * INDENT;
    let toggle_area = Rect::from_min_size(
        Pos2::new(indent - 4.0, rect.top()),
        egui::vec2(INDENT + 4.0, rect.height()),
    );
    let has_children = tree.is_directory(row.node) && !tree.children(row.node).is_empty();
    if has_children {
        paint_disclosure(painter, Pos2::new(indent + 4.0, rect.center().y), state.is_expanded);
    }

    let rating = theme::rating_of(tree, ratings, row.node);
    let swatch = Rect::from_center_size(
        Pos2::new(indent + INDENT + 6.0, rect.center().y),
        egui::vec2(9.0, 9.0),
    );
    painter.rect_filled(swatch, 2.0, theme::tile_color(tree, row.node, rating));

    let name_color = if tree.is_unreadable(row.node) { theme::FAINT } else { theme::TEXT };
    let mut name = tree.name(row.node).to_string();
    if tree.is_link(row.node) {
        name.push_str("  → link");
    }
    painter.with_clip_rect(columns.name).text(
        Pos2::new(swatch.right() + 7.0, rect.center().y),
        Align2::LEFT_CENTER,
        name,
        FontId::proportional(13.0),
        name_color,
    );

    let parent_size = if row.node == ROOT { node.size } else { tree.node(node.parent).size };
    let share = if parent_size == 0 { 0.0 } else { node.size as f32 / parent_size as f32 };
    let track = Rect::from_center_size(
        columns.share.center() + egui::vec2(6.0, 0.0),
        egui::vec2(SHARE_WIDTH - 16.0, 5.0),
    );
    painter.rect_filled(track, 2.0, theme::BORDER);
    let filled = Rect::from_min_size(track.min, egui::vec2(track.width() * share, track.height()));
    painter.rect_filled(filled, 2.0, rating.map_or(theme::ACCENT, theme::safety_color));

    let number = |column: Rect, text: String, color| {
        painter.text(
            column.right_center(),
            Align2::RIGHT_CENTER,
            text,
            FontId::monospace(12.0),
            color,
        );
    };
    number(columns.size, format::bytes(node.size), theme::TEXT);
    let frees_color = if node.frees < node.size / 2 { theme::REVIEW } else { theme::MUTED };
    number(columns.frees, format::bytes(node.frees), frees_color);
    if tree.is_directory(row.node) {
        number(columns.files, format::count(u64::from(node.file_count)), theme::MUTED);
    }
    let age = format::age((now_unix - node.modified_unix).max(0));
    number(columns.modified, age, theme::MUTED);
    toggle_area
}

fn paint_disclosure(painter: &egui::Painter, center: Pos2, is_expanded: bool) {
    let points = if is_expanded {
        vec![
            center + egui::vec2(-4.0, -2.5),
            center + egui::vec2(4.0, -2.5),
            center + egui::vec2(0.0, 3.0),
        ]
    } else {
        vec![
            center + egui::vec2(-2.5, -4.0),
            center + egui::vec2(3.0, 0.0),
            center + egui::vec2(-2.5, 4.0),
        ]
    };
    painter.add(Shape::convex_polygon(points, theme::MUTED, Stroke::NONE));
}
