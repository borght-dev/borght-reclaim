# Contributing

Issues and pull requests are welcome.

## Build and test

Windows, Rust stable with the MSVC toolchain, and `git` on the `PATH`.

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs the same three commands.

The tests create real folders, hardlinks, junctions and git repositories in the temp folder. They never touch anything outside it.

## Try a change without risk

`scripts\New-DemoDrive.ps1` builds a small tree of fake projects with backdated timestamps. Scan that instead of your own drive:

```powershell
.\scripts\New-DemoDrive.ps1 -Path $env:TEMP\reclaim-demo
cargo run --release -- plan $env:TEMP\reclaim-demo
```

For a change to the window, save a picture and look at it:

```powershell
cargo run --release -- $env:TEMP\reclaim-demo --screenshot shot.png
```

`--click X,Y` clicks a point, in logical pixels, before the picture is taken.

## Where things are

| File | Holds |
| --- | --- |
| `src/scan.rs`, `src/winfs.rs` | the folder walk and the Win32 calls |
| `src/index.rs`, `src/journal.rs`, `src/snapshot.rs` | the saved index and the change journal replay |
| `src/plan.rs` | the rules that find and rate what can be removed |
| `src/territory.rs` | which parts of a drive are off limits |
| `src/activity.rs`, `src/git.rs` | running processes, agent sessions, git state |
| `src/clean.rs` | deleting |
| `src/gui/` | the window |

## Rules for rule changes

A new rule in `plan.rs` or a change to `clean.rs` needs a test that builds the situation on disk. When a rule cannot prove that something is unused, it rates the item "your call" or "blocked". It never rates it safe.

## Style

Names carry the meaning. A comment explains why, never what. `cargo fmt` decides the layout.
