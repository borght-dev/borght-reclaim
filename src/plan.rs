use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::activity::{describe_occupants, AgentSessions, Occupant, ProcessSnapshot};
use crate::format;
use crate::git::{self, MergedPullRequests, Safekeeping, WorkspaceLink, WorktreeState};
use crate::scan::{NodeId, Tree, ROOT};
use crate::territory::{self, Territory};

pub const TRASH_MARKER: &str = ".reclaim-trash-";
const SECONDS_PER_MINUTE: i64 = 60;
const SECONDS_PER_HOUR: i64 = 3600;
const SECONDS_PER_DAY: i64 = 24 * SECONDS_PER_HOUR;
const BYTES_PER_MEGABYTE: u64 = 1024 * 1024;
const CACHE_ENTRY_SETTLE_SECONDS: i64 = SECONDS_PER_DAY;
const BUN_ENTRY_MARKER: &str = "@@@";
const LARGE_FILES_SHOWN: usize = 40;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub root: PathBuf,
    pub scratch_directories: Vec<String>,
    pub scratch_max_age_days: i64,
    /// A workspace touched more recently than this is active: nothing in it is offered.
    pub active_within_minutes: i64,
    /// Build output of a workspace idle at least this long is removed without asking.
    pub build_output_idle_hours: i64,
    pub dependencies_idle_days: i64,
    pub npx_max_age_days: i64,
    /// A clean, pushed worktree is offered for removal after this long without activity.
    pub worktree_idle_days: i64,
    pub stale_folder_days: i64,
    pub stale_folder_megabytes: u64,
    pub large_file_megabytes: u64,
    pub minimum_candidate_megabytes: u64,
    pub protected_paths: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            root: system_drive(),
            scratch_directories: vec!["tmp".into(), "temp".into()],
            scratch_max_age_days: 7,
            active_within_minutes: 30,
            build_output_idle_hours: 6,
            dependencies_idle_days: 7,
            npx_max_age_days: 14,
            worktree_idle_days: 7,
            stale_folder_days: 60,
            stale_folder_megabytes: 100,
            large_file_megabytes: 256,
            minimum_candidate_megabytes: 10,
            protected_paths: Vec::new(),
        }
    }
}

fn system_drive() -> PathBuf {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    PathBuf::from(format!("{drive}\\"))
}

impl Config {
    pub fn file_path() -> Option<PathBuf> {
        Some(PathBuf::from(std::env::var_os("USERPROFILE")?).join(".reclaim.json"))
    }

    pub fn load() -> Result<Self, String> {
        let Some(path) = Self::file_path().filter(|path| path.exists()) else {
            return Ok(Self::default());
        };
        let json = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
        serde_json::from_str(&json).map_err(|error| format!("{}: {error}", path.display()))
    }

