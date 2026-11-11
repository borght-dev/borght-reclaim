mod brand;
mod plan_view;
mod theme;
mod tree_view;
mod treemap_view;

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Align, Layout, Pos2, Rect, RichText, Sense};

use crate::clean::{self, CleanProgress, CleanReport, Outcome};
use crate::format;
use crate::plan::{self, Action, Candidate, Config, Depth, Kind, Plan, Safety, Target};
use crate::scan::{NodeId, ScanProgress, Tree, ROOT};
use crate::snapshot;
use crate::winfs::{self, VolumeSpace};
use plan_view::{Activity, PlanView};
use theme::Ratings;
use tree_view::TreeView;
use treemap_view::TreemapView;

const FRAMES_BEFORE_SCREENSHOT: u32 = 6;
const CANDIDATES_LISTED_IN_CONFIRMATION: usize = 12;

pub enum Intent {
    Focus(NodeId),
    Zoom(NodeId),
    Delete(NodeId),
    Clean(Vec<usize>),
}

/// Drives the window without a person: an optional click once a scan is shown, then a
/// picture of the settled result, then exit.
#[derive(Default)]
pub struct Script {
    pub screenshot: Option<PathBuf>,
    pub click: Option<[f32; 2]>,
}

pub fn run(config: Config, root: PathBuf, script: Script) -> Result<(), String> {
    detach_console_when_launched_alone();
    let root = std::path::absolute(&root).map_err(|error| error.to_string())?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(window_title(&root))
            .with_icon(brand::window_icon())
            .with_inner_size([1500.0, 940.0])
            .with_min_inner_size([1000.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Borght Reclaim",
        options,
        Box::new(move |creation| {
            theme::apply(&creation.egui_ctx);
            Ok(Box::new(App::new(creation.egui_ctx.clone(), config, root, script)))
        }),
    )
    .map_err(|error| error.to_string())
}

/// Started from Explorer, the console window is ours alone and only gets in the way.
fn detach_console_when_launched_alone() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut attached = [0u32; 2];
    if unsafe { GetConsoleProcessList(attached.as_mut_ptr(), 2) } <= 1 {
        unsafe { FreeConsole() };
    }
}

enum ScanNews {
    /// The last saved scan, shown while it is being brought up to date.
    Remembered(Tree),
    Finished(io::Result<Tree>),
}

struct Scanning {
    root: PathBuf,
    progress: Arc<ScanProgress>,
    news: Receiver<ScanNews>,
}

impl Scanning {
    /// Discards what a scan nobody is looking at reports; false once it has ended.
    fn is_still_running(&self) -> bool {
        loop {
            match self.news.try_recv() {
                Ok(ScanNews::Remembered(_)) => {}
                Ok(ScanNews::Finished(_)) | Err(mpsc::TryRecvError::Disconnected) => return false,
                Err(mpsc::TryRecvError::Empty) => return true,
            }
        }
    }
}

struct Drive {
    root: PathBuf,
    space: Option<VolumeSpace>,
}

fn is_same_folder(one: &Path, other: &Path) -> bool {
    one.as_os_str().eq_ignore_ascii_case(other.as_os_str())
}

fn window_title(root: &Path) -> String {
    format!("Borght Reclaim — {}", root.display())
}

/// The quick plan arrives within seconds; the thorough one replaces it once git has answered.
struct Analysis {
    tree_generation: u64,
    depth: Depth,
    plan: Plan,
}

struct Cleaning {
    progress: Arc<CleanProgress>,
    finished: Receiver<CleanReport>,
    candidates: Vec<Candidate>,
    tree_generation: u64,
}

struct App {
    context: egui::Context,
    config: Config,
    root: PathBuf,
    volume: Option<VolumeSpace>,
    drives: Vec<Drive>,

    tree: Option<Arc<Tree>>,
    tree_generation: u64,
    content_generation: u64,
    scanning: Option<Scanning>,
    /// Scans of drives switched away from: they finish and save, and are picked up again
    /// when their drive is chosen before they end.
    scans_left_running: Vec<Scanning>,
    scan_error: Option<String>,

    plan: Plan,
    ratings: Ratings,
    analysing: Option<Receiver<Analysis>>,

    awaiting_confirmation: Option<Vec<Candidate>>,
    cleaning: Option<Cleaning>,
    report: Option<CleanReport>,
    deleted_nodes: Vec<NodeId>,

    focused: Option<NodeId>,
    tree_view: TreeView,
    treemap_view: TreemapView,
    plan_view: PlanView,

