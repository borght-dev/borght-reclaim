use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::activity::modified_unix;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Directory names whose contents can always be regenerated, wherever git lists them as ignored.
pub const DISPOSABLE_DIRECTORY_NAMES: &[&str] = &[
    "node_modules",
    "bin",
    "obj",
    "target",
    "dist",
    "out",
    "build",
    ".next",
    ".nuxt",
    ".turbo",
    ".angular",
    ".svelte-kit",
    ".parcel-cache",
    "storybook-static",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "TestResults",
    "playwright-report",
    "test-results",
    "coverage",
    ".vs",
    ".idea",
    ".venv",
    "venv",
    "logs",
    "tmp",
    "temp",
    ".cache",
    "artifacts",
    ".artifacts",
];

fn git(working_directory: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(working_directory)
        .creation_flags(CREATE_NO_WINDOW);
    command
}

fn succeeded(output: Output) -> io::Result<String> {
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    Err(io::Error::other(String::from_utf8_lossy(&output.stderr).trim().to_string()))
}

#[derive(Debug, PartialEq, Eq)]
pub enum WorkspaceLink {
    Repository,
    LinkedWorktree { repository: PathBuf },
    Submodule,
}

pub struct GitLocation {
    pub link: WorkspaceLink,
    pub git_directory: PathBuf,
}

pub fn locate(workspace: &Path) -> Option<GitLocation> {
    let dot_git = workspace.join(".git");
    if dot_git.is_dir() {
        return Some(GitLocation { link: WorkspaceLink::Repository, git_directory: dot_git });
    }
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let target = pointer.lines().find_map(|line| line.strip_prefix("gitdir:"))?.trim();
    let git_directory = workspace.join(target.replace('/', "\\"));
    let link = match worktree_owner(&git_directory) {
        Some(repository) => WorkspaceLink::LinkedWorktree { repository },
        None => WorkspaceLink::Submodule,
    };
    Some(GitLocation { link, git_directory })
}

fn worktree_owner(git_directory: &Path) -> Option<PathBuf> {
    let worktrees = git_directory.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let shared_git_directory = worktrees.parent()?;
    if shared_git_directory.file_name()? != ".git" {
        return None;
    }
    Some(shared_git_directory.parent()?.to_path_buf())
}

