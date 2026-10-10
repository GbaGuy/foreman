//! The crew config, the prompts, and the plan → work → review loop. No UI here.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;

pub const BACKENDS: [(&str, &str); 3] = [
    ("claude", "claude -p {prompt}"),
    ("codex", "codex exec {prompt}"),
    ("opencode", "opencode run -m opencode/nemotron-3.5-lightning-free {prompt}"),
];
/// The manager is Claude with read-only tools: it can look around but not do the work.
/// (`--tools=` needs the "=": the flag takes several values and would swallow the prompt.)
pub const MANAGER_COMMAND: &str = "claude -p --tools=Read,Grep,Glob {prompt}";
pub const CLAUDE_MODELS: [&str; 3] = ["opus", "sonnet", "haiku"];

pub fn manager_command(model: &str) -> String {
    format!("claude -p --model {model} --tools=Read,Grep,Glob {{prompt}}")
}
const PLAN_FORMAT: &str = r#"[{"worker": "<name>", "role": "<role>", "task": "<task>", "step": 1}]"#;

#[derive(Clone, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub role: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Crew {
    #[serde(default = "default_rounds")]
    pub max_rounds: u32,
    pub manager: Agent,
    pub workers: Vec<Agent>,
}

fn default_rounds() -> u32 {
    3
}

impl Crew {
    pub fn path() -> PathBuf {
        let home = std::env::var_os("HOME").unwrap_or_default();
        PathBuf::from(home).join(".config/foreman/crew.json")
    }

    pub fn load() -> Crew {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| {
                let agent = |name: &str, role: &str| Agent {
                    name: name.into(),
                    command: BACKENDS[0].1.into(),
                    role: role.into(),
                };
                let mut manager = agent("manager", "Engineering lead");
                manager.command = MANAGER_COMMAND.into();
                Crew {
                    max_rounds: 3,
                    manager,
                    workers: vec![
                        agent("coder", "Writes and edits code"),
                        agent("reviewer", "Reviews work for bugs and gaps"),
                    ],
                }
            })
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap())
    }

    pub fn find(&self, name: &str) -> Option<&Agent> {
        std::iter::once(&self.manager).chain(&self.workers).find(|a| a.name == name)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Idle,
    Working,
    Done,
    Failed,
}

/// How a log line should look; the UI maps these to theme colours.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Out,
    Prompt,
    Info,
    Good,
    Warn,
    Bad,
}

#[derive(Debug)]
pub enum Msg {
    Log(String, Kind, String),
    State(String, State),
    Role(String, String),
    Notify(String),
    /// A user/manager exchange to keep as conversation history.
    Remember(String),
    Finished,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    pub worker: String,
    pub role: String,
    pub task: String,
    pub step: i64,
}

/// Split the template and drop the prompt in as one argv item (no shell).
pub fn build_argv(command: &str, prompt: &str) -> Option<Vec<String>> {
    let mut argv = shlex::split(command)?;
    let mut placed = false;
    for part in argv.iter_mut().filter(|p| *p == "{prompt}") {
        *part = prompt.to_string();
        placed = true;
    }
    if !placed {
        argv.push(prompt.to_string());
    }
    (!argv.is_empty()).then_some(argv)
}

pub fn manager_prompt(message: &str, workers: &[Agent], history: &str) -> String {
    let crew: Vec<String> = workers
        .iter()
        .map(|w| format!("- {}: {}", w.name, if w.role.is_empty() { "general agent" } else { &w.role }))
        .collect();
    let crew = if crew.is_empty() { "(none)".to_string() } else { crew.join("\n") };
    let history = if history.is_empty() { String::new() } else { format!("Conversation so far:\n{history}\n\n") };
    format!(
        "You are the manager of a team of AI agents working in the current directory. You do not do \
         the work yourself and you do not use tools to write, edit or run anything: you break the \
         work down and command your workers.\n\n{history}\
         The user says: {message}\n\nYour workers:\n{crew}\n\n\
         If this needs any work done (code, files, research, writing, checking), reply with ONLY a JSON \
         array assigning it, no prose. Give each worker a specific role and a concrete task. Tasks with \
         the same step run in parallel; a later step sees the output of earlier steps, so anything that \
         checks work (review, testing) must have a higher step than the work it checks:\n{PLAN_FORMAT}\n\n\
         Only if it is a quick question or chat that needs no work, or no worker can do it, answer it \
         yourself in plain text with no JSON."
    )
}