    fn is_protected(&self, path: &Path) -> bool {
        self.protected_paths.iter().any(|protected| path.starts_with(protected))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Kind {
    BuildOutput,
    Dependencies,
    PackageCache,
    Scratch,
    RecycleBin,
    Worktree,
    GlobalPackage,
    StaleFolder,
    LargeFile,
}

impl Kind {
    pub const ALL: [Kind; 9] = [
        Kind::BuildOutput,
        Kind::Dependencies,
        Kind::PackageCache,
        Kind::Scratch,
        Kind::RecycleBin,
        Kind::Worktree,
        Kind::GlobalPackage,
        Kind::StaleFolder,
        Kind::LargeFile,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Kind::BuildOutput => "Build output",
            Kind::Dependencies => "Dependencies",
            Kind::PackageCache => "Package caches",
            Kind::Scratch => "Scratch files",
            Kind::RecycleBin => "Recycle bin",
            Kind::Worktree => "Finished worktrees",
            Kind::GlobalPackage => "Global CLI packages",
            Kind::StaleFolder => "Stale folders",
            Kind::LargeFile => "Large files",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Safety {
    /// Regenerable and provably unused: selected by default.
    Safe,
    /// Removable, but the trade-off is yours to judge.
    Review,
    /// Belongs to work that is active or not saved elsewhere.
    Blocked,
}

#[derive(Clone, Debug, Serialize)]
pub enum Action {
    DeleteTrees,
    DeleteFiles,
    /// Rechecks at delete time that no project hardlinks into the entry any more.
    DeleteUnlinkedCacheEntries,
    EmptyRecycleBin,
    /// `verified_head` is the commit proven saved; a worktree that has moved on is left alone.
    RemoveWorktree {
        repository: PathBuf,
        regenerable: Vec<PathBuf>,
        verified_head: String,
    },
    UninstallGlobalPackage {
        prefix: PathBuf,
        package: String,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct Target {
    pub node: NodeId,
    pub path: PathBuf,
    pub frees: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub kind: Kind,
    pub safety: Safety,
    pub title: String,
    pub detail: String,
    pub reason: String,
    pub location: NodeId,
    pub size: u64,
    pub frees: u64,
    pub action: Action,
    pub targets: Vec<Target>,
    /// Re-inspected for running processes right before anything is deleted.
    pub occupancy_root: Option<PathBuf>,
}

#[derive(Default, Serialize)]
pub struct Plan {
    pub candidates: Vec<Candidate>,
}

impl Plan {
    pub fn frees(&self, safety: Safety) -> u64 {
        self.candidates
            .iter()
            .filter(|candidate| candidate.safety == safety)
            .map(|candidate| candidate.frees)
            .sum()
    }

    /// Sums what the chosen candidates free, counting a target inside another chosen target once.
    pub fn frees_together(&self, tree: &Tree, chosen: impl Fn(usize) -> bool) -> u64 {
        let targets: Vec<&Target> = self
            .candidates
            .iter()
            .enumerate()
            .filter(|(index, _)| chosen(*index))
            .flat_map(|(_, candidate)| &candidate.targets)
            .collect();
        let chosen_nodes: HashSet<NodeId> = targets.iter().map(|target| target.node).collect();
        targets
            .iter()
            .filter(|target| !has_ancestor_in(tree, target.node, &chosen_nodes))
            .map(|target| target.frees)
            .sum()
    }
}

fn has_ancestor_in(tree: &Tree, node: NodeId, nodes: &HashSet<NodeId>) -> bool {
    let mut current = node;
    while current != ROOT {
        current = tree.node(current).parent;
        if nodes.contains(&current) {
            return true;
        }
    }
    false
}

pub struct Environment<'a> {
    pub processes: &'a ProcessSnapshot,
    pub sessions: &'a AgentSessions,
    pub now_unix: i64,
    pub depth: Depth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    /// Everything that needs no `git status`: ready in seconds.
    Quick,
    /// Also whole worktrees and repositories, each verified through git and merged pull requests.
    Thorough,
}

/// Judges idleness as of the scan, so a plan made from an older snapshot never counts the
/// time since then as idle time; processes, git and agent sessions are read live.
pub fn analyze(tree: &Tree, config: &Config, depth: Depth) -> Plan {
    let processes = ProcessSnapshot::capture();
    let sessions = AgentSessions::discover();
    let environment = Environment {
        processes: &processes,
        sessions: &sessions,
        now_unix: tree.stats.finished_unix,
        depth,
    };
    build_plan(tree, config, &environment)
}

pub fn build_plan(tree: &Tree, config: &Config, environment: &Environment) -> Plan {
    let mut discovery = Discovery::new(tree, config);
    discovery.run();
    let Discovery {
        mut workspaces,
        scratch_directories,
        package_caches,
        global_package_prefixes,
        recycle_bin,
        large_files,
        leftover_trash,
        ..
    } = discovery;

    keep_only_proven_build_output(tree, &mut workspaces);
    let activities = workspace_activities(tree, &workspaces, environment);
    let builder = Builder { tree, config, environment };

    let mut candidates = Vec::new();
    for (workspace, activity) in workspaces.iter().zip(&activities) {
        if workspace.is_ours_to_clean() {
            candidates.extend(builder.generated_candidates(workspace, activity));
        }
    }
    candidates.extend(builder.worktree_candidates(&workspaces, &activities));
    candidates.extend(builder.stale_folder_candidates(&workspaces, &scratch_directories));
    for &scratch in &scratch_directories {
        candidates.extend(builder.scratch_candidates(scratch));
    }
    for &cache in &package_caches {
        candidates.extend(builder.package_cache_candidates(cache));
    }
    for &prefix in &global_package_prefixes {
        candidates.extend(builder.global_package_candidates(prefix));
    }
    candidates.extend(recycle_bin.and_then(|bin| builder.recycle_bin_candidate(bin)));
    candidates.extend(builder.leftover_trash_candidate(&leftover_trash));
    candidates.extend(builder.large_file_candidates(large_files));

    let minimum_bytes = config.minimum_candidate_megabytes * BYTES_PER_MEGABYTE;
    candidates.retain(|candidate| candidate.size.max(candidate.frees) >= minimum_bytes);
    candidates.sort_by_key(|candidate| (candidate.safety, std::cmp::Reverse(candidate.frees)));
    Plan { candidates }
}

struct Generated {
    node: NodeId,
    kind: Kind,
    label: &'static str,
    needs_ignore_proof: bool,
}

struct Workspace {
    root: NodeId,
    path: PathBuf,
    link: Option<WorkspaceLink>,
    git_directory: Option<PathBuf>,
    newest_source_unix: i64,
    generated: Vec<Generated>,
    is_worktree_slot: bool,
    contains_workspaces: bool,
    territory: Territory,
}

impl Workspace {
    /// Inside a program's own folder only worktree checkouts are someone's project; anything
    /// else there is the program itself.
    fn is_ours_to_clean(&self) -> bool {
        self.territory == Territory::Projects
            || self.is_worktree_slot
            || matches!(self.link, Some(WorkspaceLink::LinkedWorktree { .. }))
    }
}

#[derive(Default)]
struct Markers {
    has_git: bool,
    dotnet_project: bool,
    package_json: bool,
    cargo_toml: bool,
    is_project_root: bool,
    is_package_cache: bool,
    /// `npm install -g` target: packages in `node_modules` with npm's launcher pairs
    /// (`tool.cmd` + `tool.ps1`) beside it. Its packages are installed tools, not project
    /// dependencies, even when someone left a `package.json` there.
    is_global_package_prefix: bool,
}

impl Markers {
    fn read(tree: &Tree, directory: NodeId) -> Self {
        let mut markers = Markers::default();
        let mut bun_entries = 0;
        let mut has_node_modules = false;
        let mut command_launchers = HashSet::new();
        let mut powershell_launchers = HashSet::new();
        for child in tree.children(directory) {
            let name = tree.name(child);
            if tree.is_directory(child) {
                markers.has_git |= name == ".git";
                markers.is_package_cache |= name == "_cacache";
                has_node_modules |= name == "node_modules";
                bun_entries += usize::from(name.contains(BUN_ENTRY_MARKER));
                continue;
            }
            let lowercase = name.to_ascii_lowercase();
            if let Some(tool) = lowercase.strip_suffix(".cmd") {
                command_launchers.insert(tool.to_string());
            } else if let Some(tool) = lowercase.strip_suffix(".ps1") {
                powershell_launchers.insert(tool.to_string());
            }
            markers.has_git |= lowercase == ".git";
            markers.package_json |= lowercase == "package.json";
            markers.cargo_toml |= lowercase == "cargo.toml";
            let is_dotnet_project =
                [".csproj", ".fsproj", ".vbproj"].iter().any(|suffix| lowercase.ends_with(suffix));
            markers.dotnet_project |= is_dotnet_project;
            markers.is_project_root |= is_dotnet_project
                || matches!(lowercase.as_str(), "pyproject.toml" | "go.mod" | ".gitignore")
                || lowercase.ends_with(".sln")
                || lowercase.ends_with(".slnx");
        }
        markers.is_project_root |= markers.package_json || markers.cargo_toml;
        markers.is_package_cache |= bun_entries >= 3;
        markers.is_global_package_prefix =
            has_node_modules && !command_launchers.is_disjoint(&powershell_launchers);
        markers
    }
}

fn generated_rule(tree: &Tree, directory: NodeId, markers: &Markers) -> Option<Generated> {
    let name = tree.name(directory).to_ascii_lowercase();
    let build = |label| Some((Kind::BuildOutput, label, false));
    let rule = match name.as_str() {
        "node_modules" if markers.package_json && !markers.is_global_package_prefix => {
            Some((Kind::Dependencies, "node_modules", false))
        }
        artifacts if artifacts.starts_with(".artifacts") && markers.is_project_root => {
            Some((Kind::BuildOutput, "build artifacts", true))
        }
        "bin" | "obj" if markers.dotnet_project => build(".NET bin/obj"),
        "target" if markers.cargo_toml => build("cargo target"),
        ".next" | ".nuxt" | ".turbo" | ".angular" | ".svelte-kit" | ".parcel-cache"
        | "storybook-static"
            if markers.package_json =>
        {
            build("JS build cache")
        }
        "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache" => build("Python cache"),
        "testresults" if markers.is_project_root => build("test results"),
        "playwright-report" | "test-results" if markers.package_json => build("test results"),
        "dist" | "out" | "build" if markers.package_json => {
            Some((Kind::BuildOutput, "JS build output", true))
        }
        ".venv" | "venv" if tree.child_named(directory, "pyvenv.cfg").is_some() => {
            Some((Kind::Dependencies, "Python venv", false))
        }
        _ => None,
    };
    rule.map(|(kind, label, needs_ignore_proof)| Generated {
        node: directory,
        kind,
        label,
        needs_ignore_proof,
    })
}

struct Discovery<'a> {
    tree: &'a Tree,
    config: &'a Config,
    workspaces: Vec<Workspace>,
    scratch_directories: Vec<NodeId>,
    package_caches: Vec<NodeId>,
    global_package_prefixes: Vec<NodeId>,
    recycle_bin: Option<NodeId>,
    large_files: Vec<NodeId>,
    leftover_trash: Vec<NodeId>,
    root_depth_below_drive: usize,
}

struct Visit {
    directory: NodeId,
    workspace: Option<usize>,
    /// Folders up to this many levels below a `worktrees` folder count as worktree checkouts,
    /// which covers both `worktrees\<branch>` and `worktrees\<repository>\<branch>`.
    worktree_slot_levels: u8,
    territory: Territory,
    depth_below_root: usize,
}

const WORKTREE_SLOT_LEVELS: u8 = 2;
const PAGING_FILES: [&str; 3] = ["pagefile.sys", "hiberfil.sys", "swapfile.sys"];

impl<'a> Discovery<'a> {
    fn new(tree: &'a Tree, config: &'a Config) -> Self {
        Self {
            tree,
            config,
            workspaces: Vec::new(),
            scratch_directories: Vec::new(),
            package_caches: Vec::new(),
            global_package_prefixes: Vec::new(),
            recycle_bin: None,
            large_files: Vec::new(),
            leftover_trash: Vec::new(),
            root_depth_below_drive: territory::depth_below_drive(tree.root_path()),
        }
    }

    fn run(&mut self) {
        let mut pending = vec![Visit {
            directory: ROOT,
            workspace: None,
            worktree_slot_levels: 0,
            territory: Territory::Projects,
            depth_below_root: 0,
        }];
        while let Some(visit) = pending.pop() {
            self.visit(visit, &mut pending);
        }
    }

    fn visit(&mut self, visit: Visit, pending: &mut Vec<Visit>) {
        let tree = self.tree;
        let directory = visit.directory;
        let markers = Markers::read(tree, directory);
        if markers.is_package_cache {
            self.package_caches.push(directory);
            return;
        }
        if markers.is_global_package_prefix {
            self.global_package_prefixes.push(directory);
        }

        let mut generated: Vec<Generated> = tree
            .children(directory)
            .filter(|&child| tree.is_directory(child) && !tree.is_link(child))
            .filter_map(|child| generated_rule(tree, child, &markers))
            .collect();
        let generated_nodes: HashSet<NodeId> = generated.iter().map(|item| item.node).collect();

        let is_worktree_slot = visit.worktree_slot_levels > 0 && markers.is_project_root;
        let opens_workspace = markers.has_git
            || is_worktree_slot
            || (visit.workspace.is_none() && !generated.is_empty());
        let workspace = if opens_workspace {
            self.open_workspace(directory, visit.workspace, is_worktree_slot, visit.territory)
        } else {
            visit.workspace
        };
        let slot_levels_below =
            if opens_workspace { 0 } else { visit.worktree_slot_levels.saturating_sub(1) };
        if let Some(index) = workspace {
            self.workspaces[index].generated.append(&mut generated);
        }

        let large_file_bytes = self.config.large_file_megabytes * BYTES_PER_MEGABYTE;
        for child in tree.children(directory) {
            let node = tree.node(child);
            if !tree.is_directory(child) {
                if let Some(index) = workspace {
                    let newest = &mut self.workspaces[index].newest_source_unix;
                    *newest = (*newest).max(node.modified_unix);
                }
                let is_paging_file = directory == ROOT
                    && PAGING_FILES
                        .iter()
                        .any(|paging| paging.eq_ignore_ascii_case(tree.name(child)));
                if node.frees >= large_file_bytes && !is_paging_file {
                    self.large_files.push(child);
                }
                continue;
            }
            let name = tree.name(child);
            let never_entered = matches!(name, ".git" | "node_modules");
            if tree.is_link(child) || never_entered || generated_nodes.contains(&child) {
                continue;
            }
            if name.contains(TRASH_MARKER) {
                self.leftover_trash.push(child);
                continue;
            }
            if directory == ROOT && self.claims_top_level_directory(child, name) {
                continue;
            }
            if !self.config.protected_paths.is_empty()
                && self.config.is_protected(&tree.path(child))
            {
                continue;
            }
            let territory = self.territory_of(child, &visit);
            if territory == Territory::InstalledSoftware {
                continue;
            }
            let holds_worktrees = matches!(name, ".worktrees" | "worktrees");
            pending.push(Visit {
                directory: child,
                workspace,
                worktree_slot_levels: if holds_worktrees {
                    WORKTREE_SLOT_LEVELS
                } else {
                    slot_levels_below
                },
                territory,
                depth_below_root: visit.depth_below_root + 1,
            });
        }
    }

    /// A territory starts at a folder met during the walk; pointing the scan at a folder
    /// inside one treats that folder as project space.
    fn territory_of(&self, child: NodeId, parent: &Visit) -> Territory {
        let child_depth_below_drive = self.root_depth_below_drive + parent.depth_below_root + 1;
        let may_start_territory = parent.territory == Territory::Projects
            && child_depth_below_drive <= territory::DEEPEST_START;
        if may_start_territory {
            Territory::starting_at(&self.tree.path(child))
        } else {
            parent.territory
        }
    }

    fn claims_top_level_directory(&mut self, directory: NodeId, name: &str) -> bool {
        if name.eq_ignore_ascii_case("$RECYCLE.BIN") {
            self.recycle_bin = Some(directory);
            return true;
        }
        if self.config.scratch_directories.iter().any(|scratch| scratch.eq_ignore_ascii_case(name))
        {
            self.scratch_directories.push(directory);
            return true;
        }
        name.eq_ignore_ascii_case("System Volume Information")
    }

    fn open_workspace(
        &mut self,
        root: NodeId,
        enclosing: Option<usize>,
        is_worktree_slot: bool,
        territory: Territory,
    ) -> Option<usize> {
        let path = self.tree.path(root);
        let location = git::locate(&path);
        if matches!(&location, Some(found) if found.link == WorkspaceLink::Submodule) {
            return enclosing;
        }
        if let Some(enclosing) = enclosing {
            self.workspaces[enclosing].contains_workspaces = true;
        }
        let (link, git_directory) = match location {
            Some(found) => (Some(found.link), Some(found.git_directory)),
            None => (None, None),
        };
        self.workspaces.push(Workspace {
            root,
            path,
            link,
            git_directory,
            newest_source_unix: 0,
            generated: Vec::new(),
            is_worktree_slot,
            contains_workspaces: false,
            territory,
        });
        Some(self.workspaces.len() - 1)
    }
}

/// `dist`, `out` and `build` are only build output when git ignores them and tracks nothing inside.
fn keep_only_proven_build_output(tree: &Tree, workspaces: &mut [Workspace]) {
    workspaces.par_iter_mut().for_each(|workspace| {
        let unproven: Vec<PathBuf> = workspace
            .generated
            .iter()
            .filter(|generated| generated.needs_ignore_proof)
            .map(|generated| tree.path(generated.node))
            .collect();
        if unproven.is_empty() || workspace.link.is_none() {
            return;
        }
        let proven = git::ignored_untracked(&workspace.path, &unproven);
        let mut unproven_paths = unproven.into_iter();
        workspace.generated.retain_mut(|generated| {
            if !generated.needs_ignore_proof {
                return true;
            }
            let path = unproven_paths.next().unwrap();
            generated.needs_ignore_proof = false;
            proven.contains(&path)
        });
    });
}

struct WorkspaceActivity {
    occupants: Vec<Occupant>,
    last_activity_unix: i64,
    last_activity_kind: &'static str,
}

fn workspace_activities(
    tree: &Tree,
    workspaces: &[Workspace],
    environment: &Environment,
) -> Vec<WorkspaceActivity> {
    let roots: Vec<PathBuf> = workspaces.iter().map(|workspace| workspace.path.clone()).collect();
    let occupants = environment.processes.occupants(&roots);
    workspaces
        .iter()
        .zip(occupants)
        .map(|(workspace, occupants)| {
            let newest_generated = workspace
                .generated
                .iter()
                .map(|generated| tree.node(generated.node).modified_unix)
                .max();
            let git_activity = workspace.git_directory.as_deref().and_then(git::last_activity_unix);
            let agent_activity = environment.sessions.last_activity_unix(&workspace.path);
            let (last_activity_unix, last_activity_kind) = [
                (Some(workspace.newest_source_unix), "source edit"),
                (newest_generated, "build or install"),
                (git_activity, "git commit or checkout"),
                (agent_activity, "agent session"),
            ]
            .into_iter()
            .filter_map(|(when, kind)| Some((when?, kind)))
            .max_by_key(|&(when, _)| when)
            .unwrap();
            WorkspaceActivity { occupants, last_activity_unix, last_activity_kind }
        })
        .collect()
}

struct Builder<'a> {
    tree: &'a Tree,
    config: &'a Config,
    environment: &'a Environment<'a>,
}

impl Builder<'_> {
    fn display_path(&self, node: NodeId) -> String {
        let path = self.tree.path(node);
        match path.strip_prefix(self.tree.root_path()) {
            Ok(relative) if !relative.as_os_str().is_empty() => relative.display().to_string(),
            _ => path.display().to_string(),
        }
    }

    fn target(&self, node: NodeId) -> Target {
        Target { node, path: self.tree.path(node), frees: self.tree.node(node).frees }
    }

    fn idle_seconds(&self, last_activity_unix: i64) -> i64 {
        (self.environment.now_unix - last_activity_unix).max(0)
    }

    fn age_seconds(&self, node: NodeId) -> i64 {
        self.idle_seconds(self.tree.node(node).modified_unix)
    }

    fn generated_safety(
        &self,
        kind: Kind,
        workspace: &Workspace,
        activity: &WorkspaceActivity,
    ) -> (Safety, String) {
        if !activity.occupants.is_empty() {
            let occupants = describe_occupants(&activity.occupants);
            return (Safety::Blocked, format!("workspace in use by {occupants}"));
        }
        let idle = self.idle_seconds(activity.last_activity_unix);
        let last = format!("last {} {} ago", activity.last_activity_kind, format::age(idle));
        if idle < self.config.active_within_minutes * SECONDS_PER_MINUTE {
            return (Safety::Blocked, format!("workspace active: {last}"));
        }
        let (threshold, threshold_text) = match kind {
            Kind::Dependencies => (
                self.config.dependencies_idle_days * SECONDS_PER_DAY,
                format!("{}d", self.config.dependencies_idle_days),
            ),
            _ => (
                self.config.build_output_idle_hours * SECONDS_PER_HOUR,
                format!("{}h", self.config.build_output_idle_hours),
            ),
        };
        if idle < threshold {
            let reason = format!(
                "no running process, but {last} (under {threshold_text}); needs a rebuild if work resumes"
            );
            return (Safety::Review, reason);
        }
        if workspace.link.is_none() && kind == Kind::BuildOutput && has_unproven(workspace) {
            return (Safety::Review, format!("{last}; not a git checkout, so unverified"));
        }
        (Safety::Safe, format!("regenerable, no running process, {last}"))
    }

    fn generated_candidates(
        &self,
        workspace: &Workspace,
        activity: &WorkspaceActivity,
    ) -> Vec<Candidate> {
        let mut by_kind: BTreeMap<Kind, Vec<&Generated>> = BTreeMap::new();
        for generated in &workspace.generated {
            by_kind.entry(generated.kind).or_default().push(generated);
        }
        by_kind
            .into_iter()
            .map(|(kind, generated)| {
                let (safety, reason) = self.generated_safety(kind, workspace, activity);
                let targets: Vec<Target> =
                    generated.iter().map(|item| self.target(item.node)).collect();
                Candidate {
                    kind,
                    safety,
                    title: self.display_path(workspace.root),
                    detail: count_by_label(&generated),
                    reason,
                    location: workspace.root,
                    size: generated.iter().map(|item| self.tree.node(item.node).size).sum(),
                    frees: targets.iter().map(|target| target.frees).sum(),
                    action: Action::DeleteTrees,
                    targets,
                    occupancy_root: Some(workspace.path.clone()),
                }
            })
            .collect()
    }

    fn worktree_candidates(
        &self,
        workspaces: &[Workspace],
        activities: &[WorkspaceActivity],
    ) -> Vec<Candidate> {
        if self.environment.depth == Depth::Quick {
            return Vec::new();
        }
        let quiet_threshold = self.config.active_within_minutes * SECONDS_PER_MINUTE;
        let quiet: Vec<(&Workspace, i64)> = workspaces
            .iter()
            .zip(activities)
            .filter(|(workspace, activity)| {
                activity.occupants.is_empty()
                    && !workspace.contains_workspaces
                    && self.idle_seconds(activity.last_activity_unix) >= quiet_threshold
            })
            .map(|(workspace, activity)| {
                (workspace, self.idle_seconds(activity.last_activity_unix))
            })
            .collect();

        let repositories: BTreeSet<&Path> = quiet
            .iter()
            .filter_map(|(workspace, _)| match &workspace.link {
                Some(WorkspaceLink::LinkedWorktree { repository }) => Some(repository.as_path()),
                _ => None,
            })
            .collect();
        let merged_by_repository: HashMap<&Path, MergedPullRequests> = repositories
            .into_par_iter()
            .map(|repository| (repository, git::merged_pull_requests(repository)))
            .collect();

        let long_idle = self.config.worktree_idle_days * SECONDS_PER_DAY;
        quiet
            .par_iter()
            .filter_map(|&(workspace, idle_seconds)| match &workspace.link {
                Some(WorkspaceLink::LinkedWorktree { repository }) => self
                    .linked_worktree_candidate(
                        workspace,
                        repository,
                        idle_seconds,
                        &merged_by_repository[repository.as_path()],
                    ),
                None if workspace.is_worktree_slot && idle_seconds >= long_idle => {
                    Some(self.orphaned_worktree_candidate(workspace, idle_seconds))
                }
                _ => None,
            })
            .collect()
    }

    /// Offered when removing it destroys nothing: no uncommitted or untracked files, and the
    /// reason says where its commits live on (a merged pull request, a remote, another local
    /// branch). A worktree whose commits exist only in its own branch waits until it has sat
    /// idle; one with unsaved files is only listed, as blocked, once it looks forgotten.
    fn linked_worktree_candidate(
        &self,
        workspace: &Workspace,
        repository: &Path,
        idle_seconds: i64,
        merged: &MergedPullRequests,
    ) -> Option<Candidate> {
        let idle_text = format::age(idle_seconds);
        let is_long_idle = idle_seconds >= self.config.worktree_idle_days * SECONDS_PER_DAY;
        // An agent between steps leaves no process behind; hours of silence are the only sign
        // from here that its thread is done with the checkout.
        let has_settled = idle_seconds >= self.config.build_output_idle_hours * SECONDS_PER_HOUR;
        let mut inspected = git::inspect_worktree(&workspace.path);
        if let Ok(state) = &mut inspected {
            let branch = state.branch.clone().unwrap_or_default();
            state.merged_pull_request = merged.get(&(branch.clone(), state.head.clone())).copied();
            let may_be_an_older_merge = state.merged_pull_request.is_none()
                && state.safekeeping() != Safekeeping::Pushed
                && !branch.is_empty();
            if may_be_an_older_merge {
                state.merged_pull_request =
                    git::merged_pull_request_of(repository, &branch, &state.head);
            }
        }
        let (safety, reason) = match &inspected {
            Err(error) if is_long_idle => {
                (Safety::Blocked, format!("git could not inspect it: {error}"))
            }
            Err(_) => return None,
            Ok(state) => {
                let proof = match (state.blocker(), state.safekeeping()) {
                    (Some(blocker), _) if is_long_idle => {
                        return Some(self.worktree_candidate(
                            workspace,
                            repository,
                            state,
                            Safety::Blocked,
                            format!("idle {idle_text}, but {blocker}"),
                        ));
                    }
                    (Some(_), _) => return None,
                    (None, Safekeeping::MergedPullRequest(number)) => format!(
                        "PR #{number} merged at this exact commit, nothing uncommitted, idle {idle_text}"
                    ),
                    (None, Safekeeping::OtherLocalBranch(other)) if has_settled => format!(
                        "idle {idle_text}, nothing uncommitted, every commit is also in local branch {other}"
                    ),
                    (None, Safekeeping::Pushed) if has_settled => {
                        format!("idle {idle_text}, nothing uncommitted, everything pushed")
                    }
                    (None, Safekeeping::OwnBranchOnly) if is_long_idle => format!(
                        "idle {idle_text}, nothing uncommitted; its commits exist only in this local branch"
                    ),
                    (None, _) => return None,
                };
                (Safety::Review, removable_worktree_reason(state, &proof))
            }
        };
        let state = inspected.unwrap_or_default();
        Some(self.worktree_candidate(workspace, repository, &state, safety, reason))
    }

    fn worktree_candidate(
        &self,
        workspace: &Workspace,
        repository: &Path,
        state: &WorktreeState,
        safety: Safety,
        reason: String,
    ) -> Candidate {
        let (branch, verified_head) = (state.branch.clone(), state.head.clone());
        let node = self.tree.node(workspace.root);
        Candidate {
            kind: Kind::Worktree,
            safety,
            title: self.display_path(workspace.root),
            detail: branch.map_or("detached HEAD".to_string(), |branch| format!("branch {branch}")),
            reason,
            location: workspace.root,
            size: node.size,
            frees: node.frees,
            action: Action::RemoveWorktree {
                repository: repository.to_path_buf(),
                regenerable: workspace
                    .generated
                    .iter()
                    .map(|generated| self.tree.path(generated.node))
                    .collect(),
                verified_head,
            },
            targets: vec![self.target(workspace.root)],
            occupancy_root: Some(workspace.path.clone()),
        }
    }

    /// Top-level folders nothing has touched for months. Folders holding git checkouts other
    /// than themselves are left out: their unsaved work cannot be judged from here.
    fn stale_folder_candidates(
        &self,
        workspaces: &[Workspace],
        scratch_directories: &[NodeId],
    ) -> Vec<Candidate> {
        let tree = self.tree;
        let stale_seconds = self.config.stale_folder_days * SECONDS_PER_DAY;
        let minimum_bytes = self.config.stale_folder_megabytes * BYTES_PER_MEGABYTE;
        let stale: Vec<NodeId> = tree
            .children(ROOT)
            .filter(|&folder| {
                let name = tree.name(folder);
                tree.is_directory(folder)
                    && !tree.is_link(folder)
                    && !scratch_directories.contains(&folder)
                    && !name.starts_with('$')
                    && !name.eq_ignore_ascii_case("System Volume Information")
                    && !name.contains(TRASH_MARKER)
                    && !territory::is_operating_system_folder(&tree.path(folder))
                    && tree.node(folder).frees >= minimum_bytes
                    && self.age_seconds(folder) >= stale_seconds
            })
            .filter(|&folder| {
                !workspaces.iter().any(|workspace| {
                    workspace.link.is_some()
                        && workspace.root != folder
                        && tree.is_inside(workspace.root, folder)
                })
            })
            .collect();

        let is_repository = |folder: NodeId| {
            workspaces.iter().any(|workspace| workspace.root == folder && workspace.link.is_some())
        };
        stale
            .par_iter()
            .filter(|&&folder| self.environment.depth == Depth::Thorough || !is_repository(folder))
            .map(|&folder| {
                let untouched =
                    format!("nothing modified for {}", format::age(self.age_seconds(folder)));
                let (safety, reason) = if !is_repository(folder) {
                    (Safety::Review, untouched)
                } else {
                    match git::repository_blocker(&tree.path(folder)) {
                        Ok(None) => (
                            Safety::Review,
                            format!("git repository, clean and pushed; {untouched}"),
                        ),
                        Ok(Some(blocker)) => {
                            (Safety::Blocked, format!("git repository with {blocker}"))
                        }
                        Err(error) => {
                            (Safety::Blocked, format!("git could not inspect it: {error}"))
                        }
                    }
                };
                let node = tree.node(folder);
                Candidate {
                    kind: Kind::StaleFolder,
                    safety,
                    title: self.display_path(folder),
                    detail: format!("{} files", format::count(u64::from(node.file_count))),
                    reason,
                    location: folder,
                    size: node.size,
                    frees: node.frees,
                    action: Action::DeleteTrees,
                    targets: vec![self.target(folder)],
                    occupancy_root: Some(tree.path(folder)),
                }
            })
            .collect()
    }

    fn global_package_candidates(&self, prefix: NodeId) -> Vec<Candidate> {
        let tree = self.tree;
        let Some(modules) = tree.child_named(prefix, "node_modules") else {
            return Vec::new();
        };
        let stale_seconds = self.config.stale_folder_days * SECONDS_PER_DAY;
        let packages: Vec<(String, NodeId)> = tree
            .children(modules)
            .filter(|&entry| tree.is_directory(entry) && !tree.is_link(entry))
            .flat_map(|entry| {
                let name = tree.name(entry).to_string();
                if !name.starts_with('@') {
                    return vec![(name, entry)];
                }
                tree.children(entry)
                    .filter(|&scoped| tree.is_directory(scoped))
                    .map(|scoped| (format!("{name}/{}", tree.name(scoped)), scoped))
                    .collect()
            })
            .filter(|(name, package)| {
                !name.starts_with('.') && self.age_seconds(*package) >= stale_seconds
            })
            .collect();
        let paths: Vec<PathBuf> = packages.iter().map(|(_, package)| tree.path(*package)).collect();
        let occupants = self.environment.processes.occupants(&paths);

        packages
            .into_iter()
            .zip(occupants)
            .map(|((name, package), occupants)| {
                let node = tree.node(package);
                let age = format::age(self.age_seconds(package));
                let (safety, reason) = if occupants.is_empty() {
                    (Safety::Review, "uninstalled through npm; reinstall any time".to_string())
                } else {
                    (Safety::Blocked, format!("running now: {}", describe_occupants(&occupants)))
                };
                Candidate {
                    kind: Kind::GlobalPackage,
                    safety,
                    title: name.clone(),
                    detail: format!(
                        "global CLI in {}, not updated for {age}",
                        self.display_path(prefix)
                    ),
                    reason,
                    location: package,
                    size: node.size,
                    frees: node.frees,
                    action: Action::UninstallGlobalPackage {
                        prefix: tree.path(prefix),
                        package: name,
                    },
                    targets: vec![self.target(package)],
                    occupancy_root: Some(tree.path(package)),
                }
            })
            .collect()
    }

    fn orphaned_worktree_candidate(&self, workspace: &Workspace, idle_seconds: i64) -> Candidate {
        let node = self.tree.node(workspace.root);
        let regenerable_files: u32 = workspace
            .generated
            .iter()
            .map(|generated| self.tree.node(generated.node).file_count)
            .sum();
        Candidate {
            kind: Kind::Worktree,
            safety: Safety::Review,
            title: self.display_path(workspace.root),
            detail: "leftover folder without git metadata".to_string(),
            reason: format!(
                "idle {}; {} files outside build output cannot be checked against any branch",
                format::age(idle_seconds),
                format::count(u64::from(node.file_count - regenerable_files)),
            ),
            location: workspace.root,
            size: node.size,
            frees: node.frees,
            action: Action::DeleteTrees,
            targets: vec![self.target(workspace.root)],
            occupancy_root: Some(workspace.path.clone()),
        }
    }

    fn scratch_candidates(&self, scratch: NodeId) -> Vec<Candidate> {
        let max_age = self.config.scratch_max_age_days * SECONDS_PER_DAY;
        let (databases, disposable): (Vec<NodeId>, Vec<NodeId>) = self
            .tree
            .children(scratch)
            .filter(|&entry| self.age_seconds(entry) >= max_age)
            .partition(|&entry| holds_database_files(self.tree, entry));

        let days = self.config.scratch_max_age_days;
        let candidate = |entries: Vec<NodeId>, safety, detail: String, reason: &str| Candidate {
            kind: Kind::Scratch,
            safety,
            title: self.display_path(scratch),
            detail,
            reason: reason.to_string(),
            location: scratch,
            size: entries.iter().map(|&entry| self.tree.node(entry).size).sum(),
            frees: entries.iter().map(|&entry| self.tree.node(entry).frees).sum(),
            action: Action::DeleteTrees,
            targets: entries.iter().map(|&entry| self.target(entry)).collect(),
            occupancy_root: None,
        };
        let disposable_detail = format!("{} entries untouched for {days}+ days", disposable.len());
        let databases_detail =
            format!("{} old entries holding SQL Server database files", databases.len());
        vec![
            candidate(
                disposable,
                Safety::Safe,
                disposable_detail,
                "old scratch; anything still open is skipped",
            ),
            candidate(
                databases,
                Safety::Review,
                databases_detail,
                "may still be registered with a LocalDB instance",
            ),
        ]
    }

    fn package_cache_candidates(&self, cache: NodeId) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        if let Some(npx) = self.tree.child_named(cache, "_npx") {
            candidates.extend(self.npx_candidate(npx));
        }
        if let Some(downloads) = self.tree.child_named(cache, "_cacache") {
            let node = self.tree.node(downloads);
            candidates.push(Candidate {
                kind: Kind::PackageCache,
                safety: Safety::Review,
                title: self.display_path(downloads),
                detail: "npm download cache".to_string(),
                reason: "re-downloaded on demand; the next installs are slower".to_string(),
                location: downloads,
                size: node.size,
                frees: node.frees,
                action: Action::DeleteTrees,
                targets: vec![self.target(downloads)],
                occupancy_root: None,
            });
        }
        candidates.extend(self.unlinked_bun_entries_candidate(cache));
        candidates
    }

    fn npx_candidate(&self, npx: NodeId) -> Option<Candidate> {
        let max_age = self.config.npx_max_age_days * SECONDS_PER_DAY;
        let old: Vec<NodeId> = self
            .tree
            .children(npx)
            .filter(|&entry| self.tree.is_directory(entry) && self.age_seconds(entry) >= max_age)
            .collect();
        let paths: Vec<PathBuf> = old.iter().map(|&entry| self.tree.path(entry)).collect();
        let occupants = self.environment.processes.occupants(&paths);
        let unused: Vec<NodeId> = old
            .into_iter()
            .zip(occupants)
            .filter(|(_, occupants)| occupants.is_empty())
            .map(|(entry, _)| entry)
            .collect();
        if unused.is_empty() {
            return None;
        }
        Some(Candidate {
            kind: Kind::PackageCache,
            safety: Safety::Safe,
            title: self.display_path(npx),
            detail: format!(
                "{} npx packages unused for {}+ days",
                unused.len(),
                self.config.npx_max_age_days
            ),
            reason: "npx downloads them again on demand".to_string(),
            location: npx,
            size: unused.iter().map(|&entry| self.tree.node(entry).size).sum(),
            frees: unused.iter().map(|&entry| self.tree.node(entry).frees).sum(),
            action: Action::DeleteTrees,
            targets: unused.iter().map(|&entry| self.target(entry)).collect(),
            occupancy_root: None,
        })
    }

    /// Bun hardlinks cache entries into every `node_modules`; an entry nothing links to is dead
    /// weight. Only a whole-volume scan sees every link, so partial scans propose nothing.
    fn unlinked_bun_entries_candidate(&self, cache: NodeId) -> Option<Candidate> {
        let scanned_whole_volume = self.tree.root_path().parent().is_none();
        if !scanned_whole_volume {
            return None;
        }
        let tree = self.tree;
        let scopes = tree
            .children(cache)
            .filter(|&child| tree.is_directory(child) && tree.name(child).starts_with('@'));
        let unlinked: Vec<NodeId> = std::iter::once(cache)
            .chain(scopes)
            .flat_map(|holder| tree.children(holder))
            .filter(|&entry| {
                tree.is_directory(entry)
                    && !tree.is_link(entry)
                    && tree.name(entry).contains(BUN_ENTRY_MARKER)
                    && !tree.shares_files_outside(entry)
                    && self.age_seconds(entry) >= CACHE_ENTRY_SETTLE_SECONDS
            })
            .collect();
        if unlinked.is_empty() {
            return None;
        }
        Some(Candidate {
            kind: Kind::PackageCache,
            safety: Safety::Safe,
            title: self.display_path(cache),
            detail: format!("{} bun cache entries no project links to", unlinked.len()),
            reason: "link counts are rechecked per entry right before deleting".to_string(),
            location: cache,
            size: unlinked.iter().map(|&entry| tree.node(entry).size).sum(),
            frees: unlinked.iter().map(|&entry| tree.node(entry).frees).sum(),
            action: Action::DeleteUnlinkedCacheEntries,
            targets: unlinked.iter().map(|&entry| self.target(entry)).collect(),
            occupancy_root: None,
        })
    }

    fn recycle_bin_candidate(&self, bin: NodeId) -> Option<Candidate> {
        let node = self.tree.node(bin);
        (node.frees > 0).then(|| Candidate {
            kind: Kind::RecycleBin,
            safety: Safety::Safe,
            title: self.display_path(bin),
            detail: format!("{} deleted files", format::count(u64::from(node.file_count))),
            reason: "already deleted once".to_string(),
            location: bin,
            size: node.size,
            frees: node.frees,
            action: Action::EmptyRecycleBin,
            targets: vec![self.target(bin)],
            occupancy_root: None,
        })
    }

    fn leftover_trash_candidate(&self, leftovers: &[NodeId]) -> Option<Candidate> {
        let first = *leftovers.first()?;
        Some(Candidate {
            kind: Kind::Scratch,
            safety: Safety::Safe,
            title: "interrupted clean-up".to_string(),
            detail: format!("{} half-deleted folders", leftovers.len()),
            reason: "a previous run was stopped while deleting these".to_string(),
            location: first,
            size: leftovers.iter().map(|&node| self.tree.node(node).size).sum(),
            frees: leftovers.iter().map(|&node| self.tree.node(node).frees).sum(),
            action: Action::DeleteTrees,
            targets: leftovers.iter().map(|&node| self.target(node)).collect(),
            occupancy_root: None,
        })
    }

    fn large_file_candidates(&self, mut files: Vec<NodeId>) -> Vec<Candidate> {
        files.sort_by_key(|&file| std::cmp::Reverse(self.tree.node(file).frees));
        files
            .into_iter()
            .take(LARGE_FILES_SHOWN)
            .map(|file| {
                let node = self.tree.node(file);
                Candidate {
                    kind: Kind::LargeFile,
                    safety: Safety::Review,
                    title: self.display_path(file),
                    detail: format!("modified {} ago", format::age(self.age_seconds(file))),
                    reason: "a single large file; only you know whether it is still needed"
                        .to_string(),
                    location: file,
                    size: node.size,
                    frees: node.frees,
                    action: Action::DeleteFiles,
                    targets: vec![self.target(file)],
                    occupancy_root: None,
                }
            })
            .collect()
    }
}

fn has_unproven(workspace: &Workspace) -> bool {
    workspace.generated.iter().any(|generated| generated.needs_ignore_proof)
}

fn removable_worktree_reason(state: &WorktreeState, proof: &str) -> String {
    let saved = match state.branch {
        Some(_) => format!("{proof}; the branch is kept"),
        None => proof.to_string(),
    };
    if state.ignored_local_files.is_empty() {
        return saved;
    }
    let examples: Vec<&str> =
        state.ignored_local_files.iter().take(3).map(String::as_str).collect();
    format!(
        "{saved}. Loses {} git-ignored local files, e.g. {}",
        state.ignored_local_files.len(),
        examples.join(", ")
    )
}

fn count_by_label(generated: &[&Generated]) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for item in generated {
        *counts.entry(item.label).or_default() += 1;
    }
    counts
        .into_iter()
        .map(
            |(label, count)| {
                if count == 1 {
                    label.to_string()
                } else {
                    format!("{label} ×{count}")
                }
            },
        )
        .collect::<Vec<_>>()
        .join(", ")
}