/// Commits, checkouts, resets and pulls rewrite one of these files. The index is left out on
/// purpose: every `git status` an editor or agent host polls rewrites it.
pub fn last_activity_unix(git_directory: &Path) -> Option<i64> {
    ["HEAD", "logs/HEAD"].iter().filter_map(|file| modified_unix(&git_directory.join(file))).max()
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct WorktreeState {
    pub branch: Option<String>,
    pub head: String,
    pub merged_pull_request: Option<u64>,
    pub uncommitted_changes: usize,
    pub untracked_files: usize,
    pub unpushed_commits: Option<usize>,
    pub exists_on_remote: bool,
    /// Another local branch that already contains this worktree's commit.
    pub containing_local_branch: Option<String>,
    pub ignored_local_files: Vec<String>,
    /// Submodule repositories live inside the worktree's git directory and go with it.
    pub has_submodules: bool,
    pub submodule_work: Option<String>,
}

/// Where a worktree's commits remain once the worktree itself is removed.
#[derive(Debug, PartialEq, Eq)]
pub enum Safekeeping {
    MergedPullRequest(u64),
    Pushed,
    OtherLocalBranch(String),
    /// `git worktree remove` keeps the branch, so its commits stay in this repository only.
    OwnBranchOnly,
    /// A detached HEAD on commits no branch holds: removing the worktree would orphan them.
    Nowhere,
}

impl WorktreeState {
    pub fn safekeeping(&self) -> Safekeeping {
        if let Some(number) = self.merged_pull_request {
            return Safekeeping::MergedPullRequest(number);
        }
        if self.unpushed_commits == Some(0) || self.exists_on_remote {
            return Safekeeping::Pushed;
        }
        match (&self.containing_local_branch, &self.branch) {
            (Some(other), _) => Safekeeping::OtherLocalBranch(other.clone()),
            (None, Some(_)) => Safekeeping::OwnBranchOnly,
            (None, None) => Safekeeping::Nowhere,
        }
    }

    pub fn local_changes(&self) -> Option<String> {
        if let Some(work) = &self.submodule_work {
            return Some(work.clone());
        }
        if self.uncommitted_changes > 0 {
            return Some(format!("{} uncommitted changes", self.uncommitted_changes));
        }
        (self.untracked_files > 0).then(|| format!("{} untracked files", self.untracked_files))
    }

    /// What removing the worktree would destroy. Unpushed commits on a branch are not among
    /// it: the branch outlives the worktree.
    pub fn blocker(&self) -> Option<String> {
        if let Some(changes) = self.local_changes() {
            return Some(changes);
        }
        (self.safekeeping() == Safekeeping::Nowhere)
            .then(|| "a detached HEAD on commits no branch holds".to_string())
    }
}

/// PR number by (branch name, head commit).
pub type MergedPullRequests = std::collections::HashMap<(String, String), u64>;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergedPullRequest {
    number: u64,
    head_ref_name: String,
    head_ref_oid: String,
}

/// Branch name and head commit of recently merged pull requests, mapped to the PR number.
/// A squash merge leaves the branch's commits on no remote ref, so this is the only proof
/// that such a worktree's work has landed. Empty when `gh` is missing or not signed in.
pub fn merged_pull_requests(repository: &Path) -> MergedPullRequests {
    list_merged_pull_requests(repository, &["--limit", "300"])
}

/// For a branch whose merge is older than the recent list reaches back.
pub fn merged_pull_request_of(repository: &Path, branch: &str, head: &str) -> Option<u64> {
    list_merged_pull_requests(repository, &["--limit", "5", "--head", branch])
        .get(&(branch.to_string(), head.to_string()))
        .copied()
}

fn list_merged_pull_requests(repository: &Path, filter: &[&str]) -> MergedPullRequests {
    let listed = Command::new("gh")
        .current_dir(repository)
        .args(["pr", "list", "--state", "merged"])
        .args(filter)
        .args(["--json", "number,headRefName,headRefOid"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let Ok(json) = listed.and_then(succeeded) else {
        return Default::default();
    };
    serde_json::from_str::<Vec<MergedPullRequest>>(&json)
        .unwrap_or_default()
        .into_iter()
        .map(|merged| ((merged.head_ref_name, merged.head_ref_oid), merged.number))
        .collect()
}

pub fn inspect_worktree(worktree: &Path) -> io::Result<WorktreeState> {
    let status = succeeded(
        git(worktree)
            .args([
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=normal",
                "--ignore-submodules=none",
            ])
            .output()?,
    )?;
    let mut state = parse_status(&status);
    let submodules = Submodules::of(worktree)?;
    state.has_submodules = submodules.exist();
    state.submodule_work = submodules.unsaved_work(worktree)?;
    if state.unpushed_commits.is_none() {
        let remote_branches = succeeded(
            git(worktree)
                .args(["for-each-ref", "--count=1", "--contains", "HEAD", "refs/remotes"])
                .output()?,
        )?;
        state.exists_on_remote = !remote_branches.trim().is_empty();
    }
    if state.safekeeping() != Safekeeping::Pushed {
        let containing = succeeded(
            git(worktree)
                .args([
                    "for-each-ref",
                    "--format=%(refname:short)",
                    "--contains",
                    "HEAD",
                    "refs/heads",
                ])
                .output()?,
        )?;
        state.containing_local_branch = containing
            .lines()
            .find(|&branch| Some(branch) != state.branch.as_deref())
            .map(str::to_string);
    }
    state.ignored_local_files = ignored_local_files(worktree)?;
    for checkout in &submodules.checkouts {
        let inside = ignored_local_files(&worktree.join(checkout))?;
        state.ignored_local_files.extend(inside.iter().map(|path| format!("{checkout}/{path}")));
    }
    Ok(state)
}

fn ignored_local_files(repository: &Path) -> io::Result<Vec<String>> {
    let ignored = succeeded(
        git(repository)
            .args(["ls-files", "--others", "--ignored", "--exclude-standard", "--directory"])
            .output()?,
    )?;
    Ok(ignored.lines().filter(|path| !is_disposable(path)).map(str::to_string).collect())
}

/// What deleting a whole repository folder would destroy. Unlike removing a worktree this
/// takes every branch and stash with it, so anything no remote holds counts.
pub fn repository_blocker(repository: &Path) -> io::Result<Option<String>> {
    let status = succeeded(
        git(repository).args(["status", "--porcelain=v2", "--untracked-files=normal"]).output()?,
    )?;
    if let Some(changes) = parse_status(&status).local_changes() {
        return Ok(Some(changes));
    }
    history_blocker(|| git(repository))
}

fn history_blocker(git: impl Fn() -> Command) -> io::Result<Option<String>> {
    let local_only = succeeded(
        git()
            .args(["rev-list", "--count", "--branches", "--tags", "HEAD", "--not", "--remotes"])
            .output()?,
    )?;
    if local_only.trim() != "0" {
        return Ok(Some(format!("{} commits that no remote has", local_only.trim())));
    }
    let stashes = succeeded(git().args(["stash", "list"]).output()?)?;
    Ok((!stashes.trim().is_empty()).then(|| format!("{} stashes", stashes.lines().count())))
}

/// A linked worktree keeps its submodule repositories under its own git directory, so they
/// are deleted with it.
struct Submodules {
    /// Checked-out submodules at any depth, relative to the worktree.
    checkouts: Vec<String>,
    /// Every repository under `modules`, including ones whose checkout was deinitialised.
    stores: Vec<PathBuf>,
    modules: PathBuf,
}

impl Submodules {
    fn of(worktree: &Path) -> io::Result<Self> {
        let git_directory = PathBuf::from(
            succeeded(git(worktree).args(["rev-parse", "--absolute-git-dir"]).output()?)?.trim(),
        );
        let modules = git_directory.join("modules");
        if !worktree.join(".gitmodules").exists() && !modules.exists() {
            return Ok(Self { checkouts: Vec::new(), stores: Vec::new(), modules });
        }
        Ok(Self {
            checkouts: populated_submodules(worktree)?,
            stores: submodule_stores(&modules),
            modules,
        })
    }

    fn exist(&self) -> bool {
        !self.checkouts.is_empty() || !self.stores.is_empty()
    }

    /// As with a whole repository folder, anything a submodule holds that no remote has counts.
    fn unsaved_work(&self, worktree: &Path) -> io::Result<Option<String>> {
        for checkout in &self.checkouts {
            if let Some(work) = repository_blocker(&worktree.join(checkout))? {
                return Ok(Some(format!("submodule {checkout} has {work}")));
            }
        }
        for store in &self.stores {
            if let Some(work) = history_blocker(|| store_git(store))? {
                let name = store.strip_prefix(&self.modules).unwrap_or(store);
                let name = name.to_string_lossy().replace('\\', "/");
                return Ok(Some(format!("submodule repository {name} has {work}")));
            }
        }
        Ok(None)
    }
}

/// Checked-out submodules at any depth, as paths relative to `repository`.
fn populated_submodules(repository: &Path) -> io::Result<Vec<String>> {
    let staged = succeeded(git(repository).args(["ls-files", "-z", "--stage"]).output()?)?;
    let mut populated = Vec::new();
    for entry in staged.split('\0') {
        let Some((metadata, path)) = entry.split_once('\t') else { continue };
        if !metadata.starts_with("160000 ") || !repository.join(path).join(".git").exists() {
            continue;
        }
        let nested = populated_submodules(&repository.join(path))?;
        populated.push(path.to_string());
        populated.extend(nested.into_iter().map(|inner| format!("{path}/{inner}")));
    }
    Ok(populated)
}

/// Git directories under `modules`, nested ones included; names may contain slashes.
fn submodule_stores(modules: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(modules) else { return Vec::new() };
    let mut stores = Vec::new();
    for directory in entries.flatten().map(|entry| entry.path()).filter(|path| path.is_dir()) {
        if directory.join("HEAD").is_file() && directory.join("objects").is_dir() {
            stores.extend(submodule_stores(&directory.join("modules")));
            stores.push(directory);
        } else {
            stores.extend(submodule_stores(&directory));
        }
    }
    stores
}

fn store_git(store: &Path) -> Command {
    let mut command = git(store);
    command.arg("--git-dir=.");
    command
}

fn parse_status(status: &str) -> WorktreeState {
    let mut state = WorktreeState::default();
    for line in status.lines() {
        if let Some(commit) = line.strip_prefix("# branch.oid ") {
            state.head = commit.to_string();
        } else if let Some(head) = line.strip_prefix("# branch.head ") {
            state.branch = (head != "(detached)").then(|| head.to_string());
        } else if let Some(ahead_behind) = line.strip_prefix("# branch.ab ") {
            let ahead = ahead_behind.split_whitespace().next().unwrap_or("+0");
            state.unpushed_commits = ahead.trim_start_matches('+').parse().ok();
        } else if line.starts_with("? ") {
            state.untracked_files += 1;
        } else if line.starts_with(['1', '2', 'u']) {
            state.uncommitted_changes += 1;
        }
    }
    state
}

fn is_disposable(ignored_path: &str) -> bool {
    ignored_path.split('/').filter(|part| !part.is_empty()).any(|part| {
        DISPOSABLE_DIRECTORY_NAMES.iter().any(|name| name.eq_ignore_ascii_case(part))
            || part.ends_with(".log")
            || part.ends_with(".tsbuildinfo")
    })
}

/// Returns the subset of `paths` that git ignores and that hold no tracked files.
pub fn ignored_untracked(workspace: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|path| {
            let ignored = git(workspace)
                .args(["check-ignore", "--quiet"])
                .arg(path)
                .status()
                .is_ok_and(|status| status.success());
            let tracked = git(workspace)
                .args(["ls-files", "--"])
                .arg(path)
                .output()
                .map_or(true, |output| !output.stdout.is_empty());
            ignored && !tracked
        })
        .cloned()
        .collect()
}

/// Git refuses any worktree with submodules unless forced, and a single force skips its own
/// clean check while still refusing a locked worktree. So `with_submodules` must only be passed
/// right after [`inspect_worktree`] found no local changes, submodule work included.
pub fn remove_worktree(
    repository: &Path,
    worktree: &Path,
    with_submodules: bool,
) -> io::Result<()> {
    let mut command = git(repository);
    command.args(["worktree", "remove"]);
    if with_submodules {
        command.arg("--force");
    }
    succeeded(command.arg(worktree).output()?).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_pushed_branch_has_no_blocker() {
        let state = parse_status("# branch.oid abc\n# branch.head feature\n# branch.upstream origin/feature\n# branch.ab +0 -3\n");

        assert_eq!(state.branch.as_deref(), Some("feature"));
        assert_eq!(state.blocker(), None);
        assert_eq!(state.safekeeping(), Safekeeping::Pushed);
    }

    #[test]
    fn only_work_that_exists_nowhere_else_blocks() {
        let modified = parse_status(
            "# branch.head f\n# branch.ab +0 -0\n1 .M N... 100644 100644 100644 a b src/app.ts\n",
        );
        let untracked = parse_status("# branch.head f\n# branch.ab +0 -0\n? notes.md\n");
        let unpushed_on_a_branch = parse_status("# branch.head f\n# branch.ab +2 -0\n");
        let detached_on_stray_commits = parse_status("# branch.head (detached)\n");

        assert_eq!(modified.blocker().as_deref(), Some("1 uncommitted changes"));
        assert_eq!(untracked.blocker().as_deref(), Some("1 untracked files"));
        assert_eq!(unpushed_on_a_branch.blocker(), None);
        assert!(detached_on_stray_commits.blocker().is_some());
    }

    #[test]
    fn safekeeping_names_the_strongest_place_the_commits_live() {
        let mut state = parse_status("# branch.head local-only\n");
        assert_eq!(state.safekeeping(), Safekeeping::OwnBranchOnly);

        state.containing_local_branch = Some("integration".to_string());
        assert_eq!(state.safekeeping(), Safekeeping::OtherLocalBranch("integration".to_string()));

        state.exists_on_remote = true;
        assert_eq!(state.safekeeping(), Safekeeping::Pushed);

        state.merged_pull_request = Some(42);
        assert_eq!(state.safekeeping(), Safekeeping::MergedPullRequest(42));
    }

    #[test]
    fn detached_head_is_removable_once_a_branch_holds_its_commit() {
        let mut state = parse_status("# branch.head (detached)\n");
        state.containing_local_branch = Some("main".to_string());

        assert_eq!(state.blocker(), None);
    }

    fn run_git(directory: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(["-c", "user.name=test", "-c", "user.email=test@example.invalid"])
            .args(["-c", "protocol.file.allow=always"])
            .args(arguments)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn repository_folder_is_only_deletable_when_a_remote_holds_everything() {
        let root = tempfile::tempdir().unwrap();
        let origin = root.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        run_git(&origin, &["init", "--quiet"]);
        std::fs::write(origin.join("readme.md"), "hello").unwrap();
        run_git(&origin, &["add", "."]);
        run_git(&origin, &["commit", "--quiet", "-m", "first"]);
        run_git(root.path(), &["clone", "--quiet", "origin", "clone"]);
        let clone = root.path().join("clone");

        assert!(repository_blocker(&origin).unwrap().is_some(), "no remote holds its commit");
        assert_eq!(repository_blocker(&clone).unwrap(), None);

        run_git(&clone, &["checkout", "--quiet", "-b", "side"]);
        std::fs::write(clone.join("idea.md"), "draft").unwrap();
        run_git(&clone, &["add", "."]);
        run_git(&clone, &["commit", "--quiet", "-m", "local only"]);
        run_git(&clone, &["checkout", "--quiet", "-"]);
        let unpushed_side_branch = repository_blocker(&clone).unwrap();

        assert_eq!(unpushed_side_branch.as_deref(), Some("1 commits that no remote has"));
    }

    #[test]
    fn worktree_commit_held_by_another_local_branch_is_safe_to_remove() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repo");
        std::fs::create_dir_all(&repository).unwrap();
        run_git(&repository, &["init", "--quiet"]);
        std::fs::write(repository.join("readme.md"), "hello").unwrap();
        run_git(&repository, &["add", "."]);
        run_git(&repository, &["commit", "--quiet", "-m", "first"]);
        let worktree = root.path().join("step");
        run_git(
            &repository,
            &["worktree", "add", "--quiet", "-b", "step", worktree.to_str().unwrap()],
        );
        std::fs::write(worktree.join("step.md"), "work").unwrap();
        run_git(&worktree, &["add", "."]);
        run_git(&worktree, &["commit", "--quiet", "-m", "step work"]);

        let before_integration = inspect_worktree(&worktree).unwrap();
        run_git(&repository, &["merge", "--quiet", "--ff-only", "step"]);
        let after_integration = inspect_worktree(&worktree).unwrap();

        assert_eq!(before_integration.safekeeping(), Safekeeping::OwnBranchOnly);
        assert!(matches!(after_integration.safekeeping(), Safekeeping::OtherLocalBranch(_)));
        assert_eq!(after_integration.blocker(), None);
    }

    fn commit_file(repository: &Path, name: &str) {
        std::fs::write(repository.join(name), name).unwrap();
        run_git(repository, &["add", "."]);
        run_git(repository, &["commit", "--quiet", "-m", name]);
    }

    /// A pushed worktree of `app` with `lib` checked out at `vendor/lib`.
    fn worktree_with_submodule(root: &Path) -> (PathBuf, PathBuf) {
        let library = root.join("lib");
        let repository = root.join("app");
        for directory in [&library, &repository] {
            std::fs::create_dir_all(directory).unwrap();
            run_git(directory, &["init", "--quiet"]);
            commit_file(directory, "readme.md");
        }
        run_git(&repository, &["submodule", "add", "--quiet", "../lib", "vendor/lib"]);
        run_git(&repository, &["commit", "--quiet", "-m", "add submodule"]);
        let worktree = root.join("wt");
        run_git(&repository, &["worktree", "add", "--quiet", "--detach", "../wt"]);
        run_git(&worktree, &["submodule", "update", "--init", "--quiet"]);
        (repository, worktree)
    }

    #[test]
    fn clean_worktree_with_submodule_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let (repository, worktree) = worktree_with_submodule(root.path());

        let state = inspect_worktree(&worktree).unwrap();
        run_git(&repository, &["worktree", "lock", "../wt"]);
        let locked = remove_worktree(&repository, &worktree, state.has_submodules);
        run_git(&repository, &["worktree", "unlock", "../wt"]);

        assert!(state.has_submodules);
        assert_eq!(state.local_changes(), None);
        assert_eq!(state.ignored_local_files, Vec::<String>::new());
        assert!(remove_worktree(&repository, &worktree, false).is_err(), "git refuses unforced");
        assert!(locked.is_err(), "a lock still holds");
        remove_worktree(&repository, &worktree, state.has_submodules).unwrap();
        assert!(!worktree.exists());
    }

    #[test]
    fn work_inside_a_submodule_blocks_worktree_removal() {
        let root = tempfile::tempdir().unwrap();
        let (_, worktree) = worktree_with_submodule(root.path());
        let submodule = worktree.join("vendor/lib");

        std::fs::write(submodule.join("draft.md"), "draft").unwrap();
        let untracked = inspect_worktree(&worktree).unwrap().local_changes();
        run_git(&submodule, &["add", "."]);
        run_git(&submodule, &["stash", "--quiet"]);
        let stashed = inspect_worktree(&worktree).unwrap().local_changes();
        std::fs::write(submodule.join(".gitignore"), ".env\n").unwrap();
        std::fs::write(submodule.join(".env"), "secret").unwrap();
        run_git(&submodule, &["add", ".gitignore"]);
        let with_local_settings = inspect_worktree(&worktree).unwrap();
        assert_eq!(with_local_settings.ignored_local_files, ["vendor/lib/.env"]);
        run_git(&submodule, &["stash", "drop", "--quiet"]);
        commit_file(&submodule, "feature.md");
        run_git(&worktree, &["commit", "--quiet", "-am", "bump submodule"]);
        let unpushed = inspect_worktree(&worktree).unwrap();

        assert_eq!(untracked.as_deref(), Some("submodule vendor/lib has 1 untracked files"));
        assert_eq!(stashed.as_deref(), Some("submodule vendor/lib has 1 stashes"));
        assert_eq!(unpushed.uncommitted_changes, 0, "the parent itself is clean");
        assert_eq!(
            unpushed.blocker().as_deref(),
            Some("submodule vendor/lib has 1 commits that no remote has")
        );
    }

    #[test]
    fn deinitialised_submodule_store_still_blocks_with_unpushed_commits() {
        let root = tempfile::tempdir().unwrap();
        let (_, worktree) = worktree_with_submodule(root.path());
        commit_file(&worktree.join("vendor/lib"), "feature.md");
        run_git(&worktree, &["commit", "--quiet", "-am", "bump submodule"]);
        run_git(&worktree, &["submodule", "deinit", "--quiet", "--force", "--all"]);

        let state = inspect_worktree(&worktree).unwrap();

        assert!(state.has_submodules);
        assert_eq!(
            state.local_changes().as_deref(),
            Some("submodule repository vendor/lib has 1 commits that no remote has")
        );
    }

    #[test]
    fn only_regenerable_ignored_paths_are_disposable() {
        assert!(is_disposable("Shop/Shop.Portal/node_modules/"));
        assert!(is_disposable("Shop/Api/bin/"));
        assert!(is_disposable("build.log"));
        assert!(!is_disposable(".env"));
        assert!(!is_disposable("Shop/Api/appsettings.Local.json"));
    }

    #[test]
    fn linked_worktree_pointer_resolves_to_its_repository() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repo");
        let worktree = root.path().join("wt");
        std::fs::create_dir_all(repository.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        let pointer =
            format!("gitdir: {}/.git/worktrees/wt\n", repository.display()).replace('\\', "/");
        std::fs::write(worktree.join(".git"), pointer).unwrap();

        let located = locate(&worktree).unwrap();

        assert_eq!(located.link, WorkspaceLink::LinkedWorktree { repository: repository.clone() });
        assert_eq!(locate(&repository).unwrap().link, WorkspaceLink::Repository);
    }
}