    script: Script,
    frames_since_ready: u32,
    logo: egui::TextureHandle,
}

impl App {
    fn new(context: egui::Context, config: Config, root: PathBuf, script: Script) -> Self {
        let logo = brand::logo(&context);
        let mut app = Self {
            context,
            config,
            volume: None,
            drives: Vec::new(),
            root,
            tree: None,
            tree_generation: 0,
            content_generation: 0,
            scanning: None,
            scans_left_running: Vec::new(),
            scan_error: None,
            plan: Plan::default(),
            ratings: Ratings::new(),
            analysing: None,
            awaiting_confirmation: None,
            cleaning: None,
            report: None,
            deleted_nodes: Vec::new(),
            focused: None,
            tree_view: TreeView::new(),
            treemap_view: TreemapView::new(),
            plan_view: PlanView::new(),
            script,
            frames_since_ready: 0,
            logo,
        };
        app.measure_volumes();
        app.start_scan(false);
        app
    }

    fn measure_volumes(&mut self) {
        self.volume = winfs::volume_space(&self.root).ok();
        self.drives = winfs::fixed_drive_roots()
            .into_iter()
            .map(|root| Drive { space: winfs::volume_space(&root).ok(), root })
            .collect();
    }

    /// Node ids, the plan and everything the views remember belong to the drive being left.
    fn switch_to(&mut self, root: PathBuf) {
        self.scans_left_running.extend(self.scanning.take());
        self.root = root;
        self.tree = None;
        self.tree_generation += 1;
        self.content_generation += 1;
        self.scan_error = None;
        self.plan = Plan::default();
        self.ratings.clear();
        self.analysing = None;
        self.report = None;
        self.deleted_nodes.clear();
        self.focused = None;
        self.tree_view = TreeView::new();
        self.treemap_view = TreemapView::new();
        self.plan_view = PlanView::new();
        self.measure_volumes();
        self.context.send_viewport_cmd(egui::ViewportCommand::Title(window_title(&self.root)));

        let left_running = self
            .scans_left_running
            .iter()
            .position(|scanning| is_same_folder(&scanning.root, &self.root));
        match left_running {
            Some(index) => self.scanning = Some(self.scans_left_running.swap_remove(index)),
            None => self.start_scan(false),
        }
    }

    /// Shows the saved scan first (unless one is already on screen), then replays the change
    /// journal onto it, walking every folder only when that is impossible or asked for.
    fn start_scan(&mut self, force_full_scan: bool) {
        let progress = Arc::new(ScanProgress::default());
        let (sender, news) = mpsc::channel();
        let (root, context, shared_progress) =
            (self.root.clone(), self.context.clone(), Arc::clone(&progress));
        let shows_a_tree_already = self.tree.is_some();
        std::thread::spawn(move || {
            let current =
                snapshot::current_index(&root, &shared_progress, force_full_scan, |remembered| {
                    if !shows_a_tree_already {
                        let _ = sender.send(ScanNews::Remembered(remembered.tree()));
                        context.request_repaint();
                    }
                });
            let _ = sender.send(ScanNews::Finished(current.map(|current| current.index.tree())));
            context.request_repaint();
        });
        self.scan_error = None;
        self.scanning = Some(Scanning { root: self.root.clone(), progress, news });
    }

    fn start_analysis(&mut self) {
        let Some(tree) = self.tree.clone() else {
            return;
        };
        let (sender, analysed) = mpsc::channel();
        let (config, context, generation) =
            (self.config.clone(), self.context.clone(), self.tree_generation);
        std::thread::spawn(move || {
            for depth in [Depth::Quick, Depth::Thorough] {
                let plan = plan::analyze(&tree, &config, depth);
                let _ = sender.send(Analysis { tree_generation: generation, depth, plan });
                context.request_repaint();
            }
        });
        self.analysing = Some(analysed);
    }

    fn start_clean(&mut self, candidates: Vec<Candidate>) {
        let progress = Arc::new(CleanProgress::default());
        let (sender, finished) = mpsc::channel();
        let (root, context, shared_progress, chosen) =
            (self.root.clone(), self.context.clone(), Arc::clone(&progress), candidates.clone());
        std::thread::spawn(move || {
            let _ = sender.send(clean::clean(&root, &chosen, &shared_progress));
            context.request_repaint();
        });
        self.report = None;
        self.cleaning = Some(Cleaning {
            progress,
            finished,
            candidates,
            tree_generation: self.tree_generation,
        });
    }

