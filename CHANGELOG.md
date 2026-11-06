# Changelog

Every release of `ccm` and what changed in it. Each section is also used as the notes of its release on GitHub.

## Unreleased

### Added

- **Codex conversations.** They show in the list with search and preview, next to Claude Code's. A Tool column and the `t` filter appear when both tools have conversations. Runs of `codex exec` and conversations Codex starts for itself are left out. For now they can only be read: resume, fork, delete, rename and crash recovery come later. ([#37](https://github.com/nunoaleixogoncalves/ccm/issues/37))
- New setting `tools` in `config.toml` to choose which tools to read. By default, every tool whose folder exists is read.

## [1.1.1] - 2026-11-06

### Fixed

- **Resuming on WSL.** `ccm` now opens a new Windows Terminal tab there, starting the conversation in its Linux folder through `wsl.exe --cd`. Before, it tried `xdg-terminal-exec`, which WSL usually doesn't have, and resuming failed with "No such file or directory". ([#33](https://github.com/nunoaleixogoncalves/ccm/pull/33))
- When a terminal can't be started, the message now says why: that the launcher isn't installed, or what it printed before failing. ([#33](https://github.com/nunoaleixogoncalves/ccm/pull/33))
- **Worktrees made by Git for Windows, seen from WSL.** Git in WSL can't reach a folder recorded as `C:/...`, so it reported such a worktree as missing. `ccm` then suggested it on the clean-up screen and would have cleared Git's record of a worktree that still exists on Windows. It now shows these as "Windows" and never suggests or removes them. ([#34](https://github.com/nunoaleixogoncalves/ccm/pull/34))

## [1.1.0] - 2026-11-06

### Added

- **Worktree view.** Press `w` to see the Git worktrees of the repositories your conversations used, with their uncommitted changes, unpushed commits and the conversations that worked in each. ([#23](https://github.com/nunoaleixogoncalves/ccm/pull/23))
- **Removing worktrees.** Press `d` in the worktree view to remove the selected or marked worktrees with `git worktree remove`, never forced. The branch stays, so its commits are kept. ([#24](https://github.com/nunoaleixogoncalves/ccm/pull/24)) Worktrees with unpushed commits can be removed too, after a short countdown and a warning. ([#25](https://github.com/nunoaleixogoncalves/ccm/pull/25))
- **Group by repository.** `g` now groups by folder, then by Git repository, then not at all, so the work in a repository and all its worktrees shows together. ([#27](https://github.com/nunoaleixogoncalves/ccm/pull/27))
- **Fork.** Press `f` to start a fork of the selected conversation: a new conversation with a copy of the whole history, while the original stays as it was. ([#28](https://github.com/nunoaleixogoncalves/ccm/pull/28))
- **Clean-up screen.** Press `Shift+C` to see what could be cleaned up and how much space it would free. `d` deletes a line's items after one confirmation, and Space marks several lines to delete together. The lines are:
  - conversations Claude never replied to, ones whose folder is gone, and large ones not used for a while ([#29](https://github.com/nunoaleixogoncalves/ccm/pull/29))
  - worktrees no conversation worked in that have nothing to lose ([#30](https://github.com/nunoaleixogoncalves/ccm/pull/30))
  - background job folders left without a conversation ([#31](https://github.com/nunoaleixogoncalves/ccm/pull/31))
- New setting `old_after_days` in `config.toml`: how long a conversation must go unused before the clean-up screen calls it old. The default is 30.

### Changed

- On narrow terminals the footer can take a fifth row, so every button still fits.

### Fixed

- Popups now show the blank lines meant to separate their parts.

## [1.0.0] - 2026-11-05

The first release.

### Added

- **Browse** every conversation from every project in one table: folder, age, context used, messages, size, model, branch and status.
- **Search** by anything said in a conversation.
- **Preview** the latest messages and scroll back, without opening the conversation.
- **Resume** a conversation in a new terminal, in the folder it ran in, or in that folder's workspace inside `herdr-app`.
- **Delete** one or many conversations and their data, after a confirmation that lists what goes. `d` or Del.
- **Rename** a conversation. The new name also shows in Claude Code's own `/resume`.
- **Filters** for open, crashed, junk, missing-folder and large conversations, and grouping by folder.
- **Crash recovery:** after a crash, see what was running and reopen it all at once.
- `ccm setup` to install the hooks crash recovery needs, after showing the changes and asking.
- Ready-made programs for Linux (static, x86-64 and ARM) and macOS (Intel and Apple silicon), and an install script that checks their checksums.

[1.1.1]: https://github.com/nunoaleixogoncalves/ccm/compare/v1.1.0...v1.1.1
[1.1.0]: https://github.com/nunoaleixogoncalves/ccm/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/nunoaleixogoncalves/ccm/releases/tag/v1.0.0
