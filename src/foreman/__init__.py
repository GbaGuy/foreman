"""foreman: a terminal boss for a crew of AI agents.

A manager agent reads your goal, gives each worker a role and a task, the
workers run in parallel, then the manager reviews their output. Every agent is
just a CLI command (claude, codex, opencode, ...) with {prompt} in it.
"""

import asyncio
import json
import re
import shlex
from pathlib import Path

from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical
from textual.screen import ModalScreen
from textual.widgets import (
    Button, Footer, Header, Input, Label, ListItem, ListView, RichLog,
    Select, TabbedContent, TabPane,
)

CONFIG = Path.home() / ".config" / "foreman" / "crew.json"
BACKENDS = {
    "claude": "claude -p {prompt}",
    "codex": "codex exec {prompt}",
    "opencode": "opencode run {prompt}",
}
DEFAULT_CREW = {
    "manager": {"name": "manager", "command": BACKENDS["claude"], "role": "Engineering lead"},
    "workers": [
        {"name": "coder", "command": BACKENDS["claude"], "role": "Writes and edits code"},
        {"name": "reviewer", "command": BACKENDS["claude"], "role": "Reviews work for bugs and gaps"},
    ],
}
PLAN_FORMAT = '[{"worker": "<name>", "role": "<role>", "task": "<task>", "step": 1}]'
NAME_RE = re.compile(r"^[a-z][a-z0-9_-]{0,23}$")
ICONS = {"idle": "[dim]○[/]", "working": "[yellow]◐[/]", "done": "[green]●[/]", "failed": "[red]✗[/]"}


def load_crew() -> dict:
    if CONFIG.exists():
        return json.loads(CONFIG.read_text())
    return json.loads(json.dumps(DEFAULT_CREW))


def save_crew(crew: dict) -> None:
    CONFIG.parent.mkdir(parents=True, exist_ok=True)
    CONFIG.write_text(json.dumps(crew, indent=2))


def build_argv(command: str, prompt: str) -> list[str]:
    """Split the template and drop the prompt in as one argv item (no shell)."""
    argv = [prompt if part == "{prompt}" else part for part in shlex.split(command)]
    return argv if prompt in argv else argv + [prompt]


def manager_prompt(goal: str, workers: list[dict]) -> str:
    crew = "\n".join(f"- {w['name']}: {w.get('role') or 'general agent'}" for w in workers)
    return (
        "You manage a team of AI agents working in the current directory.\n"
        f"Goal: {goal}\n\nWorkers:\n{crew}\n\n"
        "Pick the workers you need, give each a specific role for this goal and a "
        "concrete task. Tasks with the same step run in parallel; a later step sees "
        "the output of earlier steps (e.g. review after writing). "
        f"Reply with ONLY a JSON array, no prose:\n{PLAN_FORMAT}"
    )


def review_prompt(goal: str, report: str, last_round: bool) -> str:
    ask = ("This was the last round: give a final summary of what got done and what is left."
           if last_round else
           "If the goal is fully met, say so and include no JSON. Otherwise end your reply with "
           f"a JSON array assigning the next round of work, in this format:\n{PLAN_FORMAT}")
    return (f"Goal: {goal}\n\nYour workers reported:\n{report}\n\n"
            f"Review their work: what got done and what is missing. {ask}")


def parse_plan(text: str, names: set[str]) -> list[dict]:
    """Pull the JSON array ending the manager's reply; keep known workers, default step 1."""
    end = text.rfind("]")
    # the outermost array is the first "[" that parses through to the final "]"
    for start in (i for i, c in enumerate(text[:end]) if c == "["):
        try:
            plan = json.loads(text[start:end + 1])
            break
        except json.JSONDecodeError:
            continue
    else:
        return []
    if not isinstance(plan, list):
        return []
    plan = [p for p in plan if isinstance(p, dict) and p.get("worker") in names and p.get("task")]
    for p in plan:
        p["step"] = p["step"] if isinstance(p.get("step"), int) else 1
    return plan


class AgentForm(ModalScreen[dict | None]):
    BINDINGS = [Binding("escape", "dismiss(None)", "Cancel")]

    def compose(self) -> ComposeResult:
        with Vertical(id="form"):
            yield Label("[b]New worker[/]")
            yield Input(placeholder="name (e.g. tester)", id="name")
            yield Input(placeholder="role (e.g. Writes pytest tests)", id="role")
            yield Select([(k, k) for k in BACKENDS], value="claude", allow_blank=False, id="backend")
            yield Input(BACKENDS["claude"], placeholder="command with {prompt}", id="command")
            yield Label("", id="error")
            with Horizontal(id="buttons"):
                yield Button("Add", variant="primary", id="add")
                yield Button("Cancel", id="cancel")

    def on_select_changed(self, event: Select.Changed) -> None:
        self.query_one("#command", Input).value = BACKENDS[event.value]

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "cancel":
            return self.dismiss(None)
        name = self.query_one("#name", Input).value.strip().lower()
        command = self.query_one("#command", Input).value.strip()
        error = self.query_one("#error", Label)
        if not NAME_RE.match(name) or name in self.app.agents:
            return error.update("[red]name: unique, lowercase, letters/digits/-/_[/]")
        if not command:
            return error.update("[red]command is required[/]")
        self.dismiss({"name": name, "role": self.query_one("#role", Input).value.strip(), "command": command})


