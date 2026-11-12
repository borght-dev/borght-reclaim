use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;

use rayon::prelude::*;
use serde::Serialize;

use crate::activity::{describe_occupants, ProcessSnapshot};
use crate::git;
use crate::plan::{Action, Candidate, Safety, TRASH_MARKER};
use crate::territory;
use crate::winfs::{self, WidePath};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Outcome {
    Removed,
    Skipped(String),
    Failed(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct TargetResult {
    pub path: PathBuf,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CleanReport {
    pub results: Vec<TargetResult>,
    pub free_bytes_before: u64,
    pub free_bytes_after: u64,
}

impl CleanReport {
    pub fn freed_bytes(&self) -> u64 {
        self.free_bytes_after.saturating_sub(self.free_bytes_before)
    }

    pub fn removed_count(&self) -> usize {
        self.results.iter().filter(|result| result.outcome == Outcome::Removed).count()
    }

    pub fn not_removed(&self) -> impl Iterator<Item = &TargetResult> {
        self.results.iter().filter(|result| result.outcome != Outcome::Removed)
    }
}

#[derive(Default)]
pub struct CleanProgress {
    pub finished_targets: AtomicUsize,
    pub total_targets: AtomicUsize,
    pub current: Mutex<String>,
    pub cancelled: AtomicBool,
}

pub fn clean(scan_root: &Path, candidates: &[Candidate], progress: &CleanProgress) -> CleanReport {
    let total_targets = candidates.iter().map(|candidate| candidate.targets.len()).sum();
    progress.total_targets.store(total_targets, Ordering::Relaxed);
    let free_bytes_before = winfs::volume_space(scan_root).map_or(0, |space| space.free_bytes);

    let cleaner = Cleaner {
        scan_root,
        progress,
        processes: ProcessSnapshot::capture(),
        trash_sequence: AtomicU32::new(0),
    };
    let results =
        candidates.iter().flat_map(|candidate| cleaner.clean_candidate(candidate)).collect();

    let free_bytes_after = winfs::volume_space(scan_root).map_or(0, |space| space.free_bytes);
    CleanReport { results, free_bytes_before, free_bytes_after }
}

struct Cleaner<'a> {
    scan_root: &'a Path,
    progress: &'a CleanProgress,
    processes: ProcessSnapshot,
    trash_sequence: AtomicU32,
}

impl Cleaner<'_> {
    fn clean_candidate(&self, candidate: &Candidate) -> Vec<TargetResult> {
        if let Some(refusal) = self.refusal(candidate) {
            return self.skip_all(candidate, &refusal);
        }
        match &candidate.action {
            Action::DeleteTrees | Action::DeleteFiles => {
                self.each_target(candidate, |path| self.remove(path))
            }
            Action::DeleteUnlinkedCacheEntries => {
                self.each_target(candidate, |path| self.remove_unlinked_cache_entry(path))
            }
            Action::EmptyRecycleBin => self.each_target(candidate, |_| {
                match winfs::empty_recycle_bin(&winfs::volume_root(self.scan_root)) {
                    Ok(()) => Outcome::Removed,
                    Err(error) => Outcome::Failed(error.to_string()),
                }
            }),
            Action::RemoveWorktree { repository, regenerable, verified_head } => self
                .each_target(candidate, |path| {
                    self.remove_worktree(repository, path, regenerable, verified_head)
                }),
            Action::UninstallGlobalPackage { prefix, package } => {
                self.each_target(candidate, |_| uninstall_global_package(prefix, package))
            }
        }
    }

    fn refusal(&self, candidate: &Candidate) -> Option<String> {
        if self.progress.cancelled.load(Ordering::Relaxed) {
            return Some("cancelled".to_string());
        }
        if candidate.safety == Safety::Blocked {
            return Some(format!("blocked: {}", candidate.reason));
        }
        let root = candidate.occupancy_root.clone()?;
        let occupants = self.processes.occupants(&[root]).remove(0);
        (!occupants.is_empty())
            .then(|| format!("workspace now in use by {}", describe_occupants(&occupants)))
    }

    fn skip_all(&self, candidate: &Candidate, reason: &str) -> Vec<TargetResult> {
        self.each_target(candidate, |_| Outcome::Skipped(reason.to_string()))
    }

    fn each_target(
        &self,
        candidate: &Candidate,
        handle: impl Fn(&Path) -> Outcome,
    ) -> Vec<TargetResult> {
        candidate
            .targets
            .iter()
            .map(|target| {
                *self.progress.current.lock().unwrap() = target.path.display().to_string();
                let outcome = if self.progress.cancelled.load(Ordering::Relaxed) {
                    Outcome::Skipped("cancelled".to_string())
                } else {
                    handle(&target.path)
                };
                self.progress.finished_targets.fetch_add(1, Ordering::Relaxed);
                TargetResult { path: target.path.clone(), outcome }
            })
            .collect()
    }

    fn remove(&self, path: &Path) -> Outcome {
        if let Err(refusal) = deletable_within(self.scan_root, path) {
            return Outcome::Skipped(refusal);
        }
        let wide = WidePath::new(path);
        let attributes = match winfs::attributes(&wide) {
            Ok(attributes) => attributes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Outcome::Skipped("already gone".to_string())
            }
            Err(error) => return Outcome::Failed(error.to_string()),
        };
        if !winfs::is_directory(attributes) {
            return match winfs::delete_file(&wide) {
                Ok(()) => Outcome::Removed,
                Err(error) => Outcome::Skipped(format!("in use ({error})")),
            };
        }
        if winfs::is_link(attributes) {
            return match winfs::remove_directory(&wide) {
                Ok(()) => Outcome::Removed,
                Err(error) => Outcome::Failed(error.to_string()),
            };
        }
        self.remove_directory_tree(path, &wide)
    }

    /// Renaming first proves nothing inside is open and takes the folder away from other
    /// tools in one atomic step; only then are its contents deleted.
    fn remove_directory_tree(&self, path: &Path, wide: &WidePath) -> Outcome {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let already_trash = name.contains(TRASH_MARKER);
        let trash_path = if already_trash {
            path.to_path_buf()
        } else {
            let sequence = self.trash_sequence.fetch_add(1, Ordering::Relaxed);
            path.with_file_name(format!("{name}{TRASH_MARKER}{}-{sequence}", std::process::id()))
        };
        let wide_trash = WidePath::new(&trash_path);
        if !already_trash {
            if let Err(error) = winfs::rename(wide, &wide_trash) {
                return Outcome::Skipped(format!("in use ({error})"));
            }
        }
        match delete_tree(&wide_trash) {
            0 => Outcome::Removed,
            failures => Outcome::Failed(format!(
                "{failures} entries could not be deleted; the rest is left at {}",
                trash_path.display()
            )),
        }
    }

    fn remove_unlinked_cache_entry(&self, entry: &Path) -> Outcome {
        match linked_file_count(&WidePath::new(entry)) {
            Ok(0) => {}
            Ok(linked) => {
                return Outcome::Skipped(format!("{linked} files are linked into a project again"))
            }
            Err(error) => return Outcome::Failed(error.to_string()),
        }
        let outcome = self.remove(entry);
        if outcome == Outcome::Removed {
            remove_bun_version_junction(entry);
        }
        outcome
    }

    fn remove_worktree(
        &self,
        repository: &Path,
        worktree: &Path,
        regenerable: &[PathBuf],
        verified_head: &str,
    ) -> Outcome {
        if let Err(refusal) = deletable_within(self.scan_root, worktree) {
            return Outcome::Skipped(refusal);
        }
        let state = match git::inspect_worktree(worktree) {
            Err(error) => return Outcome::Failed(format!("git could not inspect it: {error}")),
            Ok(state) => state,
        };
        if let Some(changes) = state.local_changes() {
            return Outcome::Skipped(format!("no longer removable: {changes}"));
        }
        if state.head != verified_head {
            return Outcome::Skipped("it has new commits since the analysis".to_string());
        }
        for generated in regenerable {
            if let Outcome::Skipped(reason) | Outcome::Failed(reason) = self.remove(generated) {
                if reason != "already gone" {
                    return Outcome::Skipped(format!("{}: {reason}", generated.display()));
                }
            }
        }
        match git::remove_worktree(repository, worktree, state.has_submodules) {
            Ok(()) => Outcome::Removed,
            Err(error) => Outcome::Failed(format!("git worktree remove: {error}")),
        }
    }
}