    /// Node ids are only valid within one scan, so everything the views remember is carried
    /// over by path and the old plan is dropped until the new tree has been analysed.
    fn adopt_tree(&mut self, tree: Tree) {
        let remembered = self.tree.as_deref().map(|old| {
            let focused = self.focused.map(|node| old.path(node));
            (self.tree_view.expanded_paths(old), focused, old.path(self.treemap_view.zoom()))
        });
        let tree = Arc::new(tree);
        if let Some((expanded, focused, zoom)) = remembered {
            self.tree_view.restore_expanded(&tree, &expanded);
            self.focused = focused.and_then(|path| tree.find(&path));
            self.treemap_view.set_zoom(tree.find(&zoom).unwrap_or(ROOT));
        }
        self.tree_view.invalidate();
        self.tree = Some(tree);
        self.tree_generation += 1;
        self.content_generation += 1;
        self.deleted_nodes.clear();
        self.plan = Plan::default();
        self.ratings.clear();
        self.start_analysis();
    }

    fn adopt_plan(&mut self, plan: Plan) {
        self.plan_view.adopt(&self.plan, &plan);
        self.plan = plan;
        self.rate_targets();
    }

    fn rate_targets(&mut self) {
        self.ratings = self
            .plan
            .candidates
            .iter()
            .flat_map(|candidate| {
                candidate.targets.iter().map(|target| (target.node, candidate.safety))
            })
            .collect();
        self.content_generation += 1;
    }

    fn poll_background_work(&mut self) {
        self.scans_left_running.retain(Scanning::is_still_running);
        while let Some(news) =
            self.scanning.as_ref().and_then(|scanning| scanning.news.try_recv().ok())
        {
            match news {
                ScanNews::Remembered(tree) => self.adopt_tree(tree),
                ScanNews::Finished(result) => {
                    self.scanning = None;
                    self.measure_volumes();
                    match result {
                        Ok(tree) => self.adopt_tree(tree),
                        Err(error) => self.scan_error = Some(error.to_string()),
                    }
                }
            }
        }
        while let Some(analysis) =
            self.analysing.as_ref().and_then(|analysed| analysed.try_recv().ok())
        {
            if analysis.depth == Depth::Thorough {
                self.analysing = None;
            }
            if analysis.tree_generation == self.tree_generation {
                self.adopt_plan(analysis.plan);
            }
        }
        if let Some(report) =
            self.cleaning.as_ref().and_then(|cleaning| cleaning.finished.try_recv().ok())
        {
            let cleaning = self.cleaning.take().unwrap();
            self.finish_clean(cleaning, report);
        }
        self.forget_deleted_nodes();
    }

    fn finish_clean(&mut self, cleaning: Cleaning, report: CleanReport) {
        let removed: HashSet<&Path> = report
            .results
            .iter()
            .filter(|result| result.outcome == Outcome::Removed)
            .map(|result| result.path.as_path())
            .collect();
        if cleaning.tree_generation == self.tree_generation {
            let removed_targets = cleaning
                .candidates
                .iter()
                .flat_map(|candidate| &candidate.targets)
                .filter(|target| removed.contains(target.path.as_path()));
            self.deleted_nodes.extend(removed_targets.map(|target| target.node));
            let kept: Vec<bool> = self
                .plan
                .candidates
                .iter()
                .map(|candidate| {
                    !candidate.targets.iter().all(|target| removed.contains(target.path.as_path()))
                })
                .collect();
            let mut still_listed = kept.iter().copied();
            self.plan.candidates.retain(|_| still_listed.next().unwrap());
            self.plan_view.keep_candidates(&kept);
            self.rate_targets();
        }
        self.measure_volumes();
        self.report = Some(report);
        if self.scanning.is_none() {
            self.start_scan(false);
        }
    }

    /// The tree is shared with background jobs; totals are corrected once it is ours alone.
    fn forget_deleted_nodes(&mut self) {
        if self.deleted_nodes.is_empty() {
            return;
        }
        let Some(tree) = self.tree.as_mut().and_then(Arc::get_mut) else {
            self.context.request_repaint_after(Duration::from_millis(200));
            return;
        };
        for node in self.deleted_nodes.drain(..) {
            tree.forget(node);
        }
        self.tree_view.invalidate();
        self.content_generation += 1;
    }

