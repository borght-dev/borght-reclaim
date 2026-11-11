use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::clean::{self, CleanProgress, CleanReport, Outcome};
use crate::format;
use crate::gui;
use crate::plan::{analyze, Candidate, Config, Depth, Kind, Plan, Safety};
use crate::scan::{ScanProgress, Tree, ROOT};
use crate::snapshot;
use crate::winfs;

const USAGE: &str = "\
Borght Reclaim — fast disk usage analyzer and safe clean-up for dev drives

USAGE
  reclaim [PATH]                     open the window (default path: config root, else the system drive)
  reclaim scan  [PATH] [--top N] [--export FILE.tsv]
                                     print the largest folders; export everything of 16 MB+
  reclaim plan  [PATH] [--json]      list what can be removed, and why
  reclaim clean [PATH] [--yes] [--include-review]
                                     remove everything rated safe; a dry run without --yes
  reclaim config                     print the effective configuration

OPTIONS
  --full                             walk every folder instead of replaying the change journal
  --screenshot FILE                  (window) save a PNG once the scan is shown, then exit
  --click X,Y                        (window) click that point once a scan is shown; with
                                     --screenshot the picture shows what the click led to
  --include-review                   (clean) also remove build output, dependencies and caches
                                     rated \"review\"; worktrees and large files stay manual

Thresholds live in %USERPROFILE%\\.reclaim.json (see `reclaim config`).";

struct Arguments {
    command: String,
    path: Option<PathBuf>,
    json: bool,
    confirmed: bool,
    include_review: bool,
    top: usize,
    screenshot: Option<PathBuf>,
    click: Option<[f32; 2]>,
    export: Option<PathBuf>,
    full_scan: bool,
}

fn parse_point(text: &str) -> Result<[f32; 2], String> {
    let malformed = || format!("--click: expected X,Y but got {text}");
    let (x, y) = text.split_once(',').ok_or_else(malformed)?;
    Ok([x.trim().parse().map_err(|_| malformed())?, y.trim().parse().map_err(|_| malformed())?])
}

const EXPORT_MINIMUM_BYTES: u64 = 16 * 1024 * 1024;

fn parse(arguments: &[String]) -> Result<Arguments, String> {
    const COMMANDS: [&str; 6] = ["gui", "scan", "plan", "clean", "config", "help"];
    let mut parsed = Arguments {
        command: "gui".to_string(),
        path: None,
        json: false,
        confirmed: false,
        include_review: false,
        top: 25,
        screenshot: None,
        click: None,
        export: None,
        full_scan: false,
    };
    let mut remaining = arguments.iter().peekable();
    if let Some(first) = remaining.peek().filter(|first| COMMANDS.contains(&first.as_str())) {
        parsed.command = first.to_string();
        remaining.next();
    }
    while let Some(argument) = remaining.next() {
        match argument.as_str() {
            "--json" => parsed.json = true,
            "--full" => parsed.full_scan = true,
            "--yes" | "-y" => parsed.confirmed = true,
            "--include-review" => parsed.include_review = true,
            "--help" | "-h" => parsed.command = "help".to_string(),
            "--top" => {
                let value = remaining.next().ok_or("--top needs a number")?;
                parsed.top = value.parse().map_err(|_| format!("--top: not a number: {value}"))?;
            }
            "--screenshot" => {
                let file = remaining.next().ok_or("--screenshot needs a file path")?;
                parsed.screenshot = Some(PathBuf::from(file));
            }
            "--click" => {
                parsed.click = Some(parse_point(remaining.next().ok_or("--click needs X,Y")?)?);
            }
            "--export" => {
                let file = remaining.next().ok_or("--export needs a file path")?;
                parsed.export = Some(PathBuf::from(file));
            }
            option if option.starts_with("--") => return Err(format!("unknown option {option}")),
            path if parsed.path.is_none() => parsed.path = Some(PathBuf::from(path)),
            extra => return Err(format!("unexpected argument {extra}")),
        }
    }
    Ok(parsed)
}