fn uninstall_global_package(prefix: &Path, package: &str) -> Outcome {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let uninstalled = std::process::Command::new("cmd")
        .args(["/c", "npm", "uninstall", "--global", package, "--prefix"])
        .arg(prefix)
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match uninstalled {
        Ok(output) if output.status.success() => Outcome::Removed,
        Ok(output) => Outcome::Failed(String::from_utf8_lossy(&output.stderr).trim().to_string()),
        Err(error) => Outcome::Failed(format!("could not start npm: {error}")),
    }
}

fn deletable_within(scan_root: &Path, path: &Path) -> Result<(), String> {
    let inside_scan = path.starts_with(scan_root) && path != scan_root;
    if !path.is_absolute() || !inside_scan || path.parent().is_none() {
        return Err(format!("refused: outside {}", scan_root.display()));
    }
    if path.components().any(|component| component.as_os_str() == ".git") {
        return Err("refused: git data is never deleted".to_string());
    }
    if territory::is_inside_installed_software(path) {
        return Err("refused: Windows and installed programs are never deleted".to_string());
    }
    Ok(())
}

enum Child {
    File(WidePath),
    Link(WidePath),
    Directory(WidePath),
}

fn list_children(directory: &WidePath) -> io::Result<Vec<Child>> {
    let mut children = Vec::new();
    winfs::read_directory(directory, |entry| {
        let path = directory.join(entry.name);
        children.push(match (entry.is_directory(), entry.is_link()) {
            (false, _) => Child::File(path),
            (true, true) => Child::Link(path),
            (true, false) => Child::Directory(path),
        });
    })?;
    Ok(children)
}