    fn apply(&mut self, intents: Vec<Intent>) {
        let Some(tree) = self.tree.clone() else {
            return;
        };
        for intent in intents {
            match intent {
                Intent::Focus(node) => {
                    self.focused = Some(node);
                    self.tree_view.reveal(&tree, node);
                }
                Intent::Zoom(node) => self.treemap_view.set_zoom(node),
                Intent::Delete(node) => {
                    self.awaiting_confirmation = Some(vec![manual_candidate(&tree, node)]);
                }
                Intent::Clean(chosen) => {
                    let candidates =
                        chosen.iter().map(|&index| self.plan.candidates[index].clone());
                    self.awaiting_confirmation = Some(candidates.collect());
                }
            }
        }
    }

    fn show_header(&mut self, context: &egui::Context) {
        let frame = egui::Frame::new()
            .fill(theme::BACKGROUND)
            .inner_margin(egui::Margin::symmetric(16, 12));
        egui::TopBottomPanel::top("header").frame(frame).show(context, |ui| {
            ui.horizontal(|ui| {
                let logo_size = egui::Vec2::splat(brand::HEADER_LOGO_POINTS);
                ui.image((self.logo.id(), logo_size));
                ui.label(RichText::new("Reclaim").size(21.0).strong());
                self.show_drives(ui);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let is_idle = self.scanning.is_none();
                    let full_scan = egui::Button::new("Full scan");
                    if ui
                        .add_enabled(is_idle, full_scan)
                        .on_hover_text("walk every folder again; takes minutes on a busy volume")
                        .clicked()
                    {
                        self.start_scan(true);
                    }
                    if ui
                        .add_enabled(is_idle, egui::Button::new("Refresh"))
                        .on_hover_text("re-read only the folders the change journal says changed")
                        .clicked()
                    {
                        self.start_scan(false);
                    }
                    self.show_scan_status(ui);
                });
            });
            ui.add_space(6.0);
            if let Some(volume) = self.volume {
                show_volume(ui, volume, &self.plan);
            }
        });
    }

    /// One button per local disk; a scan of a folder shows that folder beside them.
    fn show_drives(&mut self, ui: &mut egui::Ui) {
        let may_switch = self.cleaning.is_none();
        let mut chosen = None;
        for drive in &self.drives {
            let letter = drive.root.display().to_string();
            let label = match drive.space {
                Some(space) => {
                    format!(
                        "{}  {} free",
                        letter.trim_end_matches('\\'),
                        format::bytes(space.free_bytes)
                    )
                }
                None => letter,
            };
            let is_shown = is_same_folder(&drive.root, &self.root);
            let button = egui::Button::new(RichText::new(label).monospace()).selected(is_shown);
            let clicked = ui
                .add_enabled(may_switch, button)
                .on_hover_text(format!("show {}", drive.root.display()))
                .clicked();
            if clicked && !is_shown {
                chosen = Some(drive.root.clone());
            }
        }
        let shows_whole_drive =
            self.drives.iter().any(|drive| is_same_folder(&drive.root, &self.root));
        if !shows_whole_drive {
            ui.label(
                RichText::new(self.root.display().to_string()).monospace().color(theme::MUTED),
            );
        }
        if let Some(root) = chosen {
            self.switch_to(root);
        }
    }

    fn show_scan_status(&self, ui: &mut egui::Ui) {
        if let Some(scanning) = &self.scanning {
            let status = format!(
                "{}  ·  {} files  ·  {}",
                scanning.progress.status(),
                format::count(scanning.progress.files.load(Ordering::Relaxed)),
                format::bytes(scanning.progress.bytes.load(Ordering::Relaxed)),
            );
            ui.label(RichText::new(status).monospace().color(theme::ACCENT));
            ui.spinner();
            self.context.request_repaint_after(Duration::from_millis(120));
        }
        if let Some(error) = &self.scan_error {
            ui.label(RichText::new(format!("scan failed: {error}")).color(theme::DANGER));
        }
        if let Some(tree) = &self.tree {
            let stats = tree.stats;
            let age = format::age((winfs::unix_seconds_now() - stats.finished_unix).max(0));
            let pass = match stats.refreshed_directories {
                Some(changed) => format!("{} changed folders re-read", format::count(changed)),
                None => "every folder walked".to_string(),
            };
            let scanned = format!(
                "{} files in {} folders  ·  {pass} {age} ago in {:.1}s",
                format::count(stats.files),
                format::count(stats.directories),
                stats.elapsed.as_secs_f64(),
            );
            ui.label(RichText::new(scanned).small().color(theme::MUTED));
        }
    }

    fn show_first_scan(&self, context: &egui::Context) {
        egui::CentralPanel::default().show(context, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.36);
                let Some(scanning) = &self.scanning else {
                    let message = self.scan_error.as_deref().unwrap_or("nothing scanned yet");
                    ui.label(RichText::new(message).color(theme::DANGER));
                    return;
                };
                let counted = format::bytes(scanning.progress.bytes.load(Ordering::Relaxed));
                ui.label(RichText::new(counted).size(44.0).monospace().color(theme::ACCENT));
                let files = format::count(scanning.progress.files.load(Ordering::Relaxed));
                let status = format!("{}  ·  {files} files on {}", scanning.progress.status(), self.root.display());
                ui.label(RichText::new(status).color(theme::MUTED));
                ui.add_space(10.0);
                let note = "The first scan reads every folder once. After this the window opens instantly on the last result.";
                ui.label(RichText::new(note).small().color(theme::FAINT));
            });
        });
    }

    fn show_confirmation(&mut self, context: &egui::Context) {
        let Some(candidates) = &self.awaiting_confirmation else {
            return;
        };
        let mut decision = None;
        let modal = egui::Modal::new(egui::Id::new("confirm-clean")).show(context, |ui| {
            ui.set_width(560.0);
            let bytes: u64 = candidates.iter().map(|candidate| candidate.frees).sum();
            let items: usize = candidates.iter().map(|candidate| candidate.targets.len()).sum();
            ui.label(RichText::new(format!("Delete {items} items for good?")).size(18.0).strong());
            let summary = format!(
                "About {} comes back. Nothing goes to the recycle bin. Anything still open is skipped.",
                format::bytes(bytes)
            );
            ui.label(RichText::new(summary).color(theme::MUTED));
            ui.add_space(8.0);
            for candidate in candidates.iter().take(CANDIDATES_LISTED_IN_CONFIRMATION) {
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 4.0, theme::safety_color(candidate.safety));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(format::bytes(candidate.frees)).monospace());
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            let line = format!("{}  ·  {}", candidate.title, candidate.detail);
                            ui.add(egui::Label::new(line).truncate());
                        });
                    });
                });
            }
            if candidates.len() > CANDIDATES_LISTED_IN_CONFIRMATION {
                let more = candidates.len() - CANDIDATES_LISTED_IN_CONFIRMATION;
                ui.label(RichText::new(format!("… and {more} more")).small().color(theme::MUTED));
            }
            let needs_judgement = candidates.iter().filter(|candidate| candidate.safety == Safety::Review).count();
            if needs_judgement > 0 {
                ui.add_space(6.0);
                let warning = format!("{needs_judgement} of these are rated “your call”, not “safe”.");
                ui.label(RichText::new(warning).color(theme::REVIEW));
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let delete = egui::Button::new(RichText::new("Delete permanently").strong().color(theme::BACKGROUND))
                    .fill(theme::DANGER);
                if ui.add(delete).clicked() {
                    decision = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    decision = Some(false);
                }
            });
        });
        if modal.should_close() {
            decision.get_or_insert(false);
        }
        match decision {
            Some(true) => {
                let candidates = self.awaiting_confirmation.take().unwrap();
                self.start_clean(candidates);
            }
            Some(false) => self.awaiting_confirmation = None,
            None => {}
        }
    }

    fn take_screenshot_when_ready(&mut self, context: &egui::Context) {
        let Some(file) = self.script.screenshot.clone() else {
            return;
        };
        let captured = context.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(Arc::clone(image)),
                _ => None,
            })
        });
        if let Some(image) = captured {
            let [width, height] = image.size;
            let saved = image::save_buffer(
                &file,
                image.as_raw(),
                width as u32,
                height as u32,
                image::ColorType::Rgba8,
            );
            if let Err(error) = saved {
                eprintln!("reclaim: screenshot not saved: {error}");
            }
            context.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let is_settled = self.tree.is_some() && self.scanning.is_none() && self.analysing.is_none();
        if is_settled && self.script.click.is_none() {
            self.frames_since_ready += 1;
            if self.frames_since_ready == FRAMES_BEFORE_SCREENSHOT {
                context.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                    egui::UserData::default(),
                ));
            }
        }
        context.request_repaint_after(Duration::from_millis(50));
    }
}

