# foreman

[![CI](https://github.com/GbaGuy/foreman/actions/workflows/ci.yml/badge.svg)](https://github.com/GbaGuy/foreman/actions/workflows/ci.yml)

A terminal UI, written in Rust, where you talk to one AI **manager** and it gets the work done with a team of AI **workers** that it creates, commands and keeps track of.

- You only see and talk to the manager.
- The manager is Claude. It doesn't do the work itself: for each job it creates workers, sends each one its orders (a role, a task, and the files it owns), and runs them in parallel, or in steps when one depends on another (a tester after a builder, for example).
- Workers run in parallel without stepping on each other. Each one may only create or edit the files the manager gave it, and if two workers in the same step claim the same file, foreman makes them take turns. After every step foreman records which files really changed and who owned them, and flags any change nobody owned.
- Every worker ends with a report to the manager. The manager reviews the reports and foreman's file record, checks the actual files with its read tools, and sends workers new orders to fix anything wrong, up to `max_rounds`.
- For a question or chat that needs no work, it just answers. It remembers the conversation.

```
 ✻ foreman   Manager  ·  Workers 2  │  ⏺ builder  ·  ⏺ tester   ~/projects/calc

 > build a calculator with tests

 ⏺ Read(notes.md)

 ⏺ Round 1 · 2 workers
   ⎿  ☒ builder · Python developer
        Create calc.py with add, sub, mul and div
      ☒ tester · QA
        Write and run tests for calc.py

 ⏺ Read(calc.py)

 ⏺ The goal is met. calc.py has add, sub, mul and div, and the tester ran 5 tests:
     • all 5 pass
     • dividing by zero raises ZeroDivisionError
   ⎿  Done after 1 round

╭──────────────────────────────────────────────────────────────────────────────────────────────╮
│ > Try "build a todo app in todo.py, with tests"                                              │
╰──────────────────────────────────────────────────────────────────────────────────────────────╯
  tab to switch view · ↑↓ to scroll · ctrl+o/w models   claude sonnet · workers nemotron-3.5-lightning-free
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
| `Tab` / `Shift+Tab` | Switches tabs: ★ Manager, ▦ Workers (all workers live), then one tab per worker |
| `Ctrl+S` | Stops the job right away and kills any running workers |
| `Ctrl+O` | Picks the manager's Claude model (opus, sonnet, haiku) |
| `Ctrl+W` | Picks the workers' opencode model, free models first |
| `↑` `↓` / `PgUp` `PgDn` / `Esc` | Scrolls the current tab; `Esc` jumps back to the bottom |
| `←` `→` / `Home` `End` | Moves the cursor in the message box |
| `Ctrl+T` | Switches the colour theme (claude, tokyo-night, catppuccin) |
| `Ctrl+Q` | Quits |

The main view is a **team chat** in Claude Code's style: `you → manager`, `manager → builder` (the orders, the files it owns, and live status: queued, working 12s, done, failed, with tokens used), `builder → manager` (the worker's report), and `manager → you` (the summary). Each worker has its own colour, and foreman notes which files each step changed. An orange `✻ builder, tester working… (40s · ctrl+s to stop)` spinner sits above a plain `>` prompt box.

Tabs switch between the **Team chat**, a **Workers** overview (every worker live, up to six panes), and one tab per worker showing its conversation with the manager and its work log (`⏺ Write(calc.py)`, `⏺ Bash(pytest)`, errors in red). Token usage shows for each worker and, at the bottom right, for the manager and the workers in total (with the cost when the CLI reports one). You can type to the manager from any tab, and `ctrl+t` cycles colour themes (claude, tokyo-night, catppuccin).

Replies are shown with basic Markdown formatting.

## Configure

Settings live in `~/.config/foreman/crew.json`:

```json
{
  "max_rounds": 3,
  "manager": "claude -p --output-format json --tools=Read,Grep,Glob {prompt}",
  "worker": "opencode run -m opencode/nemotron-3.5-lightning-free --auto --format json {prompt}",
  "max_parallel": 4,
  "worker_timeout_mins": 20,
  "worker_idle_mins": 5
}
```

- `manager` runs Claude with read-only tools, so it can look at the project to plan and check work but can't edit files or run commands. `--output-format json` is how foreman reads its token usage.
- `worker` is the command every worker runs. Workers need permission to act, which is what `--auto` gives opencode; `--format json` lets foreman show each action and count tokens. Any other agent CLI works too (its output is shown as plain text, without token counts), for example `claude -p --permission-mode acceptEdits {prompt}` or `codex exec {prompt}`.
- `max_rounds` caps how many plan, work, review rounds one message can take.
- `max_parallel` caps how many workers run at once; the rest wait their turn.
- `worker_timeout_mins` kills a worker that runs longer than this.
- `worker_idle_mins` kills a worker that prints nothing for this long. Some agent CLIs finish their work and then never exit; this frees the job, and the manager still gets everything the worker printed.

Commands run without a shell, and the prompt is passed as a single argument.

## Develop

```sh
cargo test
```
