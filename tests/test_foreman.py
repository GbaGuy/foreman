import asyncio, json, sys

import foreman
from foreman import Foreman, build_argv, parse_plan


def test_parse_plan():
    reply = 'Sure!\n```json\n[{"worker": "coder", "role": "dev", "task": "x"}, {"worker": "ghost", "task": "y"}]\n```'
    assert parse_plan(reply, {"coder"}) == [{"worker": "coder", "role": "dev", "task": "x", "step": 1}]
    assert parse_plan("no json here", {"coder"}) == []
    assert parse_plan("[not json]", {"coder"}) == []


def test_build_argv_keeps_prompt_whole():
    assert build_argv("claude -p {prompt}", "a b; rm -rf /") == ["claude", "-p", "a b; rm -rf /"]
    assert build_argv("mytool --go", "hi") == ["mytool", "--go", "hi"]


def test_orchestrate_end_to_end(tmp_path, monkeypatch):
    plan = json.dumps([{"worker": "coder", "role": "dev", "task": "write it", "step": 1},
                       {"worker": "checker", "role": "qa", "task": "check it", "step": 2}])
    script = tmp_path / "boss.py"
    script.write_text(f"import sys; print({plan!r} if 'JSON' in sys.argv[1] else 'REVIEWED')")
    fake_boss = f"{sys.executable} {script} {{prompt}}"
    crew = {"manager": {"name": "manager", "command": fake_boss},
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
            assert "WORKED" in log("coder") and "REVIEWED" in log("manager")
            assert "WORKED" in log("checker")  # step 2 saw step 1's output
            assert log("idler") == "" and app.roles == {"coder": "dev", "checker": "qa"}
    asyncio.run(run())