impl eframe::App for App {
    /// The scripted click arrives as one move, press and release, after whatever the real
    /// pointer did this frame.
    fn raw_input_hook(&mut self, _context: &egui::Context, raw_input: &mut egui::RawInput) {
        if self.tree.is_none() {
            return;
        }
        let Some([x, y]) = self.script.click.take() else {
            return;
        };
        let position = Pos2::new(x, y);
        let button = |pressed| egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        raw_input.events.extend([egui::Event::PointerMoved(position), button(true), button(false)]);
    }

    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_background_work();
        self.show_header(context);

        let Some(tree) = self.tree.clone() else {
            self.show_first_scan(context);
            self.take_screenshot_when_ready(context);
            return;
        };
        let mut intents = Vec::new();

        let plan_frame = egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::same(12));
        egui::SidePanel::right("plan")
            .frame(plan_frame)
            .default_width(480.0)
            .width_range(380.0..=720.0)
            .show(context, |ui| {
                let activity = match (&self.cleaning, &self.analysing) {
                    (Some(cleaning), _) => Activity::Cleaning(&cleaning.progress),
                    (None, Some(_)) => Activity::Analysing,
                    (None, None) => Activity::Idle,
                };
                intents.extend(self.plan_view.show(
                    ui,
                    &tree,
                    &self.plan,
                    activity,
                    self.report.as_ref(),
                ));
            });

        let flush = egui::Frame::new().fill(theme::PANEL);
        egui::TopBottomPanel::bottom("treemap")
            .frame(flush)
            .resizable(true)
            .default_height(400.0)
            .height_range(160.0..=760.0)
            .show(context, |ui| {
                ui.add_space(4.0);
                intents.extend(self.treemap_view.show(
                    ui,
                    &tree,
                    self.focused,
                    &self.ratings,
                    self.content_generation,
                ));
            });

        egui::CentralPanel::default().frame(flush).show(context, |ui| {
            let now_unix = winfs::unix_seconds_now();
            intents.extend(self.tree_view.show(ui, &tree, self.focused, &self.ratings, now_unix));
        });

        drop(tree);
        self.apply(intents);
        self.show_confirmation(context);
        self.take_screenshot_when_ready(context);
    }
}

