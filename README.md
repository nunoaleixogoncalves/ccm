# ccm

**A terminal app for finding, resuming and cleaning up your Claude Code conversations.**

![ccm showing a list of conversations with details and a preview of the selected one](docs/screenshot.png)

*The conversations in this screenshot are examples.*

## Why

If you run a lot of Claude Code sessions, they pile up:

- Finding an old conversation means remembering when and where it ran, or what it was called.
- Claude Code can purge a whole project, but not one conversation and everything stored with it.
- After a crash or reboot, nothing tells you which conversations were open or which folders they ran in.

`ccm` puts every conversation from every project in one table, with full-text search and a preview. It only uses the files Claude Code already keeps, and it opens in a fraction of a second, even with hundreds of conversations.

## What you can do

- **Browse:** see every conversation's folder, age, context used, messages, size, model, branch and status.
- **Search:** find a conversation by anything said in it.
- **Preview:** read the latest messages and scroll back, without opening the conversation.
- **Resume:** open it in a new terminal, in the folder it ran in. Inside `herdr-app`, it opens in that folder's workspace.
- **Fork:** start a new conversation as a copy of one, to try another direction while the original stays as it was.
- **Delete:** permanently remove one or many conversations and their data, after a confirmation that lists what goes.
- **Rename:** the new name also shows in Claude Code's own `/resume`.
- **Group:** by the folder a conversation ran in, or by Git repository, so the work in a repository and all its worktrees shows together.
- **Clean up:** filter for conversations Claude never replied to, ones whose folder is gone, and large ones.
- **Recover:** after a crash, see what was running and reopen it all at once.
- **Worktrees:** see the Git worktrees of the repositories your conversations used, with their uncommitted changes, unpushed commits and the conversations that worked in each, and remove the ones you're done with.

## Install