/// Deletes in parallel, never following junctions or symlinks. Returns how many entries survived.
fn delete_tree(directory: &WidePath) -> u64 {
    let Ok(children) = list_children(directory) else {
        return 1;
    };
    let surviving: u64 = children
        .par_iter()
        .with_min_len(8)
        .map(|child| match child {
            Child::File(path) => u64::from(winfs::delete_file(path).is_err()),
            Child::Link(path) => u64::from(winfs::remove_directory(path).is_err()),
            Child::Directory(path) => delete_tree(path),
        })
        .sum();
    surviving + u64::from(winfs::remove_directory(directory).is_err())
}

fn linked_file_count(directory: &WidePath) -> io::Result<u64> {
    let mut linked = 0;
    for child in list_children(directory)? {
        linked += match child {
            Child::File(path) => u64::from(winfs::hard_link_count(&path)? > 1),
            Child::Link(_) => 0,
            Child::Directory(path) => linked_file_count(&path)?,
        };
    }
    Ok(linked)
}

/// Bun keeps `<cache>\<name>\<version>@@@1` as a junction to `<cache>\<name>@<version>@@@1`.
fn bun_version_junction(entry: &Path) -> Option<PathBuf> {
    let entry_name = entry.file_name()?.to_str()?;
    let marker = entry_name.find("@@@")?;
    let version_separator = entry_name[..marker].rfind('@').filter(|&position| position > 0)?;
    let (package, version) =
        (&entry_name[..version_separator], &entry_name[version_separator + 1..]);
    Some(entry.parent()?.join(package).join(version))
}

