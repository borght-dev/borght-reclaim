use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occupant {
    pub pid: u32,
    pub name: String,
}

#[derive(Default)]
pub struct RunningProcess {
    pub pid: u32,
    pub name: String,
    pub working_directory: Option<String>,
    pub executable: Option<String>,
    pub arguments: Vec<String>,
}

#[derive(Default)]
pub struct ProcessSnapshot {
    processes: Vec<RunningProcess>,
}

impl ProcessSnapshot {
    pub fn capture() -> Self {
        let mut system = System::new();
        let wanted = ProcessRefreshKind::nothing()
            .with_cwd(UpdateKind::Always)
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always);
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, wanted);

        let own_pid = std::process::id();
        let processes = system
            .processes()
            .iter()
            .filter(|(pid, _)| pid.as_u32() != own_pid)
            .map(|(pid, process)| RunningProcess {
                pid: pid.as_u32(),
                name: process.name().to_string_lossy().into_owned(),
                working_directory: process.cwd().map(|path| path.to_string_lossy().into_owned()),
                executable: process.exe().map(|path| path.to_string_lossy().into_owned()),
                arguments: process
                    .cmd()
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect(),
            })
            .collect();
        Self { processes }
    }

    #[cfg(test)]
    pub fn of(processes: Vec<RunningProcess>) -> Self {
        Self { processes }
    }

    /// For every root, the processes running in it or started with a path inside it.
    /// A process located in a nested root only occupies the deepest one.
    pub fn occupants(&self, roots: &[PathBuf]) -> Vec<Vec<Occupant>> {
        let normalized_roots: Vec<String> =
            roots.iter().map(|root| normalized(&root.to_string_lossy())).collect();
        let mut deepest_first: Vec<usize> = (0..roots.len()).collect();
        deepest_first.sort_by_key(|&index| std::cmp::Reverse(normalized_roots[index].len()));

        let mut occupants = vec![Vec::new(); roots.len()];
        for process in &self.processes {
            let mut occupied = BTreeSet::new();
            let locations = process.working_directory.iter().chain(&process.executable);
            for location in locations.map(|location| normalized(location)) {
                let deepest = deepest_first
                    .iter()
                    .find(|&&index| is_inside(&location, &normalized_roots[index]));
                occupied.extend(deepest.copied());
            }
            for argument in process.arguments.iter().filter(|argument| argument.contains(':')) {
                let argument = normalized(argument);
                occupied.extend(deepest_roots_mentioned(
                    &argument,
                    &normalized_roots,
                    &deepest_first,
                ));
            }
            for index in occupied {
                occupants[index].push(Occupant { pid: process.pid, name: process.name.clone() });
            }
        }
        occupants
    }
}

pub fn normalized(path: &str) -> String {
    let unified = path.replace('/', "\\").to_lowercase();
    let without_prefix = unified.strip_prefix(r"\\?\").unwrap_or(&unified);
    without_prefix.trim_end_matches('\\').to_string()
}

fn is_inside(location: &str, root: &str) -> bool {
    location.strip_prefix(root).is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'))
}

fn deepest_roots_mentioned(
    argument: &str,
    roots: &[String],
    deepest_first: &[usize],
) -> Vec<usize> {
    let mut claimed_positions = Vec::new();
    let mut mentioned = Vec::new();
    for &index in deepest_first {
        let root = &roots[index];
        if root.is_empty() {
            continue;
        }
        for (position, _) in argument.match_indices(root.as_str()) {
            let continues_a_longer_name = argument[position + root.len()..]
                .chars()
                .next()
                .is_some_and(|next| next.is_alphanumeric() || matches!(next, '-' | '_' | '.'));
            let starts_mid_word =
                argument[..position].chars().next_back().is_some_and(char::is_alphanumeric);
            if continues_a_longer_name || starts_mid_word || claimed_positions.contains(&position) {
                continue;
            }
            claimed_positions.push(position);
            mentioned.push(index);
        }
    }
    mentioned
}

pub fn describe_occupants(occupants: &[Occupant]) -> String {
    const SHOWN: usize = 3;
    let mut names: Vec<&str> = occupants.iter().map(|occupant| occupant.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    let shown = names.iter().take(SHOWN).copied().collect::<Vec<_>>().join(", ");
    if names.len() > SHOWN {
        format!("{shown} +{} more", names.len() - SHOWN)
    } else {
        shown
    }
}

/// Claude Code keeps one transcript folder per working directory; its newest file tells when
/// an agent last worked there, even if that agent is idle right now.
pub struct AgentSessions {
    project_stores: Vec<PathBuf>,
}

impl AgentSessions {
    pub fn discover() -> Self {
        let Some(home) = std::env::var_os("USERPROFILE").map(PathBuf::from) else {
            return Self { project_stores: Vec::new() };
        };
        let project_stores = std::fs::read_dir(&home)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".claude"))
            .map(|entry| entry.path().join("projects"))
            .filter(|projects| projects.is_dir())
            .collect();
        Self { project_stores }
    }

    #[cfg(test)]
    pub fn none() -> Self {
        Self { project_stores: Vec::new() }
    }

    pub fn last_activity_unix(&self, working_directory: &Path) -> Option<i64> {
        let slug: String = working_directory
            .to_string_lossy()
            .chars()
            .map(|character| if character.is_ascii_alphanumeric() { character } else { '-' })
            .collect();
        self.project_stores.iter().filter_map(|store| newest_entry_unix(&store.join(&slug))).max()
    }
}

