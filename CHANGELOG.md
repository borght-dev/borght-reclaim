# Changelog

## Unreleased

- Worktrees with submodules can be removed. Git refuses those unless forced, so Reclaim first checks every submodule, deinitialised ones included, for uncommitted files, stashes and commits no remote has

## 0.1.0

First public version.

- Window with a folder tree, a treemap and a rated clean-up plan
- `scan`, `plan`, `clean` and `config` commands
- Saved index with change journal replay on NTFS and ReFS
- Hardlink-aware sizes
- Drive switcher in the window header
- Windows and installed programs are never offered or deleted
