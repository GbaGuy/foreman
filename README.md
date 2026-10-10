# foreman

[![CI](https://github.com/GbaGuy/foreman/actions/workflows/ci.yml/badge.svg)](https://github.com/GbaGuy/foreman/actions/workflows/ci.yml)

A terminal UI, written in Rust, where you talk to one AI **manager** and it gets the work done with a team of AI **workers** that it creates as needed.

- You only see and talk to the manager.
- The manager is Claude. It doesn't do the work itself: for each job it creates workers, gives each a name, a role and a task, and runs them in parallel, or in steps when one depends on another (a tester after a builder, for example).
- After each round it reviews the work. It reads every worker's output and reasoning, then checks the actual files with its read tools instead of trusting what workers claim. If something is wrong or missing, it creates workers to fix it in another round, up to `max_rounds`.
- You see everything live: the manager thinking, the files it reads, and its reply as it writes, plus a pane per worker showing its reasoning and every action.
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

The look follows Claude Code: a flowing transcript with `>` for your messages, `⏺` for the manager's replies and the files it reads (`⏺ Read(notes.md)`), each round as a to-do list (☐ queued, ◼ working, ☒ done, ✗ failed), and an orange `✻ Planning… (12s · ctrl+s to stop)` spinner above a plain prompt box. Tabs switch between the **Manager** transcript, a **Workers** overview (every worker live, up to six panes) and one tab per worker with its task and full output. Worker output is shown the same way: reasoning as `✻`, actions as `⏺ Write(calc.py)`, errors in red. You can type to the manager from any tab, and `ctrl+t` cycles colour themes (claude, tokyo-night, catppuccin).

The manager streams too: `💭 thinking (~N tokens)`, each file it reads or searches (`→ Read notes.md`), then its reply as it's written. Claude Code doesn't hand out the text of Claude's thinking, only that it's thinking and roughly how much; the workers' opencode models do show their full reasoning. Manager replies are shown with basic Markdown formatting.

## Configure

Settings live in `~/.config/foreman/crew.json`:

```json
{
  "max_rounds": 3,
  "manager": "claude -p --output-format stream-json --verbose --include-partial-messages --tools=Read,Grep,Glob {prompt}",
  "worker": "opencode run -m opencode/nemotron-3.5-lightning-free --auto --thinking {prompt}",
  "max_parallel": 4,
  "worker_timeout_mins": 20,
  "worker_idle_mins": 5
}
```

- `manager` runs Claude with read-only tools, so it can look at the project to plan and check work but can't edit files or run commands. `stream-json` is what makes its activity show live; without it you only see the final reply. Keep the `=` in `--tools=...`: without it the flag swallows the prompt.
- `worker` is the command every worker runs. Workers need permission to act, which is what `--auto` gives opencode; `--thinking` makes their reasoning visible. You can use any agent CLI here, for example `claude -p --permission-mode acceptEdits {prompt}` or `codex exec {prompt}`.
- `max_rounds` caps how many plan, work, review rounds one message can take.
- `max_parallel` caps how many workers run at once; the rest wait their turn.
- `worker_timeout_mins` kills a worker that runs longer than this.
- `worker_idle_mins` kills a worker that prints nothing for this long. Some agent CLIs finish their work and then never exit; this frees the job, and the manager still gets everything the worker printed.

Commands run without a shell, and the prompt is passed as a single argument.

## Develop

```sh
cargo test
```
