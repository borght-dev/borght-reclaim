use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;

use eframe::egui::{self, Align, Align2, FontId, Layout, RichText, Sense, Ui};

use super::theme;
use super::Intent;
use crate::clean::{CleanProgress, CleanReport, Outcome};
use crate::format;
use crate::plan::{Candidate, Kind, Plan, Safety};
use crate::scan::Tree;

const SAFETIES: [Safety; 3] = [Safety::Safe, Safety::Review, Safety::Blocked];
const TARGETS_SHOWN_ON_HOVER: usize = 10;
const FAILURES_SHOWN: usize = 6;

pub enum Activity<'a> {
    Idle,
    Analysing,
    Cleaning(&'a CleanProgress),
}

pub struct PlanView {
    selected: Vec<bool>,
    collapsed: HashSet<(Safety, Kind)>,
}

impl PlanView {
    pub fn new() -> Self {
        Self { selected: Vec::new(), collapsed: HashSet::new() }
    }

    /// Candidates the previous plan already listed keep the tick the user left on them; new
    /// ones start ticked only when rated safe. The first plan folds the blocked groups away.
    pub fn adopt(&mut self, previous: &Plan, plan: &Plan) {
        let identity =
            |candidate: &Candidate| (candidate.kind, candidate.safety, candidate.title.clone());
        let earlier_choices: HashMap<_, bool> = previous
            .candidates
            .iter()
            .zip(&self.selected)
            .map(|(candidate, &selected)| (identity(candidate), selected))
            .collect();
        if previous.candidates.is_empty() {
            self.collapsed = Kind::ALL.iter().map(|&kind| (Safety::Blocked, kind)).collect();
        }
        self.selected = plan
            .candidates
            .iter()
            .map(|candidate| {
                let by_default = candidate.safety == Safety::Safe;
                earlier_choices.get(&identity(candidate)).copied().unwrap_or(by_default)
            })
            .collect();
    }

    pub fn keep_candidates(&mut self, kept: &[bool]) {
        let mut kept = kept.iter().copied();
        self.selected.retain(|_| kept.next().unwrap_or(true));
    }

    pub fn show(
        &mut self,
        ui: &mut Ui,
        tree: &Tree,
        plan: &Plan,
        activity: Activity,
        report: Option<&CleanReport>,
    ) -> Vec<Intent> {
        let mut intents = Vec::new();
        self.selected.resize(plan.candidates.len(), false);

        show_totals(ui, plan);
        ui.add_space(4.0);
        let chosen: Vec<usize> =
            (0..plan.candidates.len()).filter(|&index| self.selected[index]).collect();
        let chosen_bytes = plan.frees_together(tree, |index| self.selected[index]);
        let is_idle = !matches!(activity, Activity::Cleaning(_));
        let label = format!("Clean selected  ·  {}", format::bytes(chosen_bytes));
        let button =
            egui::Button::new(RichText::new(label).size(14.5).strong().color(theme::BACKGROUND))
                .fill(theme::SAFE)
                .min_size(egui::vec2(ui.available_width(), 36.0));
        if ui.add_enabled(is_idle && !chosen.is_empty(), button).clicked() {
            intents.push(Intent::Clean(chosen.clone()));
        }

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{} selected", chosen.len())).small().color(theme::MUTED),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("none").clicked() {
                    self.selected.fill(false);
                }
                if ui.small_button("safe only").clicked() {
                    self.adopt_safe_only(plan);
                }
            });
        });

        match activity {
            Activity::Analysing => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    let text = "still checking worktrees against git and merged pull requests…";
                    ui.label(RichText::new(text).small().color(theme::MUTED));
                });
            }
            Activity::Cleaning(progress) => show_progress(ui, progress),
            Activity::Idle => {}
        }
        if let Some(report) = report {
            show_report(ui, report);
        }
        ui.separator();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for safety in SAFETIES {
                self.show_section(ui, plan, safety, &mut intents);
            }
        });
        intents
    }

    fn adopt_safe_only(&mut self, plan: &Plan) {
        for (selected, candidate) in self.selected.iter_mut().zip(&plan.candidates) {
            *selected = candidate.safety == Safety::Safe;
        }
    }

    fn show_section(
        &mut self,
        ui: &mut Ui,
        plan: &Plan,
        safety: Safety,
        intents: &mut Vec<Intent>,
    ) {
        let in_section = |candidate: &&Candidate| candidate.safety == safety;
        if !plan.candidates.iter().any(|candidate| in_section(&candidate)) {
            return;
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
            ui.painter().circle_filled(dot.center(), 4.5, theme::safety_color(safety));
            let title = theme::safety_label(safety).to_uppercase();
            ui.label(RichText::new(title).size(11.5).strong().color(theme::safety_color(safety)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new(format::bytes(plan.frees(safety))).monospace().strong());
            });
        });

        for kind in Kind::ALL {
            let members: Vec<usize> = plan
                .candidates
                .iter()
                .enumerate()
                .filter(|(_, candidate)| candidate.safety == safety && candidate.kind == kind)
                .map(|(index, _)| index)
                .collect();
            if members.is_empty() {
                continue;
            }
            self.show_group(ui, plan, (safety, kind), &members, intents);
        }
    }

    fn show_group(
        &mut self,
        ui: &mut Ui,
        plan: &Plan,
        group: (Safety, Kind),
        members: &[usize],
        intents: &mut Vec<Intent>,
    ) {
        let (safety, kind) = group;
        let is_collapsed = self.collapsed.contains(&group);
        let group_bytes: u64 = members.iter().map(|&index| plan.candidates[index].frees).sum();
        ui.horizontal(|ui| {
            let arrow = if is_collapsed { "⏵" } else { "⏷" };
            let heading = RichText::new(format!("{arrow}  {}", kind.label())).strong();
            if ui.add(egui::Button::new(heading).frame(false)).clicked() {
                if is_collapsed {
                    self.collapsed.remove(&group);
                } else {
                    self.collapsed.insert(group);
                }
            }
            ui.label(RichText::new(members.len().to_string()).small().color(theme::FAINT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new(format::bytes(group_bytes)).monospace().color(theme::MUTED));
                if safety != Safety::Blocked {
                    let all_selected = members.iter().all(|&index| self.selected[index]);
                    let toggle = if all_selected { "clear" } else { "all" };
                    if ui.small_button(toggle).clicked() {
                        for &index in members {
                            self.selected[index] = !all_selected;
                        }
                    }
                }
            });
        });
        if is_collapsed {
            return;
        }
        for &index in members {
            self.show_candidate(ui, &plan.candidates[index], index, intents);
        }
    }

    fn show_candidate(
        &mut self,
        ui: &mut Ui,
        candidate: &Candidate,
        index: usize,
        intents: &mut Vec<Intent>,
    ) {
        let is_selectable = candidate.safety != Safety::Blocked;
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            ui.add_enabled(is_selectable, egui::Checkbox::without_text(&mut self.selected[index]));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new(format::bytes(candidate.frees)).monospace());
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let title = egui::Label::new(RichText::new(&candidate.title))
                        .truncate()
                        .sense(Sense::click());
                    if ui.add(title).on_hover_text("locate in the tree and treemap").clicked() {
                        intents.push(Intent::Focus(candidate.location));
                    }
                });
            });
        });
        ui.horizontal(|ui| {
            ui.add_space(42.0);
            let explanation = format!("{}  ·  {}", candidate.detail, candidate.reason);
            let label =
                egui::Label::new(RichText::new(explanation).small().color(theme::MUTED)).truncate();
            ui.add(label).on_hover_ui(|ui| describe(ui, candidate));
        });
        ui.add_space(2.0);
    }
}

