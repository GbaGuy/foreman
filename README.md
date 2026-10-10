# foreman

[![CI](https://github.com/GbaGuy/foreman/actions/workflows/ci.yml/badge.svg)](https://github.com/GbaGuy/foreman/actions/workflows/ci.yml)

A terminal UI, written in Rust, where you talk to one AI **manager** and it gets the work done with a team of AI **workers** that it creates as needed.

- You only see and talk to the manager.
- The manager is Claude. It doesn't do the work itself: for each job it creates workers, gives each a name, a role and a task, and runs them in parallel, or in steps when one depends on another (a tester after a builder, for example).
- After each round it reviews the work. It reads every worker's output and reasoning, then checks the actual files with its read tools instead of trusting what workers claim. If something is wrong or missing, it creates workers to fix it in another round, up to `max_rounds`.
- You see everything live: the manager thinking, the files it reads, and its reply as it writes, plus a pane per worker showing its reasoning and every action.
- For a question or chat that needs no work, it just answers. It remembers the conversation.

```
 ⚒ foreman   ★ Manager  ▦ Workers 2  │ ◑ builder  ○ tester                                ◑ working
 ~/projects/calc                         manager claude sonnet · workers nemotron-3.5-lightning-free
╭ ◑ builder · Python developer ────────────── 2s ╮╭ ○ tester · QA ─────────────── waits for step 2 ╮
│ > build · nemotron-3.5-lightning-free          ││ queued: starts when the earlier steps finish   │
│ Thinking: I should read notes.md first.        ││                                                │
│ → Read notes.md                                ││                                                │
│ ← Write calc.py                                ││                                                │
│ Created file successfully: calc.py             ││                                                │
│                                                ││                                                │
│                                                ││                                                │
│                                                ││                                                │
│                                                ││                                                │
│                                                ││                                                │
│                                                ││                                                │
│                                                ││                                                │
│                                                ││                                                │
│                                                ││                                                │
╰────────────────────────────────────────────────╯╰────────────────────────────────────────────────╯
╭ ❯ to manager ────────────────────────────────────────────────────────────────────────────────────╮
│The manager is working… (Ctrl+S stops it)                                                         │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯
 Tab next tab  ^S stop  ^O manager model  ^W worker model  ^T theme  ^Q quit
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
| `Ctrl+T` | Switches the colour theme (tokyo-night, catppuccin, gruvbox) |
| `Ctrl+Q` | Quits |

foreman is tab based. **★ Manager** is your chat: your messages (❯), the manager's replies, a divider per round, and each task it hands out with its step, worker and role. **▦ Workers** shows every worker live in a grid (up to six panes), and each worker also gets its own tab with its task on top and its full output below. Workers show their state (○ queued, spinner while working, ● done, ✗ failed) and run time, and their output is coloured: reasoning dimmed, tool actions highlighted, errors in red. You can type to the manager from any tab.

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