pub fn run(arguments: &[String]) -> Result<(), String> {
    let arguments = parse(arguments)?;
    let config = Config::load()?;
    let root = arguments.path.clone().unwrap_or_else(|| config.root.clone());
    match arguments.command.as_str() {
        "help" => println!("{USAGE}"),
        "config" => print_config(&config)?,
        "scan" => {
            let tree = scan_with_progress(&root, arguments.full_scan)?;
            export_if_asked(&tree, &arguments)?;
            print_scan(&tree, arguments.top);
        }
        "plan" => {
            let tree = scan_with_progress(&root, arguments.full_scan)?;
            export_if_asked(&tree, &arguments)?;
            let plan = analyze(&tree, &config, Depth::Thorough);
            if arguments.json {
                println!("{}", serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?);
            } else {
                print_plan(&tree, &plan);
            }
        }
        "clean" => clean_command(&root, &config, &arguments)?,
        _ => {
            let script = gui::Script { screenshot: arguments.screenshot, click: arguments.click };
            gui::run(config, root, script)?
        }
    }
    Ok(())
}

/// Tab-separated rows for every file and folder of 16 MB or more: size, frees, files, modified, path.
fn export_if_asked(tree: &Tree, arguments: &Arguments) -> Result<(), String> {
    let Some(file) = &arguments.export else {
        return Ok(());
    };
    let mut rows = String::from("size\tfrees\tfiles\tmodified_unix\tkind\tpath\n");
    for id in 0..tree.len() as u32 {
        let node = tree.node(id);
        if node.size < EXPORT_MINIMUM_BYTES {
            continue;
        }
        let kind = if tree.is_directory(id) { "dir" } else { "file" };
        rows.push_str(&format!(
            "{}\t{}\t{}\t{}\t{kind}\t{}\n",
            node.size,
            node.frees,
            node.file_count,
            node.modified_unix,
            tree.path(id).display()
        ));
    }
    std::fs::write(file, rows).map_err(|error| format!("{}: {error}", file.display()))
}

fn print_config(config: &Config) -> Result<(), String> {
    let path = Config::file_path().map_or(String::new(), |path| path.display().to_string());
    println!("# {path}");
    println!("{}", serde_json::to_string_pretty(config).map_err(|error| error.to_string())?);
    Ok(())
}

fn scan_with_progress(root: &Path, force_full_scan: bool) -> Result<Tree, String> {
    let progress = ScanProgress::default();
    let current = std::thread::scope(|scope| {
        let scanning =
            scope.spawn(|| snapshot::current_index(root, &progress, force_full_scan, |_| {}));
        let started = Instant::now();
        while !scanning.is_finished() {
            std::thread::sleep(Duration::from_millis(50));
            if started.elapsed() > Duration::from_millis(500) {
                eprint!(
                    "\r{}: {} files  {}   ",
                    progress.status(),
                    format::count(progress.files.load(Ordering::Relaxed)),
                    format::bytes(progress.bytes.load(Ordering::Relaxed)),
                );
            }
        }
        eprint!("\r{:78}\r", "");
        scanning.join().unwrap()
    })
    .map_err(|error| format!("cannot scan {}: {error}", root.display()))?;
    if let Some(reason) = current.full_scan_reason.filter(|_| !force_full_scan) {
        eprintln!("reclaim: walked every folder ({reason})");
    }
    Ok(current.index.tree())
}

fn print_volume(tree: &Tree) {
    let stats = tree.stats;
    if let Ok(space) = winfs::volume_space(tree.root_path()) {
        println!(
            "{}  {} volume, {} free",
            tree.root_path().display(),
            format::bytes(space.total_bytes),
            format::bytes(space.free_bytes),
        );
    }
    let pass = match stats.refreshed_directories {
        Some(changed) => format!("re-read {} changed folders", format::count(changed)),
        None => "walked every folder".to_string(),
    };
    println!(
        "{} files in {} folders; {pass} in {:.2}s  ({} on disk, {} files hardlinked, {} folders unreadable)",
        format::count(stats.files),
        format::count(stats.directories),
        stats.elapsed.as_secs_f64(),
        format::bytes(tree.node(ROOT).size),
        format::count(stats.hardlinked_files),
        format::count(stats.unreadable_directories),
    );
}

