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
            .args(["status", "--porcelain=v2", "--branch", "--untracked-files=normal"])
            .output()?,
    )?;
    let mut state = parse_status(&status);
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
    let ignored = succeeded(
        git(worktree)
            .args(["ls-files", "--others", "--ignored", "--exclude-standard", "--directory"])
            .output()?,
    )?;
    state.ignored_local_files =
        ignored.lines().filter(|path| !is_disposable(path)).map(str::to_string).collect();
    Ok(state)
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
    let local_only = succeeded(
        git(repository)
            .args(["rev-list", "--count", "--branches", "--tags", "HEAD", "--not", "--remotes"])
            .output()?,
    )?;
    if local_only.trim() != "0" {
        return Ok(Some(format!("{} commits that no remote has", local_only.trim())));
    }
    let stashes = succeeded(git(repository).args(["stash", "list"]).output()?)?;
    Ok((!stashes.trim().is_empty()).then(|| format!("{} stashes", stashes.lines().count())))
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

/// Never forced: git itself refuses when the worktree is dirty or locked.
pub fn remove_worktree(repository: &Path, worktree: &Path) -> io::Result<()> {
    succeeded(git(repository).args(["worktree", "remove"]).arg(worktree).output()?).map(|_| ())
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