fn describe(ui: &mut Ui, candidate: &Candidate) {
    ui.set_max_width(560.0);
    ui.label(RichText::new(&candidate.detail).strong());
    ui.label(&candidate.reason);
    if candidate.size > candidate.frees + candidate.frees / 10 {
        let shared = format!(
            "occupies {} but frees {}: the rest is hardlinked from elsewhere",
            format::bytes(candidate.size),
            format::bytes(candidate.frees)
        );
        ui.label(RichText::new(shared).color(theme::REVIEW));
    }
    ui.separator();
    for target in candidate.targets.iter().take(TARGETS_SHOWN_ON_HOVER) {
        ui.label(RichText::new(target.path.display().to_string()).monospace().small());
    }
    if candidate.targets.len() > TARGETS_SHOWN_ON_HOVER {
        let more = candidate.targets.len() - TARGETS_SHOWN_ON_HOVER;
        ui.label(RichText::new(format!("… and {more} more")).small().color(theme::MUTED));
    }
}

fn show_totals(ui: &mut Ui, plan: &Plan) {
    let spacing = 6.0;
    let width = (ui.available_width() - 2.0 * spacing) / 3.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = spacing;
        for safety in SAFETIES {
            let (card, _) = ui.allocate_exact_size(egui::vec2(width, 58.0), Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(card, 6.0, theme::RAISED);
            painter.text(
                card.left_top() + egui::vec2(10.0, 9.0),
                Align2::LEFT_TOP,
                theme::safety_label(safety),
                FontId::proportional(11.5),
                theme::MUTED,
            );
            painter.text(
                card.left_bottom() + egui::vec2(10.0, -9.0),
                Align2::LEFT_BOTTOM,
                format::bytes(plan.frees(safety)),
                FontId::monospace(18.0),
                theme::safety_color(safety),
            );
        }
    });
}

fn show_progress(ui: &mut Ui, progress: &CleanProgress) {
    let finished = progress.finished_targets.load(Ordering::Relaxed);
    let total = progress.total_targets.load(Ordering::Relaxed).max(1);
    let bar = egui::ProgressBar::new(finished as f32 / total as f32)
        .text(format!("deleting {finished} of {total}"))
        .fill(theme::SAFE.gamma_multiply(0.6));
    ui.add(bar);
    let current = progress.current.lock().unwrap().clone();
    ui.add(egui::Label::new(RichText::new(current).small().color(theme::MUTED)).truncate());
    if ui.small_button("stop after this item").clicked() {
        progress.cancelled.store(true, Ordering::Relaxed);
    }
}

fn show_report(ui: &mut Ui, report: &CleanReport) {
    egui::Frame::new().fill(theme::RAISED).corner_radius(6.0).inner_margin(10.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let headline = format!("Freed {}", format::bytes(report.freed_bytes()));
        ui.label(RichText::new(headline).size(16.0).strong().color(theme::SAFE));
        let detail = format!(
            "free space {} → {}  ·  {} removed, {} not",
            format::bytes(report.free_bytes_before),
            format::bytes(report.free_bytes_after),
            report.removed_count(),
            report.not_removed().count(),
        );
        ui.label(RichText::new(detail).small().color(theme::MUTED));
        for result in report.not_removed().take(FAILURES_SHOWN) {
            let (label, reason, color) = match &result.outcome {
                Outcome::Skipped(reason) => ("skipped", reason, theme::REVIEW),
                Outcome::Failed(reason) => ("failed", reason, theme::DANGER),
                Outcome::Removed => continue,
            };
            let name = result.path.file_name().unwrap_or_default().to_string_lossy();
            let line = RichText::new(format!("{label}: {name}  ({reason})")).small().color(color);
            ui.add(egui::Label::new(line).truncate())
                .on_hover_text(format!("{}\n{reason}", result.path.display()));
        }
    });
}