You need [Claude Code](https://claude.com/claude-code). On Linux or macOS, run:

```sh
curl -fsSL https://raw.githubusercontent.com/nunoaleixogoncalves/ccm/main/install.sh | sh
```

This downloads the ready-made program for your system from the [latest release](https://github.com/nunoaleixogoncalves/ccm/releases/latest), checks its checksum and puts it in `~/.local/bin`. To use another folder, set `CCM_INSTALL_DIR`. The script tells you if that folder isn't on your `PATH`. You can [read the script](install.sh) first, or download the program from the releases page yourself.

It then runs `ccm setup`, which shows the changes it would make to Claude Code's settings and applies them only if you answer `y`. Those changes are only needed for crash recovery and exact context window sizes. Everything else works without them.

The Linux programs are static, so they work on any distribution. The macOS ones aren't signed: installing with the command above is fine, but macOS may refuse one you download with a browser until you allow it in **System Settings → Privacy & Security**.

### Build from source

You need Git, [Rust](https://rustup.rs) (current stable) and a C compiler to build the bundled SQLite. Open a new terminal after installing Rust, so `cargo` is found.

| System | C compiler |
|---|---|
| macOS | `xcode-select --install` |
| Arch, Omarchy | `sudo pacman -S --needed base-devel` |
| Debian, Ubuntu | `sudo apt install build-essential` |
| Fedora | `sudo dnf install gcc` |

```sh
git clone https://github.com/nunoaleixogoncalves/ccm.git
cd ccm
./install.sh
```

If you use mise, install with this script and not `cargo install`. `cargo install` puts `ccm` where mise creates a shim that fails outside this repository.

### Optional tools

| Tool | What it's for | Without it |
|---|---|---|
| `wl-copy` or `xclip` | Copying on Linux | `ccm` asks your terminal to copy, which not every terminal allows |
| `xdg-terminal-exec` | Resuming in a new terminal on Linux | Set `terminal` in the [configuration](#configuration), or run the command `ccm` shows you |
| Git 2.36 or later | The [worktree view](#worktrees) and [grouping by repository](#grouping) | The view says Git wasn't found, and conversations stay grouped by folder |

## Update

Run the install command again. `ccm --version` shows which version you have.

If you built from source, run this from the repository folder instead:

```sh
git pull
./install.sh
```

## Usage

Run `ccm` from anywhere.

| Command | What it does |
|---|---|
| `ccm` | Opens the list |
| `ccm setup` | Installs the hooks crash recovery needs, after showing the changes |
| `ccm --help` | Shows the commands |
| `ccm --version` | Shows the version |

`ccm hook` and `ccm hook status-line` are also there, but Claude Code runs those. You won't need to.

### Keys

Most keys are also buttons in the footer, so you can click them. The footer splits them into **Actions**, which do something to a conversation, and **View**, which only change what the list shows.

Moving around:

| Key | What it does |
|---|---|
| Up, Down, or `k`, `j` | Move the selection |
| Page Up, Page Down, Home, End | Move further |
| `[`, `]` | Scroll the preview back and forward |

Actions:

| Key | What it does |
|---|---|
| Enter | Resume the selected conversation |
| `f` | Fork it. See [Forking](#forking) |
| `d` or Del, then `y` or Del | Delete the selected or marked conversations. You can also click Delete twice. A second Del needs a moment's pause, so holding the key can't delete anything |
| `m`, while confirming a delete | Choose whether to delete memory notes too. It only appears when there are notes to delete |
| `Shift+R` | Rename |
| Space | Mark or unmark, then move to the next row |
| `a` | Mark everything shown, or clear those marks |
| `y` | Copy the conversation's ID |
| `Shift+Y` | Copy the command that resumes it |
| `Shift+O` | Open the list of crashed conversations to reopen |
| Esc | Clear the marks, then the search. It never quits |
| `q`, Ctrl-C | Quit |

View:

| Key | What it does |
|---|---|
| `/` | Search. Enter keeps the results, Esc clears them |
| `o` | Show only open conversations |
| `x` | Show only crashed conversations |
| `Shift+J` | Show only junk, meaning conversations Claude never replied to |
| `m` | Show only conversations whose folder is missing |
| `l` | Show only large conversations |
| `c` | Clear the filters |
| `g` | Group by folder, then by repository, then not at all. See [Grouping](#grouping) |
| `s` | Change the sort column |
| `r` | Reverse the sort |
| `w` | Show the [worktrees](#worktrees). `w` or Esc goes back |

With the mouse, you can click a row to select it, double-click to resume, click a column heading to sort, and use the wheel to scroll.

## Forking

Press `f` to start a fork of the selected conversation. It's Claude Code's own fork (`claude --resume <id> --fork-session`): a new conversation that starts with a copy of the whole history, while the original stays as it was.

- It opens like a resume: in a new terminal, or in `herdr-app`, in the folder the conversation started in. Claude Code looks for the original there, so it can't start anywhere else.
- It's named after the original with "(fork)" added. Rename it with `Shift+R` as usual.
- You can fork an open conversation. The original carries on untouched.
- Claude Code records no link between a fork and its original, so `ccm` picks the fork's ID and keeps its own log of the forks it starts. The details panel shows "Forked from" on a fork and "Forks" on the original.
- The fork shows in the list once Claude Code saves it, which may be only after you send it a first message. `ccm` then selects it.
- Deleting either one leaves the other alone.

## Grouping

Press `g` once to group conversations by the folder they started in, and again to group them by Git repository. A third press turns grouping off.

- A repository's group holds the conversations that started in its main folder and in any of its worktrees. It's named by the main folder.
- Each conversation shows its branch, and its worktree when that isn't the main folder, as in `Fix the login form [login-fix · shop-login]`.
- `ccm` asks Git which repository each folder belongs to, once per folder, and keeps the answer. A worktree you remove later stays in its repository's group. Folders that weren't in a repository are asked about again each time you pick repository grouping.
- A folder that's gone and was never seen in Git goes to the repository whose folder contains it, as with `.claude/worktrees`. Otherwise it gets its own group, marked "folder gone". Folders outside Git keep their own groups, marked "not in Git".
- Until Git answers, which usually takes a fraction of a second, conversations stay grouped by folder.

## Worktrees

Press `w` to see the Git worktrees of every repository a conversation worked in. Each one shows its folder, branch, uncommitted changes, unpushed commits and how many conversations worked in it, and the panel below lists those conversations.

- `ccm` asks Git which folders are worktrees, so it finds them wherever they are, including beside the main repository. A repository's main worktree isn't listed.
- A conversation counts as having worked in a worktree if it was in that folder, or a folder inside it, at any point. Conversations often change folder, so the list says how recently each one was there, how much of its activity happened there, and whether it started or stopped there.
- Unpushed commits are the ones on no remote branch. In a repository without a remote, they're the ones on no other branch. Remote branches are as of your last fetch, because `ccm` never uses the network. When a branch's remote branch was deleted, its commits count as unpushed, and the details say it was probably merged.
- `ccm` checks Git again each time you open the view, then every 20 seconds while it's open.

The keys work like the conversation list: Space marks, `a` marks everything, Esc clears the marks and then goes back, and `d` or Del removes the selected or marked worktrees after a popup. Confirm with `y` or Del, or click Remove twice.

Removing runs `git worktree remove`, never with `--force`. The branch stays, so its commits are kept. Everything is checked again just before each removal, and a worktree is refused if:

- it has uncommitted changes
- a commit is checked out directly and some of its commits are on no branch, because nothing would keep them
- a conversation is open in it, or an open one was last working in it
- a conversation that worked in it was written to in the last minute, or runs in the background
- a background job may own it, which `claude rm` removes instead
- it's locked or has submodules, which Git won't remove either
- Git couldn't read its state, or a running session's registry file can't be read

Unpushed commits don't stop a removal, because the branch keeps them. They are only on your machine, though, so the popup warns about them in red. **Remove stays locked for 5 seconds**, with a countdown, and pressing `y`, Del or Remove before it ends starts it over. If more commits appear after you confirm, that worktree is skipped.

The popup lists the conversations that worked in each worktree. It also warns when a conversation started there, because resuming that conversation later recreates the folder empty. **Files Git ignores, such as `.env` or build output, are deleted with the worktree.** The popup lists them with their size, and if new ones appear after you confirm, that worktree is skipped.

## Configuration

Everything is optional. The file is `~/.config/ccm/config.toml`:

```toml
# What counts as large for the l filter, in MB. The default is 100.
large_threshold_mb = 200

# The command that opens a terminal. "claude --resume <id>" is added to the end,
# and it runs in the conversation's folder. The default is xdg-terminal-exec.
terminal = ["foot", "-e"]

# Context window sizes by model, for models that show "?".
[windows]
claude-opus-4-8 = 200000
```

On macOS, set `terminal` to a terminal that can run a command, such as `["alacritty", "-e"]`. If no terminal can be started, `ccm` shows the command to run yourself.

## What it touches on your machine

`ccm` reads Claude Code's files in `~/.claude`, or in `CLAUDE_CONFIG_DIR` if that's set. It sends nothing over the network.

While the [worktree view](#worktrees) is open, it runs Git in the repositories your conversations worked in. While [grouping by repository](#grouping), it asks Git which repository each folder belongs to. Only `git worktree remove` changes anything, and only after you confirm. It ignores `GIT_*` variables from its own environment, stops a repository's `core.fsmonitor` setting from running a program, takes no optional locks and never fetches.

It writes these files of its own:

| Path | What it holds |
|---|---|
| `~/.cache/ccm/index.db` | The search index, including a copy of conversation text, and the repository of each folder conversations started in. It's owner-only, and safe to delete because `ccm` rebuilds it |
| `~/.config/ccm/config.toml` | Your configuration, if you create one. `ccm` only reads it |
| `~/.local/state/ccm/sessions.jsonl` | When sessions start and end, as IDs and times only |
| `~/.local/state/ccm/forks.jsonl` | Which conversations are forks `ccm` started, and of which, as IDs and times only |
| `~/.local/state/ccm/windows/` | Each session's context window size |
| `~/.local/state/ccm/backups/` | Your original settings files, from before `ccm setup` changed them |

### What `ccm setup` changes

- `settings.json` gets a `SessionStart` and a `SessionEnd` hook that record when sessions start and end.
- `statusline.sh` gets two lines, a comment and a command, that save the context window size. If you don't use a `statusline.sh`, or `ccm` can't safely edit yours, it tells you what to add instead.

### What a delete removes

**A delete can't be undone.** `ccm` refuses to delete a conversation that's open, running as a background job, or written to in the last minute. It also stops if it can't tell, such as when a running session's registry file can't be read.

| | |
|---|---|
| **Removed** | The transcript and its folder of subagents and tool results, its file history, session environment, tasks, debug log and telemetry, its search index entry, and its finished background job's files |
| **Edited** | `history.jsonl` loses that conversation's lines, using Claude Code's own lock. A pasted-content file goes only if nothing else uses it |
| **Kept unless you press `m`** | Memory notes that only this conversation wrote |
| **Always kept** | Other conversations, including forks and continuations, plus memory notes edited by hand, `plans/`, the folder's trust setting, and a background job that may have a worktree, which `ccm` tells you to remove with `claude rm` |

## Crash recovery

Claude Code records a clean exit, or a closed terminal, but it can't record a power loss or a hard kill. With the hooks installed, `ccm` shows a session that started and never ended as crashed, in red. Press `Shift+O` to choose which ones to reopen. Any you untick stop showing as crashed.

Only sessions started after `ccm setup` are tracked.

## Platforms

| Platform | Status |
|---|---|
| Linux | Built and tested. Used in `foot`, GNOME Terminal and `herdr-app`, and it should work in most modern terminals |
| macOS | Built and tested on Intel and Apple Silicon by automated checks, but not yet used by hand |
| Windows | Not supported |

Tested with Claude Code 2.1.318. Claude Code's file format isn't documented and can change, so `ccm` skips anything it doesn't recognise and warns you rather than crashing.

## Security

To report a problem privately, see [SECURITY.md](SECURITY.md).

## Uninstall

1. Remove the `ccm` hook entries from `~/.claude/settings.json`, and the two lines mentioning `ccm` from `~/.claude/statusline.sh`.
2. Delete the program and its files:

```sh
rm ~/.local/bin/ccm
rm -rf ~/.cache/ccm ~/.config/ccm ~/.local/state/ccm
```

The last folder holds the backups of your settings, so copy them first if you want them. Your conversations aren't touched.

## License

[MIT](LICENSE)