fn holds_database_files(tree: &Tree, node: NodeId) -> bool {
    if !tree.is_directory(node) {
        let name = tree.name(node).to_ascii_lowercase();
        return name.ends_with(".mdf") || name.ends_with(".ldf");
    }
    tree.children(node).any(|child| holds_database_files(tree, child))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::RunningProcess;
    use crate::scan::{scan, ScanProgress};
    use std::fs;
    use std::os::windows::fs::OpenOptionsExt;
    use std::time::{Duration, SystemTime};

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const HOUR: u64 = 3600;
    const DAY: u64 = 24 * HOUR;

    fn write(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![1u8; 8192]).unwrap();
    }

    fn set_age(path: &Path, seconds: u64) {
        if path.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                set_age(&entry.unwrap().path(), seconds);
            }
        }
        let handle = fs::OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap();
        handle.set_modified(SystemTime::now() - Duration::from_secs(seconds)).unwrap();
    }

    fn plan_with(root: &Path, processes: ProcessSnapshot) -> Plan {
        let tree = scan(root, &ScanProgress::default()).unwrap();
        let config = Config {
            root: root.to_path_buf(),
            minimum_candidate_megabytes: 0,
            stale_folder_megabytes: 0,
            ..Config::default()
        };
        let sessions = AgentSessions::none();
        let environment = Environment {
            processes: &processes,
            sessions: &sessions,
            now_unix: crate::winfs::unix_seconds_now(),
            depth: Depth::Thorough,
        };
        build_plan(&tree, &config, &environment)
    }

    fn plan(root: &Path) -> Plan {
        plan_with(root, ProcessSnapshot::default())
    }

    fn only(plan: &Plan, kind: Kind) -> &Candidate {
        let matching: Vec<&Candidate> =
            plan.candidates.iter().filter(|candidate| candidate.kind == kind).collect();
        assert_eq!(matching.len(), 1, "expected one {kind:?}, got {:#?}", plan.candidates);
        matching[0]
    }

    fn dotnet_project(root: &Path) -> PathBuf {
        let project = root.join("service");
        write(&project.join("Service.csproj"));
        write(&project.join("Program.cs"));
        write(&project.join("bin/Debug/net10.0/Service.dll"));
        write(&project.join("obj/project.assets.json"));
        project
    }

    #[test]
    fn idle_project_offers_its_build_output_as_safe() {
        let root = tempfile::tempdir().unwrap();
        let project = dotnet_project(root.path());
        set_age(&project, 30 * DAY);

        let plan = plan(root.path());

        let build_output = only(&plan, Kind::BuildOutput);
        assert_eq!(build_output.safety, Safety::Safe);
        assert_eq!(build_output.targets.len(), 2);
        assert!(build_output
            .targets
            .iter()
            .all(|target| { target.path.ends_with("bin") || target.path.ends_with("obj") }));
    }

    #[test]
    fn source_edited_minutes_ago_blocks_the_whole_workspace() {
        let root = tempfile::tempdir().unwrap();
        let project = dotnet_project(root.path());
        set_age(&project, 30 * DAY);
        write(&project.join("Program.cs"));

        let plan = plan(root.path());

        let build_output = only(&plan, Kind::BuildOutput);
        assert_eq!(build_output.safety, Safety::Blocked);
        assert!(build_output.reason.contains("source edit"), "{}", build_output.reason);
    }

    #[test]
    fn source_edited_hours_ago_needs_review() {
        let root = tempfile::tempdir().unwrap();
        let project = dotnet_project(root.path());
        set_age(&project, 30 * DAY);
        set_age(&project.join("Program.cs"), 2 * HOUR);

        let plan = plan(root.path());

        assert_eq!(only(&plan, Kind::BuildOutput).safety, Safety::Review);
    }

    #[test]
    fn running_process_blocks_an_otherwise_idle_workspace() {
        let root = tempfile::tempdir().unwrap();
        let project = dotnet_project(root.path());
        set_age(&project, 30 * DAY);
        let watcher = RunningProcess {
            pid: 7,
            name: "dotnet.exe".to_string(),
            working_directory: Some(project.join("bin").to_string_lossy().into_owned()),
            ..RunningProcess::default()
        };

        let plan = plan_with(root.path(), ProcessSnapshot::of(vec![watcher]));

        let build_output = only(&plan, Kind::BuildOutput);
        assert_eq!(build_output.safety, Safety::Blocked);
        assert!(build_output.reason.contains("dotnet.exe"), "{}", build_output.reason);
    }

    #[test]
    fn folders_merely_named_bin_are_not_build_output() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("crate/Cargo.toml"));
        write(&root.path().join("crate/src/bin/tool.rs"));
        write(&root.path().join("scripts/bin/deploy.ps1"));
        write(&root.path().join("scripts/obj/model.obj"));
        set_age(root.path(), 30 * DAY);

        let plan = plan(root.path());

        assert!(plan.candidates.iter().all(|candidate| candidate.kind != Kind::BuildOutput));
    }

    #[test]
    fn dependencies_wait_longer_than_build_output() {
        let root = tempfile::tempdir().unwrap();
        for (project, idle_days) in [("recent", 3), ("old", 30)] {
            let project_root = root.path().join(project);
            write(&project_root.join("package.json"));
            write(&project_root.join("node_modules/left-pad/index.js"));
            set_age(&project_root, idle_days * DAY);
        }

        let plan = plan(root.path());

        let safety_of = |title: &str| {
            plan.candidates.iter().find(|candidate| candidate.title == title).unwrap().safety
        };
        assert_eq!(safety_of("recent"), Safety::Review);
        assert_eq!(safety_of("old"), Safety::Safe);
    }

    #[test]
    fn globally_installed_packages_are_never_project_dependencies() {
        let root = tempfile::tempdir().unwrap();
        let prefix = root.path().join("npm-global");
        write(&prefix.join("package.json"));
        write(&prefix.join("some-cli.cmd"));
        write(&prefix.join("some-cli.ps1"));
        write(&prefix.join("node_modules/some-cli/package.json"));
        write(&prefix.join("node_modules/some-cli/node_modules/dep/index.js"));
        write(&prefix.join("node_modules/@scope/other-cli/package.json"));
        set_age(&prefix, 200 * DAY);

        let plan = plan(root.path());

        assert!(plan.candidates.iter().all(|candidate| candidate.kind != Kind::Dependencies));
        let mut packages: Vec<&str> = plan
            .candidates
            .iter()
            .filter(|candidate| candidate.kind == Kind::GlobalPackage)
            .map(|candidate| candidate.title.as_str())
            .collect();
        packages.sort_unstable();
        assert_eq!(packages, ["@scope/other-cli", "some-cli"]);
        assert!(plan.candidates.iter().all(|candidate| candidate.safety != Safety::Safe));
    }

    #[test]
    fn only_old_scratch_is_offered_and_database_files_need_review() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("tmp");
        write(&scratch.join("old-run/output.log"));
        write(&scratch.join("old-database/test.mdf"));
        set_age(&scratch, 30 * DAY);
        write(&scratch.join("fresh.log"));

        let plan = plan(root.path());

        let scratch_candidates: Vec<&Candidate> =
            plan.candidates.iter().filter(|candidate| candidate.kind == Kind::Scratch).collect();
        let target_names = |safety: Safety| -> Vec<String> {
            scratch_candidates
                .iter()
                .filter(|candidate| candidate.safety == safety)
                .flat_map(|candidate| &candidate.targets)
                .map(|target| target.path.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(target_names(Safety::Safe), ["old-run"]);
        assert_eq!(target_names(Safety::Review), ["old-database"]);
    }

    #[test]
    fn stale_folders_are_offered_unless_they_hold_git_checkouts() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("archive/recording.wav"));
        write(&root.path().join("holder/project/.git/HEAD"));
        write(&root.path().join("holder/project/notes.md"));
        set_age(root.path(), 200 * DAY);
        write(&root.path().join("current/notes.md"));

        let plan = plan(root.path());

        let stale = only(&plan, Kind::StaleFolder);
        assert_eq!(stale.title, "archive");
        assert_eq!(stale.safety, Safety::Review);
    }

    #[test]
    fn nested_targets_are_counted_once() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("archive");
        write(&project.join("package.json"));
        write(&project.join("node_modules/left-pad/index.js"));
        set_age(root.path(), 200 * DAY);
        let tree = scan(root.path(), &ScanProgress::default()).unwrap();
        let plan = plan(root.path());

        let everything = plan.frees_together(&tree, |_| true);

        assert_eq!(everything, tree.node(ROOT).frees);
        assert!(plan.candidates.len() >= 2);
    }
}