fn manual_candidate(tree: &Tree, node: NodeId) -> Candidate {
    let details = tree.node(node);
    let path = tree.path(node);
    let is_directory = tree.is_directory(node);
    Candidate {
        kind: if is_directory { Kind::StaleFolder } else { Kind::LargeFile },
        safety: Safety::Review,
        title: path.display().to_string(),
        detail: "picked by hand in the tree".to_string(),
        reason: "not checked against any clean-up rule".to_string(),
        location: node,
        size: details.size,
        frees: details.frees,
        action: Action::DeleteTrees,
        targets: vec![Target { node, path: path.clone(), frees: details.frees }],
        occupancy_root: is_directory.then_some(path),
    }
}

/// One bar for the whole volume: what is used, what the plan can give back, what is free.
fn show_volume(ui: &mut egui::Ui, volume: VolumeSpace, plan: &Plan) {
    let total = volume.total_bytes.max(1);
    let used = total.saturating_sub(volume.free_bytes);
    let safe = plan.frees(Safety::Safe).min(used);
    let review = plan.frees(Safety::Review).min(used - safe);
    let free_share = volume.free_bytes as f32 / total as f32;
    let free_color = match free_share {
        share if share < 0.05 => theme::DANGER,
        share if share < 0.12 => theme::REVIEW,
        _ => theme::TEXT,
    };

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format::bytes(volume.free_bytes))
                .size(26.0)
                .monospace()
                .color(free_color),
        );
        ui.label(RichText::new(format!("free of {}", format::bytes(total))).color(theme::MUTED));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            legend(ui, theme::REVIEW, &format!("your call {}", format::bytes(review)));
            legend(ui, theme::SAFE, &format!("safe to remove {}", format::bytes(safe)));
            legend(ui, theme::USED, &format!("in use {}", format::bytes(used - safe - review)));
        });
    });

    let (bar, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 14.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(bar, 4.0, theme::RAISED);
    let mut left = bar.left();
    let segments =
        [(used - safe - review, theme::USED), (review, theme::REVIEW), (safe, theme::SAFE)];
    for (bytes, color) in segments {
        let width = bar.width() * bytes as f32 / total as f32;
        let segment =
            Rect::from_min_max(Pos2::new(left, bar.top()), Pos2::new(left + width, bar.bottom()));
        painter.rect_filled(segment, 0.0, color);
        left += width;
    }
}

fn legend(ui: &mut egui::Ui, color: egui::Color32, text: &str) {
    ui.label(RichText::new(text).small().color(theme::MUTED));
    let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
    ui.painter().circle_filled(dot.center(), 4.0, color);
    ui.add_space(6.0);
}