class Foreman(App):
    TITLE = "foreman"
    CSS = """
    #sidebar { width: 34; border: round $primary; border-title-color: $accent; padding: 0 1; }
    #sidebar ListView { background: transparent; }
    #main { border: round $secondary; border-title-color: $accent; }
    RichLog { background: $surface; padding: 0 1; }
    #goal { margin: 0 1; border: tall $accent; }
    AgentForm { align: center middle; }
    #form { width: 64; height: auto; padding: 1 2; border: thick $primary; background: $panel; }
    #form > * { margin-bottom: 1; }
    #buttons { height: auto; align-horizontal: right; }
    #buttons Button { margin-left: 1; }
    """
    BINDINGS = [
        Binding("ctrl+n", "add_worker", "Add worker"),
        Binding("ctrl+r", "remove_worker", "Remove worker"),
        Binding("ctrl+s", "stop", "Stop"),
        Binding("ctrl+t", "next_theme", "Theme"),
        Binding("ctrl+q", "quit", "Quit"),
    ]

    def __init__(self) -> None:
        super().__init__()
        self.crew = load_crew()
        self.agents = {a["name"]: a for a in [self.crew["manager"], *self.crew["workers"]]}
        self.roles: dict[str, str] = {}  # roles the manager handed out this run
        self.busy = False
        self.stopping = False

    # ---- layout -------------------------------------------------------------
    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        with Horizontal():
            sidebar = Vertical(id="sidebar")
            sidebar.border_title = "crew"
            with sidebar:
                yield ListView(*[self._item(a) for a in self.agents.values()], id="crew")
            main = TabbedContent(id="main")
            main.border_title = "output"
            with main:
                for a in self.agents.values():
                    yield self._pane(a)
        yield Input(placeholder="Goal for the manager…  (or @worker message to talk to one directly)", id="goal")
        yield Footer()

    def on_mount(self) -> None:
        self.theme = "tokyo-night"
        self.sub_title = str(Path.cwd())
        self.query_one("#goal").focus()
        self.log_to("manager", "[b]Ready.[/] Type a goal below. Workers get roles and tasks from me.")

    def _item(self, a: dict) -> ListItem:
        return ListItem(Label(self._label(a, "idle"), id=f"status-{a['name']}"), id=f"item-{a['name']}")

    def _label(self, a: dict, state: str) -> str:
        boss = " [b magenta]★[/]" if a is self.crew["manager"] else ""
        return f"{ICONS[state]} [b]{a['name']}[/]{boss}\n  [dim]{self.roles.get(a['name']) or a.get('role') or '—'}[/]\n  [dim italic]{a['command'].split()[0]}[/]"

    def _pane(self, a: dict) -> TabPane:
        return TabPane(a["name"], RichLog(markup=True, wrap=True, id=f"log-{a['name']}"), id=f"tab-{a['name']}")

    def set_state(self, name: str, state: str) -> None:
        self.query_one(f"#status-{name}", Label).update(self._label(self.agents[name], state))

    def log_to(self, name: str, text: str) -> None:
        self.query_one(f"#log-{name}", RichLog).write(text)

    # ---- crew management ----------------------------------------------------
    def action_add_worker(self) -> None:
        def added(worker: dict | None) -> None:
            if not worker:
                return
            self.crew["workers"].append(worker)
            self.agents[worker["name"]] = worker
            save_crew(self.crew)
            self.query_one("#crew", ListView).append(self._item(worker))
            self.query_one("#main", TabbedContent).add_pane(self._pane(worker))
            self.notify(f"Hired {worker['name']}")
        self.push_screen(AgentForm(), added)

    def action_remove_worker(self) -> None:
        item = self.query_one("#crew", ListView).highlighted_child
        name = item.id.removeprefix("item-") if item else None
        if not name or self.agents[name] is self.crew["manager"] or self.busy:
            return self.notify("Select a worker (not the manager) while idle", severity="warning")
        self.crew["workers"] = [w for w in self.crew["workers"] if w["name"] != name]
        del self.agents[name]
        save_crew(self.crew)
        item.remove()
        self.query_one("#main", TabbedContent).remove_pane(f"tab-{name}")
        self.notify(f"Removed {name}")

    def action_stop(self) -> None:
        if self.busy:
            self.stopping = True
            self.notify("Stopping after the running agents finish", severity="warning")

    def action_next_theme(self) -> None:
        themes = list(self.available_themes)
        self.theme = themes[(themes.index(self.theme) + 1) % len(themes)]
        self.notify(self.theme)

    def on_list_view_selected(self, event: ListView.Selected) -> None:
        self.query_one("#main", TabbedContent).active = f"tab-{event.item.id.removeprefix('item-')}"

    # ---- running agents -----------------------------------------------------
    async def run_agent(self, name: str, prompt: str) -> str:
        """Run one agent's CLI, stream its output to its tab, return all output."""
        self.set_state(name, "working")
        self.log_to(name, f"\n[b cyan]▶ {prompt.splitlines()[0][:200]}[/]")
        lines = []
        try:
            proc = await asyncio.create_subprocess_exec(
                *build_argv(self.agents[name]["command"], prompt),
                stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT,
            )
            async for raw in proc.stdout:
                line = raw.decode(errors="replace").rstrip()
                lines.append(line)
                self.query_one(f"#log-{name}", RichLog).write(line)
            ok = await proc.wait() == 0
        except OSError as e:
            lines.append(f"error: {e}")
            self.log_to(name, f"[red]{e}[/]")
            ok = False
        self.set_state(name, "done" if ok else "failed")
        return "\n".join(lines)

    async def on_input_submitted(self, event: Input.Submitted) -> None:
        text = event.value.strip()
        if not text:
            return
        if self.busy:
            return self.notify("The crew is still working", severity="warning")
        event.input.value = ""
        self.busy = True
        self.run_worker(self.direct(text) if text.startswith("@") else self.orchestrate(text), exclusive=True)

    async def direct(self, text: str) -> None:
        name, _, msg = text[1:].partition(" ")
        try:
            if name not in self.agents or not msg:
                return self.notify(f"Usage: @<agent> message — agents: {', '.join(self.agents)}", severity="error")
            self.query_one("#main", TabbedContent).active = f"tab-{name}"
            await self.run_agent(name, msg)
        finally:
            self.busy = False

    async def run_plan(self, goal: str, plan: list[dict], report: str) -> str:
        """Run one round step by step; return the report with this round's output added."""
        for step in sorted({p["step"] for p in plan}):
            if self.stopping:
                break
            batch = [p for p in plan if p["step"] == step]
            # ponytail: only the last 12k chars of history go to workers; summarise rounds if that loses context
            earlier = f"\n\nWork done so far by the team:\n{report[-12000:]}" if report else ""
            results = await asyncio.gather(*[
                self.run_agent(p["worker"], f"Your role: {p.get('role', '')}\nTeam goal: {goal}\n\n"
                                            f"Your task: {p['task']}{earlier}")
                for p in batch
            ])
            report += "".join(f"\n## {p['worker']} ({p.get('role', '')})\n{r[-6000:]}\n" for p, r in zip(batch, results))
        return report

    def show_plan(self, boss: str, rnd: int, plan: list[dict]) -> None:
        self.log_to(boss, f"\n[b magenta]Round {rnd} assignments[/]")
        for p in plan:
            self.roles[p["worker"]] = p.get("role", "")
            self.set_state(p["worker"], "idle")
            self.log_to(boss, f"  [dim]step {p['step']}[/] [b]{p['worker']}[/] as [i]{p.get('role', '?')}[/]: {p['task']}")

    async def orchestrate(self, goal: str) -> None:
        boss = self.crew["manager"]["name"]
        names = {w["name"] for w in self.crew["workers"]}
        max_rounds = self.crew.get("max_rounds", 3)
        tabs = self.query_one("#main", TabbedContent)
        self.stopping = False
        try:
            tabs.active = f"tab-{boss}"
            plan = parse_plan(await self.run_agent(boss, manager_prompt(goal, self.crew["workers"])), names)
            if not plan:
                return self.log_to(boss, "[red]No usable plan in the manager's reply.[/]")
            report, rnd = "", 1
            while plan:
                self.show_plan(boss, rnd, plan)
                report = await self.run_plan(goal, plan, report + f"\n# Round {rnd}\n")
                if self.stopping:
                    return self.log_to(boss, f"\n[yellow]Stopped during round {rnd}.[/]")
                tabs.active = f"tab-{boss}"
                review = await self.run_agent(boss, review_prompt(goal, report[-24000:], rnd >= max_rounds))
                plan = [] if rnd >= max_rounds else parse_plan(review, names)
                rnd += 1
            self.log_to(boss, f"\n[b green]Done after {rnd - 1} round(s).[/]")
            self.notify("Crew finished", title="foreman")
        finally:
            self.busy = False


def main() -> None:
    Foreman().run()


if __name__ == "__main__":
    main()
