# airbag

**Undo anything your AI coding agent did to your disk. Bash commands included.**

YOLO mode, with an airbag.

![airbag demo: an agent deletes src and .env, airbag undo brings them back](docs/demo.gif)

Claude Code's `/rewind` only restores edits made by its own file tools. It does not track
`rm`, `mv`, `cp`, `git clean`, `sed -i` or anything else that runs through Bash, and it
usually does not restore edits made by subagents ([Claude Code docs](https://code.claude.com/docs/en/checkpointing#limitations)).
Git cannot help either: it never saw the files you had not committed yet, and it ignores
`.env` and everything else in `.gitignore` on purpose.

airbag takes a copy-on-write snapshot of your whole project, `.git` and ignored files
included, right before your agent touches it. When something goes wrong:

```
airbag undo
```

## Install

On an Apple silicon Mac:

```
curl -fsSL https://github.com/wienerlabs/airbag/releases/latest/download/airbag-aarch64-apple-darwin.tar.gz | tar xz
sudo mv airbag-aarch64-apple-darwin/airbag /usr/local/bin/
airbag install claude
```

Other builds on the [releases page](https://github.com/wienerlabs/airbag/releases/latest):
`x86_64-apple-darwin` (Intel Mac), `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`
(any Linux). Each archive has a `.sha256` file and a build provenance attestation you can check
with `gh attestation verify airbag-*.tar.gz -R wienerlabs/airbag`. With Rust 1.89 or newer you can
build it yourself: `cargo install --git https://github.com/wienerlabs/airbag`.

`airbag install claude` adds one hook to `~/.claude/settings.json`. Restart open Claude Code
sessions and every Bash, Edit, Write, MultiEdit and NotebookEdit call in a git project is
protected, including calls made by subagents. `airbag uninstall claude` removes it.

### Other agents

| Agent | Install | Protects | Status |
| --- | --- | --- | --- |
| Claude Code | `airbag install claude` | Bash, Edit, Write, MultiEdit, NotebookEdit | tested end to end with Claude Code 2.1.299 |
| Codex CLI | `airbag install codex`, then trust it once in `/hooks` | shell commands and `apply_patch` | built from the Codex source and docs (0.124.0 or newer), not yet run end to end |

`--project` writes to the project's own settings instead of your user settings.

## What it looks like

```
$ airbag log
#3     12s ago  -2           claude Bash   rm -rf src .env
#2      4m ago  ~1           claude Edit   /Users/me/app/src/main.rs
#1      9m ago  +1 ~2        you    snap   before the refactor

$ airbag diff
since #3 (before claude ran Bash: rm -rf src .env):
  deleted  .env
  deleted  src/main.rs
run `airbag undo` to revert this

$ airbag undo
restored #3 (before claude ran Bash: rm -rf src .env)
  2 files written, 0 removed
changed your mind? run `airbag redo`
```

## Commands

| Command | What it does |
| --- | --- |
| `airbag undo` | Put the project back to how it was before the last change. Run it again to step back further. |
| `airbag undo 12` | Go back to snapshot #12 from the log. |
| `airbag undo --path .env` | Restore one file or folder, leave everything else alone. |
| `airbag redo` | Revert the last undo. Every undo saves the state it replaces first. |
| `airbag diff` | Preview what `undo` would revert. `airbag diff 12` compares with #12. |
| `airbag log` | Every snapshot, the agent action that followed it, and what that action changed. |
| `airbag snap -m "note"` | Take a snapshot yourself, for example before you start a big prompt. |
| `airbag install claude` | Hook into Claude Code, or `codex` for Codex CLI. |
| `airbag gc` | Delete stored copies that no snapshot needs any more. Safe to run any time. |

## How it works

- **Before every tool call** Claude Code runs `airbag hook claude`, which snapshots the git
  work tree of the folder the agent is working in. Edits protect the project of the file being
  edited. Folders outside a git work tree are left alone.
- **The whole tree**, including `.git` (so `rm -rf .git` and `git reset --hard` are
  recoverable) and files git ignores (`.env`, local databases, uncommitted work). Dependency
  and cache folders such as `node_modules`, `target` and `.venv` are skipped because they can
  be reinstalled.
- **Content-addressed** like git: files and folders are stored once under `~/.airbag`, keyed by
  their blake3 hash, so a thousand snapshots of a mostly unchanged project cost almost nothing.
- **Copy-on-write**: on APFS (every modern Mac) and on btrfs or XFS, a stored copy is a clone
  that shares blocks with your file until one of them changes. Elsewhere airbag copies files up
  to 1 GB and hard links git's object files, which git never rewrites.
- **Fast**: a stat cache skips files that did not change, and a command that changes nothing
  adds no snapshot. On a MacBook a 813-file, 85 MB repo takes 1.06 s for the first snapshot
  and about 0.1 s after that, process start included.

## Safety rules for undo

Undo is the part of an undo tool that can lose data, so it follows strict rules:

1. Before it changes anything, it saves the current state as a snapshot under a lock. That is
   why every undo can be reverted with `airbag redo`.
2. It checks that every stored copy it needs exists before touching the disk.
3. It only deletes files that appear in a snapshot. Folders it skips (`node_modules`) and files
   it could not store stay where they are. A file it does not know about that stands in the
   way is moved aside as `name.airbag-kept-*`, never overwritten.
4. It writes every file to a temporary name in the same folder and renames it into place.
   Restored files get a fresh modification time so your build tools rebuild them.

## What it does not cover

- Anything outside the git work tree, such as a command that deletes `~/Documents`. Use your
  agent's sandbox for that.
- Things that leave your disk: `git push --force`, databases, deploys, API calls.
- Folders skipped on purpose (`node_modules`, `target`, `.venv` and friends), unless you
  change `AIRBAG_EXCLUDE`.
- Windows. macOS and Linux only for now.
- Agents without a hook into airbag yet. Gemini CLI, Cursor and OpenCode adapters are next.

## Settings

| Variable | Default | Meaning |
| --- | --- | --- |
| `AIRBAG_HOME` | `~/.airbag` | Where snapshots live. |
| `AIRBAG_EXCLUDE` | `node_modules,target,.venv,...` | Folder and file names to skip anywhere in the tree. Setting it replaces the list. |
| `AIRBAG_MAX_FILE_BYTES` | 1 GiB | Biggest file copied on a disk without copy-on-write. Clones have no limit. |
| `AIRBAG_MAX_FILES` | 200000 | Refuse to snapshot projects bigger than this. |
| `AIRBAG_KEEP` | 500 | Snapshots kept per project. |

## FAQ

**Does it slow my agent down?** A snapshot of an unchanged project takes tens of
milliseconds. The first one in a project reads every file once.

**How much disk does it use?** On APFS, btrfs and XFS almost none until files change, and then
only the old versions of the changed files. Elsewhere, one copy of each version of each file.
Snapshots past `AIRBAG_KEEP` are dropped on their own, and `airbag gc` frees the copies only
they used.

**Does anything leave my machine?** No. airbag has no network code. Everything stays in
`~/.airbag`.

**Why not just commit more often?** Because you will not commit before every command your
agent runs, and git ignores `.env` and anything else in `.gitignore` on purpose.

## Development

`cargo test` runs the suite; CI runs it on macOS and Linux. `vhs docs/demo/demo.tape`
records `docs/demo.gif` again from a real Claude Code session (needs `claude` and
[vhs](https://github.com/charmbracelet/vhs), and runs from the repo root after
`cargo build --release`).

## License

MIT. Made by [Wiener Labs](https://wienerlabs.xyz).
