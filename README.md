# foreman

[![CI](https://github.com/GbaGuy/foreman/actions/workflows/ci.yml/badge.svg)](https://github.com/GbaGuy/foreman/actions/workflows/ci.yml)

A terminal UI, written in Rust, where you talk to one AI **manager** and it gets the work done with a team of AI **workers** that it creates as needed.

- You only see and talk to the manager.
- The manager is Claude. It doesn't do the work itself: for each job it creates workers, gives each a name, a role and a task, and runs them in parallel, or in steps when one depends on another (a tester after a builder, for example).
- After each round it reviews the work. It reads every worker's output and reasoning, then checks the actual files with its read tools instead of trusting what workers claim. If something is wrong or missing, it creates workers to fix it in another round, up to `max_rounds`.
- You see everything live: the manager thinking, the files it reads, and its reply as it writes, plus a pane per worker showing its reasoning and every action.
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
| type + `Enter` | Talks to the manager. Pasting multi-line text is safe: it won't send early |
| `Tab` | Opens the workers' live output; `←`/`→` switch workers, `Esc` closes |
| `Ctrl+S` | Stops the job right away and kills any running workers |
| `Ctrl+O` | Picks the manager's Claude model (opus, sonnet, haiku) |
| `Ctrl+W` | Picks the workers' opencode model, free models first |
| `↑` `↓` / `PgUp` `PgDn` / `Esc` | Scrolls the chat; `Esc` jumps back to the bottom |
| `←` `→` / `Home` `End` | Moves the cursor in the message box |
| `Ctrl+T` | Switches the colour theme (tokyo-night, catppuccin, gruvbox) |
| `Ctrl+Q` | Quits |

While a job runs, each worker gets a live pane (up to six on screen) with its state (spinner while working, ● done, ✗ failed), run time, reasoning and actions. `Tab` opens any worker's full output with scrollback.

The manager streams too: `💭 thinking (~N tokens)`, each file it reads or searches (`→ Read notes.md`), then its reply as it's written. Claude Code doesn't hand out the text of Claude's thinking, only that it's thinking and roughly how much; the workers' opencode models do show their full reasoning. Manager replies are shown with basic Markdown formatting.

## Configure

Settings live in `~/.config/foreman/crew.json`:

```json
{
  "max_rounds": 3,
  "manager": "claude -p --output-format stream-json --verbose --include-partial-messages --tools=Read,Grep,Glob {prompt}",
  "worker": "opencode run -m opencode/nemotron-3.5-lightning-free --auto --thinking {prompt}",
  "max_parallel": 4,
  "worker_timeout_mins": 20
}
```

- `manager` runs Claude with read-only tools, so it can look at the project to plan and check work but can't edit files or run commands. `stream-json` is what makes its activity show live; without it you only see the final reply. Keep the `=` in `--tools=...`: without it the flag swallows the prompt.
- `worker` is the command every worker runs. Workers need permission to act, which is what `--auto` gives opencode; `--thinking` makes their reasoning visible. You can use any agent CLI here, for example `claude -p --permission-mode acceptEdits {prompt}` or `codex exec {prompt}`.
- `max_rounds` caps how many plan, work, review rounds one message can take.
- `max_parallel` caps how many workers run at once; the rest wait their turn.
- `worker_timeout_mins` kills a worker that runs longer than this.

Commands run without a shell, and the prompt is passed as a single argument.

## Develop

```sh
cargo test
```
