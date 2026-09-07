# psst

**p**i **s**ession **s**ort-of-**t**erminal-ui — browse, inspect, and resume
[pi coding-agent](https://github.com/earendil-works/pi-mono) sessions from one
TUI. Built because nothing combined both halves: mining the full session
archive *and* showing which agents are running right now.

```
● tmux-agent-restore-work   tmux-agent…  312m  [agents:3]
○ psst                      psst         2s
⎇ wiki-session-sync         dotfiles     14d
▾ Today  12 sessions · last 2s
▾ ~/Code/github.com/ericboehs/dotfiles  47 sessions · last 2s
```

- `●` green dot = a live pi process is attached (cwd match + recent write;
  exact when the process cmdline carries `--session`)
- `[agents:3]` = the tmux session/window the running agent lives in
- `⎇` = session was forked from a parent (`/fork`, `/clone`)

## Install

```bash
cd ~/Code/github.com/ericboehs/psst
cargo build --release
# Install via rename (NOT cp over the old file): overwriting the binary in
# place leaves macOS's taskgated signature cache stale and the new binary
# dies with "zsh: killed" (SIGKILL, Taskgated Invalid Signature). Re-sign
# after install for the same reason.
cp target/release/psst /tmp/psst.new && mv -f /tmp/psst.new ~/.local/bin/psst
codesign --force --sign - ~/.local/bin/psst
```

Rust via Homebrew is the only dependency. tmux is required for opening sessions.

## Usage

```bash
psst                  # TUI
psst --list           # plain table (pipe-friendly)
psst --refresh        # rebuild the metadata cache from scratch
psst --debug-running  # print detected pi processes + tmux targets
```

### Keys

| Key | Action |
|-----|--------|
| `/` | focus search (then type anything, including `j`/`k`; Esc unfocuses). Running sessions also match `active`. |
| `f` | cycle list filter: all → named → tmux |
| `g` or `Tab` | cycle grouping: flat → project → day |
| `s` or `Ctrl+S` | cycle sort: activity → tmux |
| `a` | toggle active-only (sessions with a live pi process) |
| `↑`/`↓` or `k`/`j`, PgUp/PgDn | navigate (j/k only when search is not focused) |
| `Enter` | **open**: attach to the running tmux session, or spawn `pi --session <file>` in a new tmux window |
| `d` | toggle the bottom detail pane (off by default) |
| `p` | hide/show prompt text in list titles |
| `r` / `R` | rescan (cache-aware) / full rescan |
| `Esc` | clear filter (quit if empty) |
| `q` | quit |

The list auto-refreshes every 10s.

Group headers (project and day modes) show session count, total cost, and the
newest activity in the group. Day buckets use local dates; within a group,
rows keep the active sort's order.

## How it works

- **Sessions**: JSONL trees under `~/.pi/agent/sessions/<encoded-cwd>/*.jsonl`
  (format v3, see pi's `session-format.md`). psst reads the header for the
  project cwd, then scans for `session_info` names, first user prompt, models,
  token/cost usage (`usage.cost.total`, including nested toolResult usage),
  and branch points (entries with >1 children). Respects `PI_CODING_AGENT_DIR`.
- **Running detection** (zero-config, per design): pi appends via
  `open/write/close`, so `lsof` only catches files mid-write. Instead psst
  matches pi-like processes (via `ps`), resolves their tmux pane from the
  inherited `TMUX_PANE` env var (falling back to a parent-chain walk to
  `pane_pid`), and pins a session file: exactly when the cmdline carries
  `--session`; the newest session in the process cwd when it carries
  `-c`/`--continue` (even if idle); otherwise the newest file written in that
  cwd within the active window (default 120s). Parallel agents in one project
  claim distinct files (newest-first), so each maps to its own tmux window.
- **Open**: running sessions switch/attach to their tmux target (window, not
  just session); idle sessions spawn a new tmux window (or detached session
  when outside tmux) running the resume command in the session's project cwd.
- **Cache**: parsed metadata is cached at `~/.cache/psst/cache.json`, keyed by
  path+mtime+size, so rescans only reparse changed files.

## Config — `~/.config/psst/config.toml`

```toml
[resume]
# {file} = session jsonl path, {id} = session uuid, {cwd} = project dir
command = 'pi --session {file}'

[detection]
active_secs = 120

[ui]
# grouping on launch: "day" (default), "project", or "flat"
group = "day"
```

If you launch pi through mise or a wrapper, point `command` at it, e.g.
`command = 'mise x -- pi --session {file}'`.

## Non-goals (v1)

No editing/deleting sessions, no forking, no cross-machine sync, no analytics
dashboards. psst lists, inspects, and opens — session mutation stays in pi.

## Status

Prototype. Parser is unit-tested against the documented v3 format; running
detection is heuristic and tuned for macOS + tmux 3.x (pi launched from a
shell pane).