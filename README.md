# foreman

A terminal UI, written in Rust, where you talk to one AI **manager** and it gets the work done with a team of AI **workers** that it creates as needed.

- You only see and talk to the manager.
- The manager is Claude. It doesn't do the work itself: for each job it creates workers, gives each a name, a role and a task, and runs them in parallel, or in steps when one depends on another (a tester after a builder, for example).
- It reviews what the workers did and, if the goal isn't met, creates more workers for another round, up to `max_rounds`.
- For a question or chat that needs no work, it just answers. It remembers the conversation.

```
 ⚒ foreman   ~/projects/todo                                                                  ○ idle
╭ ★ manager · claude sonnet ───────────────────────────────────────────────────────────────────────╮
│                                                                                                  │
│ you › Make a tiny todo CLI and test it                                                           │
│ Round 1: 2 worker(s)                                                                             │
│   step 1 · builder (Python developer): Create todo.py with add, list and done                    │
│   step 2 · tester (QA tester): Write and run tests for todo.py                                   │
│ ● builder finished                                                                               │
│ ● tester finished                                                                                │
│ The goal is met. todo.py has add, list and done, and all 5 tests pass.                           │
│ Done after 1 round(s).                                                                           │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯
╭ workers · opencode/nemotron-3.5-lightning-free ──────────────────────────────────────────────────╮
│ ● builder       Python developer                                                                 │
│ ● tester        QA tester                                                                        │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯
╭ to manager ──────────────────────────────────────────────────────────────────────────────────────╮
│Tell the manager what you need…                                                                   │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯
 ^O manager model  ^W worker model  ^S stop  ^T theme  ↑↓/PgUp/PgDn scroll  ^Q quit   Crew finished
```

## Install

```sh
cargo install --git https://github.com/GbaGuy/foreman
# or from a clone: cargo run --release
```

You need `claude` (Claude Code) logged in for the manager, and `opencode` for the workers (or any other agent CLI, see below).

## Use

Run `foreman` in the folder you want the work done in.

| Key | What it does |
|---|---|
| type + `Enter` | Talks to the manager |
| `Ctrl+O` | Picks the manager's Claude model (opus, sonnet, haiku) |
| `Ctrl+W` | Picks the workers' opencode model, free models first |
| `Ctrl+S` | Stops once the running workers finish |
| `↑` `↓` / `PgUp` `PgDn` / `End` | Scrolls the chat; `End` jumps back to the bottom |
| `Ctrl+T` | Switches the colour theme (tokyo-night, catppuccin, gruvbox) |
| `Ctrl+Q` | Quits |

The workers strip shows each worker of the current job with its role and state (spinner while working, ● done, ✗ failed). Their raw output isn't shown; the manager reads it and reports to you.

## Configure

Settings live in `~/.config/foreman/crew.json`:

```json
{
  "max_rounds": 3,
  "manager": "claude -p --tools=Read,Grep,Glob {prompt}",
  "worker": "opencode run -m opencode/nemotron-3.5-lightning-free --auto {prompt}"
}
```

- `manager` runs Claude with read-only tools, so it can look at the project to plan but can't edit files or run commands. Keep the `=` in `--tools=...`: without it the flag swallows the prompt.
- `worker` is the command every worker runs. Workers need permission to act, which is what `--auto` gives opencode. You can use any agent CLI here, for example `claude -p --permission-mode acceptEdits {prompt}` or `codex exec {prompt}`.
- `max_rounds` caps how many plan, work, review rounds one message can take.

Commands run without a shell, and the prompt is passed as a single argument.

## Develop

```sh
cargo test
```
