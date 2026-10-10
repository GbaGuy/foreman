# foreman

A terminal UI, written in Rust, for running a crew of AI agents. You only talk to the **manager**. It doesn't do the work itself: it gives each **worker** a role and a task, runs them (in parallel, or in steps when one depends on another), and then reviews what they did. If the goal isn't met yet, the manager assigns another round, up to `max_rounds`. Questions and chat that need no work, the manager answers itself, and it remembers the conversation.

By default the manager runs on a free [opencode](https://opencode.ai) model in opencode's read-only `plan` agent, so it can't edit files even if it tries. Press `Ctrl+O` to pick any opencode model, free or local, for the manager or any worker.

```
 ⚒ foreman   ~/Projects                                                                     ○ idle
╭ crew ──────────────────────────╮╭ output ────────────────────────────────────────────────────────╮
│▌● manager ★                    ││ ● manager │ ● coder │ ○ reviewer │ ○ tester                    │
│   Engineering lead             ││                                                                │
│   sh                           ││                                                                │
│                                ││▶ You manage a team of AI agents working in the current         │
│ ● coder                        ││directory.                                                      │
│   Writer                       ││planning...                                                     │
│   sh                           ││[{"worker": "coder", "role": "Writer", "task": "write a poem"}] │
│                                ││Round 1 assignments                                             │
│ ○ reviewer                     ││  step 1  coder as Writer: write a poem                         │
│   Reviews work                 ││                                                                │
│   echo                         ││▶ Goal: write a poem about the sea                              │
│                                ││looks good, goal met                                            │
│                                ││Done after 1 round(s).                                          │
│                                ││                                                                │
│                                ││                                                                │
╰────────────────────────────────╯╰────────────────────────────────────────────────────────────────╯
╭ to manager ──────────────────────────────────────────────────────────────────────────────────────╮
│Tell the manager what you need…                                                                   │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯
 ^N add  ^R remove  ^O model  ^S stop  ^T theme  Tab switch  PgUp/Dn scroll  ^Q quit            Crew finished
```

Each agent is a CLI command with `{prompt}` in it, so any agent CLI works: `claude`, `codex`, `opencode`, or your own script. You can mix them, for example a Claude manager running Codex coders.

## Install

```sh
cargo install --git https://github.com/GbaGuy/foreman
# or from a clone: cargo run --release
```

You need `opencode` for the manager (or point it at another CLI in the config), plus whatever CLIs your workers use.

## Use

Run `foreman` in the directory you want the crew to work in.

| Key / input | What it does |
|---|---|
| type a message + `Enter` | Talks to the manager: it answers, or plans, runs the workers, reviews and re-plans until done |
| `Ctrl+O` | Picks an opencode model for the selected agent (free ones first) |
| `Ctrl+S` | Stops once the running agents finish |
| `Ctrl+N` | Adds a worker: name, default role, backend, command |
| `Ctrl+R` | Removes the selected worker |
| `Tab` / `↑` `↓` | Switches between agents' output tabs |
| `PgUp` / `PgDn` / `End` | Scrolls the output; `End` jumps back to the bottom |
| `Ctrl+T` | Switches the colour theme (tokyo-night, catppuccin, gruvbox) |
| `Ctrl+Q` | Quits |

The sidebar shows each agent's state (○ idle, ◐ working, ● done, ✗ failed) and the role the manager gave it on this run.

## Configure

The crew is saved in `~/.config/foreman/crew.json`. Edit it to change the manager or give agents extra flags:

```json
{
  "max_rounds": 3,
  "manager": {"name": "manager", "command": "opencode run -m opencode/nemotron-3.5-lightning-free --agent plan {prompt}", "role": "Engineering lead"},
  "workers": [
    {"name": "coder", "command": "opencode run -m opencode/nemotron-3.5-lightning-free --auto {prompt}", "role": "Writes and edits code"},
    {"name": "reviewer", "command": "claude -p {prompt}", "role": "Reviews work for bugs"}
  ]
}
```

`max_rounds` (default 3) caps how many plan, work, review rounds one goal can take. Commands run without a shell, and the prompt is passed as a single argument. Each CLI keeps its own permission settings. For example, `opencode run` can't edit files until you add `--auto`, and `claude -p` needs something like `--permission-mode acceptEdits`.

## Develop

```sh
cargo test
```