fn print_scan(tree: &Tree, top: usize) {
    print_volume(tree);
    let mut children: Vec<_> = tree.children(ROOT).collect();
    children.sort_by_key(|&child| std::cmp::Reverse(tree.node(child).size));
    println!("\n{:>10} {:>10} {:>9}  PATH", "SIZE", "FREES", "FILES");
    for child in children.into_iter().take(top) {
        let node = tree.node(child);
        println!(
            "{:>10} {:>10} {:>9}  {}{}",
            format::bytes(node.size),
            format::bytes(node.frees),
            format::count(u64::from(node.file_count)),
            tree.name(child),
            if tree.is_directory(child) { "\\" } else { "" },
        );
    }
}

fn print_plan(tree: &Tree, plan: &Plan) {
    print_volume(tree);
    let sections = [
        (Safety::Safe, "SAFE TO REMOVE"),
        (Safety::Review, "REVIEW (your call)"),
        (Safety::Blocked, "BLOCKED (active or unsaved work)"),
    ];
    for (safety, heading) in sections {
        let candidates: Vec<&Candidate> =
            plan.candidates.iter().filter(|candidate| candidate.safety == safety).collect();
        if candidates.is_empty() {
            continue;
        }
        println!("\n{heading}: frees {}", format::bytes(plan.frees(safety)));
        for candidate in candidates {
            println!(
                "{:>10}  {:<15} {}  [{}]\n{:>10}  {:<15} {}",
                format::bytes(candidate.frees),
                candidate.kind.label(),
                candidate.title,
                candidate.detail,
                "",
                "",
                candidate.reason,
            );
        }
    }
}

fn clean_command(root: &Path, config: &Config, arguments: &Arguments) -> Result<(), String> {
    let tree = scan_with_progress(root, arguments.full_scan)?;
    let depth = if arguments.include_review { Depth::Thorough } else { Depth::Quick };
    let plan = analyze(&tree, config, depth);
    let chosen: Vec<Candidate> = plan
        .candidates
        .iter()
        .filter(|candidate| is_chosen_for_bulk_clean(candidate, arguments.include_review))
        .cloned()
        .collect();
    print_volume(&tree);
    if chosen.is_empty() {
        println!("\nnothing rated safe to remove right now; `reclaim plan` shows why");
        return Ok(());
    }
    let expected = plan.frees_together(&tree, |index| {
        is_chosen_for_bulk_clean(&plan.candidates[index], arguments.include_review)
    });
    println!();
    for candidate in &chosen {
        println!(
            "{:>10}  {:<15} {}  [{}]",
            format::bytes(candidate.frees),
            candidate.kind.label(),
            candidate.title,
            candidate.detail
        );
    }
    if !arguments.confirmed {
        println!("\ndry run: would free about {}. Add --yes to delete.", format::bytes(expected));
        return Ok(());
    }

    let progress = CleanProgress::default();
    let report = std::thread::scope(|scope| {
        let cleaning = scope.spawn(|| clean::clean(tree.root_path(), &chosen, &progress));
        while !cleaning.is_finished() {
            std::thread::sleep(Duration::from_millis(200));
            eprint!(
                "\rdeleting {}/{}   ",
                progress.finished_targets.load(Ordering::Relaxed),
                progress.total_targets.load(Ordering::Relaxed)
            );
        }
        eprint!("\r{:40}\r", "");
        cleaning.join().unwrap()
    });
    print_report(&report);
    Ok(())
}

fn is_chosen_for_bulk_clean(candidate: &Candidate, include_review: bool) -> bool {
    let bulk_reviewable =
        matches!(candidate.kind, Kind::BuildOutput | Kind::Dependencies | Kind::PackageCache);
    match candidate.safety {
        Safety::Safe => true,
        Safety::Review => include_review && bulk_reviewable,
        Safety::Blocked => false,
    }
}

fn print_report(report: &CleanReport) {
    println!(
        "\nfreed {} (volume free space {} -> {}), removed {} items",
        format::bytes(report.freed_bytes()),
        format::bytes(report.free_bytes_before),
        format::bytes(report.free_bytes_after),
        report.removed_count(),
    );
    for result in report.not_removed() {
        let (label, reason) = match &result.outcome {
            Outcome::Skipped(reason) => ("skipped", reason),
            Outcome::Failed(reason) => ("FAILED", reason),
            Outcome::Removed => continue,
        };
        println!("  {label}: {}  ({reason})", result.path.display());
    }
}