pub fn review_prompt(goal: &str, report: &str, last_round: bool) -> String {
    let ask = if last_round {
        "This was the last round: give a final summary of what got done and what is left.".to_string()
    } else {
        format!(
            "If the goal is fully met, say so and include no JSON. Otherwise end your reply with \
             a JSON array assigning the next round of work, in this format:\n{PLAN_FORMAT}"
        )
    };
    format!("Goal: {goal}\n\nYour workers reported:\n{report}\n\nReview their work: what got done and what is missing. \
              Do not do missing work yourself; assign it. {ask}")
}

/// Pull the JSON array ending the manager's reply; keep known workers, default step 1.
pub fn parse_plan(text: &str, names: &[String]) -> Vec<Task> {
    let Some(end) = text.rfind(']') else { return vec![] };
    // the outermost array is the first "[" that parses through to the final "]"
    let plan = text[..end]
        .match_indices('[')
        .find_map(|(start, _)| serde_json::from_str::<Value>(&text[start..=end]).ok());
    let Some(Value::Array(items)) = plan else { return vec![] };
    let field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    items
        .iter()
        .map(|v| Task {
            worker: field(v, "worker"),
            role: field(v, "role"),
            task: field(v, "task"),
            step: v.get("step").and_then(Value::as_i64).unwrap_or(1),
        })
        .filter(|t| names.contains(&t.worker) && !t.task.is_empty())
        .collect()
}