fn newest_entry_unix(directory: &Path) -> Option<i64> {
    std::fs::read_dir(directory)
        .ok()?
        .flatten()
        .filter_map(|entry| modified_unix(&entry.path()))
        .max()
}

pub fn modified_unix(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, working_directory: &str, arguments: &[&str]) -> RunningProcess {
        RunningProcess {
            pid,
            name: format!("process-{pid}"),
            working_directory: Some(working_directory.to_string()),
            executable: None,
            arguments: arguments.iter().map(|argument| argument.to_string()).collect(),
        }
    }

    fn occupant_pids(snapshot: &ProcessSnapshot, roots: &[&str]) -> Vec<Vec<u32>> {
        let roots: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
        snapshot
            .occupants(&roots)
            .into_iter()
            .map(|occupants| occupants.into_iter().map(|occupant| occupant.pid).collect())
            .collect()
    }

    #[test]
    fn working_directory_occupies_only_the_deepest_root() {
        let snapshot =
            ProcessSnapshot::of(vec![process(1, r"D:\Repo\.worktrees\feature\src", &[])]);

        let pids = occupant_pids(&snapshot, &[r"D:\Repo", r"D:\Repo\.worktrees\feature"]);

        assert_eq!(pids, vec![vec![], vec![1]]);
    }

    #[test]
    fn sibling_with_a_longer_name_is_not_an_occupant() {
        let snapshot = ProcessSnapshot::of(vec![process(
            1,
            r"D:\Shop-Next",
            &["--project", r"D:\Shop.Portal\app.csproj"],
        )]);

        let pids = occupant_pids(&snapshot, &[r"D:\Shop"]);

        assert_eq!(pids, vec![Vec::<u32>::new()]);
    }

    #[test]
    fn path_inside_an_argument_counts_even_with_forward_slashes_and_flags() {
        let snapshot = ProcessSnapshot::of(vec![
            process(1, r"C:\Windows", &["node", "--cwd=d:/repo/app", "serve"]),
            process(2, r"C:\Windows", &[r#"cmd /c "cd /d D:\Repo && npm run dev""#]),
        ]);

        let pids = occupant_pids(&snapshot, &[r"D:\Repo"]);

        assert_eq!(pids, vec![vec![1, 2]]);
    }

    #[test]
    fn session_slug_replaces_every_separator() {
        let home = tempfile::tempdir().unwrap();
        let store = home.path().join("projects");
        let transcripts = store.join("D--Shop-Next--worktrees-one");
        std::fs::create_dir_all(&transcripts).unwrap();
        std::fs::write(transcripts.join("session.jsonl"), "{}").unwrap();
        let sessions = AgentSessions { project_stores: vec![store] };

        let known = sessions.last_activity_unix(Path::new(r"D:\Shop-Next\.worktrees\one"));
        let unknown = sessions.last_activity_unix(Path::new(r"D:\elsewhere"));

        assert!(known.is_some());
        assert!(unknown.is_none());
    }
}
