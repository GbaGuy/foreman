import asyncio, json, sys

import foreman
from foreman import Foreman, build_argv, parse_plan


def test_parse_plan():
    reply = 'Sure!\n```json\n[{"worker": "coder", "role": "dev", "task": "x"}, {"worker": "ghost", "task": "y"}]\n```'
    assert parse_plan(reply, {"coder"}) == [{"worker": "coder", "role": "dev", "task": "x", "step": 1}]
    assert parse_plan("no json here", {"coder"}) == []
    assert parse_plan("[not json]", {"coder"}) == []
    assert parse_plan('{"worker": "coder"}]', {"coder"}) == []


def test_parse_plan_after_prose_with_brackets():
    review = 'Done: [x] tests, see [1].\nNext:\n[{"worker": "coder", "task": "fix [edge] case", "step": 2}]'
    assert parse_plan(review, {"coder"}) == [{"worker": "coder", "task": "fix [edge] case", "step": 2}]
    assert parse_plan("All good, goal met [1].", {"coder"}) == []


def test_build_argv_keeps_prompt_whole():
    assert build_argv("claude -p {prompt}", "a b; rm -rf /") == ["claude", "-p", "a b; rm -rf /"]
    assert build_argv("mytool --go", "hi") == ["mytool", "--go", "hi"]


def run_crew(tmp_path, monkeypatch, max_rounds):
    """Fake manager: plans coder, then on review asks checker for a round 2, then says done."""
    round1 = [{"worker": "coder", "role": "dev", "task": "write it"}]
    round2 = [{"worker": "checker", "role": "qa", "task": "check it"}]
    script = tmp_path / "boss.py"
    script.write_text(
        "import sys; p = sys.argv[1]\n"
        f"if 'no prose' in p: print({json.dumps(round1)!r})\n"
        f"elif '# Round 2' not in p: print('REVIEW1 more work:', {json.dumps(round2)!r})\n"
        "else: print('ALL DONE')\n"
    )
    crew = {"max_rounds": max_rounds,
            "manager": {"name": "manager", "command": f"{sys.executable} {script} {{prompt}}"},
            "workers": [{"name": "coder", "command": "echo WORKED {prompt}"},
                        {"name": "checker", "command": "echo CHECKED {prompt}"},
                        {"name": "idler", "command": "echo nope {prompt}"}]}
    monkeypatch.setattr(foreman, "CONFIG", tmp_path / "crew.json")
    (tmp_path / "crew.json").write_text(json.dumps(crew))

    async def run():
        app = Foreman()
        async with app.run_test() as pilot:
            await pilot.click("#goal")
            await pilot.press(*"ship it", "enter")
            await app.workers.wait_for_complete()

            def log(name):  # hidden tabs buffer writes until they are first shown
                rl = app.query_one(f"#log-{name}")
                return "\n".join([l.text for l in rl.lines] + [str(d.content) for d in rl._deferred_renders])
            return {n: log(n) for n in app.agents}, app.roles
    return asyncio.run(run())


def test_replan_loop(tmp_path, monkeypatch):
    logs, roles = run_crew(tmp_path, monkeypatch, max_rounds=3)
    assert "WORKED" in logs["coder"]
    assert "CHECKED" in logs["checker"] and "WORKED" in logs["checker"]  # round 2 saw round 1
    assert "ALL DONE" in logs["manager"] and "Done after 2 round(s)" in logs["manager"]
    assert logs["idler"] == "" and roles == {"coder": "dev", "checker": "qa"}


def test_round_cap(tmp_path, monkeypatch):
    logs, _ = run_crew(tmp_path, monkeypatch, max_rounds=1)
    assert logs["checker"] == "" and "Done after 1 round(s)" in logs["manager"]
