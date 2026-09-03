# psst

**p**i **s**ession **s**ort-of-**t**erminal-ui — browse, inspect, and resume
[pi coding-agent](https://github.com/earendil-works/pi-mono) sessions from one
TUI. Built because nothing combined both halves: mining the full session
archive *and* showing which agents are running right now.

```
● tmux-agent-restore-work   tmux-agent…  claude-son…  $0.842   312m  [agents:3]
○ psst                      psst         -            $0       2s
⎇ wiki-session-sync         dotfiles     gpt-5        $0.104   14d
▾ Today  12 sessions · $3.42 · last 2s
▾ ~/Code/github.com/ericboehs/dotfiles  47 sessions · $12.31 · last 2s
```

- `●` green dot = a live pi process is attached (cwd match + recent write;
  exact when the process cmdline carries `--session`)
- `[agents:3]` = the tmux session/window the running agent lives in
- `⎇` = session was forked from a parent (`/fork`, `/clone`)

## Install

```bash
cd ~/Code/github.com/ericboehs/psst
cargo build --release
cp target/release/psst ~/.local/bin/
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
| type | filter (all terms must match across title/prompt/project/model) |
| `↑`/`↓` or `k`/`j`, PgUp/PgDn | navigate |
| `Enter` | **open**: attach to the running tmux session, or spawn `pi --session <file>` in a new tmux window |
| `Tab` | cycle grouping: flat → project → day (newest day first; "Today"/"Yesterday"/date headers) |
| `Ctrl+S` | cycle sort: activity → tmux → cost |
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
  matches pi-like processes (via `ps`), resolves their cwd (`lsof`), finds
  their tmux pane by walking the parent chain to a `pane_pid`, and pins the
  newest session file written in that cwd within the active window
  (default 120s). A `--session` on the cmdline pins exactly.
- **Open**: running sessions switch/attach to their tmux target; idle sessions
  spawn a new tmux window (or detached session when outside tmux) running the
  resume command in the session's project cwd.
- **Cache**: parsed metadata is cached at `~/.cache/psst/cache.json`, keyed by
  path+mtime+size, so rescans only reparse changed files.

## Config — `~/.config/psst/config.toml`

```toml
[resume]
# {file} = session jsonl path, {id} = session uuid, {cwd} = project dir
command = 'pi --session {file}'

[detection]
active_secs = 120
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