fn remove_bun_version_junction(entry: &Path) {
    let Some(junction) = bun_version_junction(entry) else {
        return;
    };
    let wide_junction = WidePath::new(&junction);
    if winfs::attributes(&wide_junction).is_ok_and(winfs::is_link) {
        let _ = winfs::remove_directory(&wide_junction);
    }
    if let Some(versions) = junction.parent() {
        let _ = winfs::remove_directory(&WidePath::new(versions));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Kind, Target};
    use std::fs;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn make_junction(link: &Path, target: &Path) {
        let created = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(std::path::absolute(link).unwrap())
            .arg(std::path::absolute(target).unwrap())
            .output()
            .unwrap();
        assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    }

    fn candidate(action: Action, paths: &[PathBuf]) -> Candidate {
        Candidate {
            kind: Kind::BuildOutput,
            safety: Safety::Safe,
            title: String::new(),
            detail: String::new(),
            reason: String::new(),
            location: 0,
            size: 0,
            frees: 0,
            action,
            targets: paths
                .iter()
                .map(|path| Target { node: 0, path: path.clone(), frees: 0 })
                .collect(),
            occupancy_root: None,
        }
    }

    fn outcomes(root: &Path, candidate: Candidate) -> Vec<Outcome> {
        clean(root, &[candidate], &CleanProgress::default())
            .results
            .into_iter()
            .map(|result| result.outcome)
            .collect()
    }

    #[test]
    fn removes_nested_trees_including_read_only_files() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("project/bin");
        write(&output.join("Debug/net9.0/app.dll"), "binary");
        write(&output.join("Debug/app.pdb"), "symbols");
        let read_only = output.join("Debug/locked.config");
        write(&read_only, "config");
        let mut permissions = fs::metadata(&read_only).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&read_only, permissions).unwrap();

        let result =
            outcomes(root.path(), candidate(Action::DeleteTrees, std::slice::from_ref(&output)));

        assert_eq!(result, vec![Outcome::Removed]);
        assert!(!output.exists());
        assert!(root.path().join("project").exists());
    }

    #[test]
    fn junction_inside_a_deleted_tree_keeps_its_target() {
        let root = tempfile::tempdir().unwrap();
        let precious = root.path().join("precious");
        write(&precious.join("thesis.docx"), "years of work");
        let dependencies = root.path().join("project/node_modules");
        write(&dependencies.join("left-pad/index.js"), "module.exports = 1");
        make_junction(&dependencies.join("linked-package"), &precious);

        let result = outcomes(
            root.path(),
            candidate(Action::DeleteTrees, std::slice::from_ref(&dependencies)),
        );

        assert_eq!(result, vec![Outcome::Removed]);
        assert!(!dependencies.exists());
        assert_eq!(fs::read_to_string(precious.join("thesis.docx")).unwrap(), "years of work");
    }

    #[test]
    fn tree_with_an_open_file_is_skipped_untouched() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("project/obj");
        write(&output.join("a.cache"), "a");
        write(&output.join("deep/b.cache"), "b");
        let held_open = fs::File::open(output.join("deep/b.cache")).unwrap();

        let result =
            outcomes(root.path(), candidate(Action::DeleteTrees, std::slice::from_ref(&output)));

        assert!(matches!(&result[0], Outcome::Skipped(reason) if reason.starts_with("in use")));
        assert!(output.join("a.cache").exists());
        assert!(output.join("deep/b.cache").exists());
        drop(held_open);
    }

    #[test]
    fn refuses_paths_outside_the_scan_root_and_git_data() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        write(&elsewhere.path().join("keep/file.txt"), "keep");
        write(&root.path().join("repo/.git/objects/pack/pack.idx"), "objects");
        let targets = [
            elsewhere.path().join("keep"),
            root.path().join("repo/.git/objects"),
            root.path().to_path_buf(),
        ];

        let result = outcomes(root.path(), candidate(Action::DeleteTrees, &targets));

        assert!(result.iter().all(|outcome| matches!(outcome, Outcome::Skipped(_))));
        assert!(elsewhere.path().join("keep/file.txt").exists());
        assert!(root.path().join("repo/.git/objects/pack/pack.idx").exists());
    }

    #[test]
    fn refuses_windows_and_installed_programs_even_inside_the_scan_root() {
        let drive = Path::new(r"C:\");
        for installed in
            [r"C:\Windows\Temp\old", r"C:\Program Files\cursor\resources\app\node_modules"]
        {
            assert!(deletable_within(drive, Path::new(installed)).is_err(), "{installed}");
        }
        assert!(deletable_within(drive, Path::new(r"C:\Users\someone\Dev\project\bin")).is_ok());
    }

    #[test]
    fn blocked_candidates_are_never_executed() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("project/bin");
        write(&output.join("app.dll"), "binary");
        let mut blocked = candidate(Action::DeleteTrees, std::slice::from_ref(&output));
        blocked.safety = Safety::Blocked;

        let result = outcomes(root.path(), blocked);

        assert!(matches!(&result[0], Outcome::Skipped(_)));
        assert!(output.join("app.dll").exists());
    }

    #[test]
    fn cache_entry_is_removed_with_its_junction_only_while_nothing_links_to_it() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let unlinked = cache.join("left-pad@1.3.0@@@1");
        let linked = cache.join("@scope").join("core@7.29.0@@@1");
        write(&unlinked.join("index.js"), "unlinked");
        write(&linked.join("lib/index.js"), "linked");
        fs::create_dir_all(cache.join("left-pad")).unwrap();
        make_junction(&cache.join("left-pad/1.3.0@@@1"), &unlinked);
        let installed = root.path().join("project/node_modules/@scope/core/lib/index.js");
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::hard_link(linked.join("lib/index.js"), &installed).unwrap();

        let result = outcomes(
            root.path(),
            candidate(Action::DeleteUnlinkedCacheEntries, &[unlinked.clone(), linked.clone()]),
        );

        assert_eq!(result[0], Outcome::Removed);
        assert!(matches!(&result[1], Outcome::Skipped(_)));
        assert!(!unlinked.exists());
        assert!(!cache.join("left-pad").exists());
        assert!(linked.join("lib/index.js").exists());
    }

    #[test]
    fn bun_junction_path_splits_scoped_and_plain_names_at_the_version() {
        let plain = bun_version_junction(Path::new(r"D:\cache\accepts@2.0.0@@@1")).unwrap();
        let scoped = bun_version_junction(Path::new(r"D:\cache\@babel\core@7.29.0@@@1")).unwrap();

        assert_eq!(plain, Path::new(r"D:\cache\accepts\2.0.0@@@1"));
        assert_eq!(scoped, Path::new(r"D:\cache\@babel\core\7.29.0@@@1"));
    }
}