fn tail(s: &str, max: usize) -> &str {
    let mut i = s.len().saturating_sub(max);
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

/// Drop terminal escape sequences (colours, cursor moves) that CLIs print.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI: ESC [ params final-byte; anything else: drop ESC and the next char
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// List opencode's models with a tag: "free" (opencode's free cloud models), "local", or "".
pub fn opencode_models() -> Result<Vec<(String, &'static str)>, String> {
    let out = std::process::Command::new("opencode").arg("models").output().map_err(|e| format!("opencode: {e}"))?;
    let mut models: Vec<(String, &'static str)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| strip_ansi(l).trim().to_string())
        .filter(|l| l.contains('/'))
        .map(|l| {
            let tag = if l.ends_with("-free") { "free" } else if l.starts_with("local-") { "local" } else { "" };
            (l, tag)
        })
        .collect();
    models.sort_by_key(|(_, tag)| ["free", "local", ""].iter().position(|t| t == tag));
    if models.is_empty() { Err("opencode listed no models".into()) } else { Ok(models) }
}

/// Run one agent's CLI, stream its output as Log messages, return (ok, all output).
pub async fn run_agent(agent: Agent, prompt: String, tx: UnboundedSender<Msg>) -> (bool, String) {
    let name = agent.name.clone();
    let log = |kind, text: String| tx.send(Msg::Log(name.clone(), kind, text)).ok();
    tx.send(Msg::State(name.clone(), State::Working)).ok();
    let first = prompt.lines().next().unwrap_or_default();
    let short: String = first.chars().take(120).collect();
    log(Kind::Prompt, format!("▶ {short}{}", if short.len() < first.len() { "…" } else { "" }));

    let Some(argv) = build_argv(&agent.command, &prompt) else {
        log(Kind::Bad, format!("bad command: {}", agent.command));
        tx.send(Msg::State(name.clone(), State::Failed)).ok();
        return (false, String::new());
    };
    // sh only merges stderr into stdout; the argv goes through "$@" untouched.
    let child = Command::new("sh")
        .args(["-c", "exec \"$@\" 2>&1", "sh"])
        .args(&argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut out = String::new();
    let ok = match child {
        Err(e) => {
            log(Kind::Bad, e.to_string());
            false
        }
        Ok(mut child) => {
            let mut reader = BufReader::new(child.stdout.take().unwrap());
            let mut buf = Vec::new();
            while reader.read_until(b'\n', &mut buf).await.unwrap_or(0) > 0 {
                let line = strip_ansi(String::from_utf8_lossy(&buf).trim_end()).replace('\t', "    ");
                out.push_str(&line);
                out.push('\n');
                log(Kind::Out, line);
                buf.clear();
            }
            match child.wait().await {
                Ok(status) if status.success() => true,
                Ok(status) => {
                    log(Kind::Bad, format!("exited with {status}"));
                    false
                }
                Err(e) => {
                    log(Kind::Bad, e.to_string());
                    false
                }
            }
        }
    };
    tx.send(Msg::State(name, if ok { State::Done } else { State::Failed })).ok();
    (ok, out)
}

/// Run one round step by step; append this round's output to the report.
async fn run_plan(crew: &Crew, goal: &str, plan: &[Task], report: &mut String, tx: &UnboundedSender<Msg>, stop: &AtomicBool) {
    let mut steps: Vec<i64> = plan.iter().map(|t| t.step).collect();
    steps.sort();
    steps.dedup();
    for step in steps {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let batch: Vec<&Task> = plan.iter().filter(|t| t.step == step).collect();
        // ponytail: only the last 12k chars of history go to workers; summarise rounds if that loses context
        let earlier = if report.trim().is_empty() {
            String::new()
        } else {
            format!("\n\nWork done so far by the team:\n{}", tail(report, 12_000))
        };
        let handles: Vec<_> = batch
            .iter()
            .map(|t| {
                let prompt = format!("Your role: {}\nTeam goal: {goal}\n\nYour task: {}{earlier}", t.role, t.task);
                tokio::spawn(run_agent(crew.find(&t.worker).unwrap().clone(), prompt, tx.clone()))
            })
            .collect();
        for (t, h) in batch.iter().zip(handles) {
            let (_, out) = h.await.unwrap_or_default();
            report.push_str(&format!("\n## {} ({})\n{}\n", t.worker, t.role, tail(&out, 6_000)));
        }
    }
}

/// Plan, run the workers, review, and re-plan until the manager is satisfied or rounds run out.
pub async fn orchestrate(crew: Crew, goal: String, history: String, tx: UnboundedSender<Msg>, stop: Arc<AtomicBool>) {
    let boss = crew.manager.name.clone();
    let log = |kind, text: String| tx.send(Msg::Log(boss.clone(), kind, text)).ok();
    let names: Vec<String> = crew.workers.iter().map(|w| w.name.clone()).collect();

    let (ok, reply) = run_agent(crew.manager.clone(), manager_prompt(&goal, &crew.workers, &history), tx.clone()).await;
    let mut plan = parse_plan(&reply, &names);
    if plan.is_empty() && ok {
        log(Kind::Good, "Answered directly, no work needed.".into());
        tx.send(Msg::Remember(format!("User: {goal}\nManager: {}", tail(&reply, 1_500)))).ok();
    }
    let mut report = String::new();
    let mut round = 1;
    while !plan.is_empty() {
        log(Kind::Info, format!("Round {round} assignments"));
        for t in &plan {
            tx.send(Msg::Role(t.worker.clone(), t.role.clone())).ok();
            tx.send(Msg::State(t.worker.clone(), State::Idle)).ok();
            log(Kind::Info, format!("  step {}  {} as {}: {}", t.step, t.worker, t.role, t.task));
        }
        report.push_str(&format!("\n# Round {round}\n"));
        run_plan(&crew, &goal, &plan, &mut report, &tx, &stop).await;
        if stop.load(Ordering::Relaxed) {
            log(Kind::Warn, format!("Stopped during round {round}."));
            break;
        }
        let last = round >= crew.max_rounds;
        let (_, review) = run_agent(crew.manager.clone(), review_prompt(&goal, tail(&report, 24_000), last), tx.clone()).await;
        plan = if last { vec![] } else { parse_plan(&review, &names) };
        if plan.is_empty() {
            log(Kind::Good, format!("Done after {round} round(s)."));
            tx.send(Msg::Remember(format!("User: {goal}\nManager (after {round} round(s) of work): {}", tail(&review, 1_500)))).ok();
            tx.send(Msg::Notify("Crew finished".into())).ok();
        }
        round += 1;
    }
    tx.send(Msg::Finished).ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_plan() {
        let reply = "Sure!\n```json\n[{\"worker\": \"coder\", \"role\": \"dev\", \"task\": \"x\"}, {\"worker\": \"ghost\", \"task\": \"y\"}]\n```";
        let plan = parse_plan(reply, &names(&["coder"]));
        assert_eq!(plan, vec![Task { worker: "coder".into(), role: "dev".into(), task: "x".into(), step: 1 }]);
        assert!(parse_plan("no json here", &names(&["coder"])).is_empty());
        assert!(parse_plan("[not json]", &names(&["coder"])).is_empty());
        assert!(parse_plan(r#"{"worker": "coder"}]"#, &names(&["coder"])).is_empty());
    }

    #[test]
    fn parses_plan_after_prose_with_brackets() {
        let review = "Done: [x] tests, see [1].\nNext:\n[{\"worker\": \"coder\", \"task\": \"fix [edge] case\", \"step\": 2}]";
        let plan = parse_plan(review, &names(&["coder"]));
        assert_eq!(plan.len(), 1);
        assert_eq!((plan[0].task.as_str(), plan[0].step), ("fix [edge] case", 2));
        assert!(parse_plan("All good, goal met [1].", &names(&["coder"])).is_empty());
    }

    #[test]
    fn keeps_prompt_as_one_arg() {
        assert_eq!(build_argv("claude -p {prompt}", "a b; rm -rf /").unwrap(), ["claude", "-p", "a b; rm -rf /"]);
        assert_eq!(build_argv("mytool --go", "hi").unwrap(), ["mytool", "--go", "hi"]);
        assert!(build_argv("unclosed 'quote", "hi").is_none());
    }

    /// Fake manager: plans coder, then on review asks checker for a round 2, then says done.
    async fn run_crew(max_rounds: u32, goal: &str) -> Vec<Msg> {
        let script = r##"case "$1" in
            *"hello"*) echo 'Hi there!' ;;
            *"no prose"*) echo '[{"worker": "coder", "role": "dev", "task": "write it"}]' ;;
            *"# Round 2"*) echo 'ALL DONE' ;;
            *) echo 'REVIEW1 more work: [{"worker": "checker", "role": "qa", "task": "check it"}]' ;;
            esac"##;
        let agent = |name: &str, command: String| Agent { name: name.into(), command, role: String::new() };
        let crew = Crew {
            max_rounds,
            manager: agent("manager", format!("sh -c {} sh {{prompt}}", shlex::try_quote(script).unwrap())),
            workers: vec![
                agent("coder", "echo WORKED {prompt}".into()),
                agent("checker", "echo CHECKED {prompt}".into()),
                agent("idler", "echo nope {prompt}".into()),
            ],
        };
        let (tx, mut rx) = unbounded_channel();
        orchestrate(crew, goal.into(), "User: earlier\nManager: ok".into(), tx, Arc::default()).await;
        let mut msgs = vec![];
        while let Ok(m) = rx.try_recv() {
            msgs.push(m);
        }
        msgs
    }

    fn log_of(msgs: &[Msg], who: &str) -> String {
        msgs.iter()
            .filter_map(|m| match m {
                Msg::Log(n, _, t) if n == who => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn replans_until_done() {
        let msgs = run_crew(3, "ship it").await;
        assert!(log_of(&msgs, "coder").contains("WORKED"));
        let checker = log_of(&msgs, "checker");
        assert!(checker.contains("CHECKED") && checker.contains("WORKED"), "round 2 sees round 1");
        let boss = log_of(&msgs, "manager");
        assert!(boss.contains("ALL DONE") && boss.contains("Done after 2 round(s)"));
        assert!(log_of(&msgs, "idler").is_empty());
        assert!(matches!(msgs.last(), Some(Msg::Finished)));
        assert!(msgs.iter().any(|m| matches!(m, Msg::Remember(r) if r.contains("ALL DONE"))));
    }

    #[tokio::test]
    async fn answers_chat_without_workers() {
        let msgs = run_crew(3, "hello").await;
        let boss = log_of(&msgs, "manager");
        assert!(boss.contains("Hi there!") && boss.contains("Answered directly"));
        assert!(log_of(&msgs, "coder").is_empty() && log_of(&msgs, "checker").is_empty());
    }

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\x1b[0m> plan \x1b[1;32mPONG\x1b[0m\r"), "> plan PONG");
    }

    #[test]
    fn manager_prompt_carries_history_and_delegates() {
        let p = manager_prompt("fix it", &[], "User: hi\nManager: hello");
        assert!(p.contains("User: hi") && p.contains("(none)") && p.contains("do not do"));
    }

    #[tokio::test]
    async fn stops_at_round_cap() {
        let msgs = run_crew(1, "ship it").await;
        assert!(log_of(&msgs, "checker").is_empty());
        assert!(log_of(&msgs, "manager").contains("Done after 1 round(s)"));
    }
}
