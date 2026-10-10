//! Config, prompts, and the plan → work → review loop. No UI here.
//!
//! The manager invents its workers per job: each plan entry names a worker, a
//! role and a task, and every worker is a fresh run of the `worker` command.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;

/// The manager is Claude with read-only tools: it can look around but not do the work.
/// (`--tools=` needs the "=": the flag takes several values and would swallow the prompt.)
pub const MANAGER_COMMAND: &str = "claude -p --tools=Read,Grep,Glob {prompt}";
pub const WORKER_COMMAND: &str = "opencode run -m opencode/nemotron-3.5-lightning-free --auto {prompt}";
pub const CLAUDE_MODELS: [&str; 3] = ["opus", "sonnet", "haiku"];
const PLAN_FORMAT: &str = r#"[{"worker": "<short-name>", "role": "<role>", "task": "<task>", "step": 1}]"#;

pub fn manager_command(model: &str) -> String {
    format!("claude -p --model {model} --tools=Read,Grep,Glob {{prompt}}")
}

pub fn worker_command(model: &str) -> String {
    format!("opencode run -m {model} --auto {{prompt}}")
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Crew {
    #[serde(default = "default_rounds")]
    pub max_rounds: u32,
    pub manager: String,
    pub worker: String,
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
            .unwrap_or_else(|| Crew { max_rounds: 3, manager: MANAGER_COMMAND.into(), worker: WORKER_COMMAND.into() })
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap())
    }
}

/// The model a command asks for (`-m x`, `--model x`, `--model=x`), if any.
pub fn model_of(command: &str) -> Option<&str> {
    let words: Vec<&str> = command.split_whitespace().collect();
    words.iter().enumerate().find_map(|(i, w)| match *w {
        "-m" | "--model" => words.get(i + 1).copied(),
        _ => w.strip_prefix("--model="),
    })
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Working,
    Done,
    Failed,
}

/// How a chat line should look; the UI maps these to theme colours.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    User,
    Out,
    Info,
    Good,
    Warn,
    Bad,
}

#[derive(Debug)]
pub enum Msg {
    Log(Kind, String),
    Manager(State),
    Worker(String, String, State), // name, role, state
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

pub fn manager_prompt(message: &str, history: &str) -> String {
    let history = if history.is_empty() { String::new() } else { format!("Conversation so far:\n{history}\n\n") };
    format!(
        "You are the manager of a team of AI agents working in the current directory. You do not do \
         the work yourself and you do not use tools to write, edit or run anything. Instead you create \
         workers as needed: each worker is a fresh AI agent that can read and write files and run \
         commands here. You give each one a short name, a role and a concrete, self-contained task.\n\n\
         {history}The user says: {message}\n\n\
         If this needs any work done (code, files, research, writing, checking), reply with ONLY a JSON \
         array creating the workers, no prose. Use as few workers as the job needs. Tasks with the same \
         step run in parallel; a later step sees the output of earlier steps, so anything that checks \
         work (review, testing) must have a higher step than the work it checks:\n{PLAN_FORMAT}\n\n\
         Only if it is a quick question or chat that needs no work, answer it yourself in plain text \
         with no JSON."
    )
}

pub fn review_prompt(goal: &str, report: &str, last_round: bool) -> String {
    let ask = if last_round {
        "This was the last round: give the user a short final summary of what got done and what is left.".to_string()
    } else {
        format!(
            "Start with a short summary for the user. If the goal is fully met, include no JSON. \
             Otherwise end your reply with a JSON array creating workers for the next round, in this \
             format:\n{PLAN_FORMAT}"
        )
    };
    format!(
        "Goal: {goal}\n\nYour workers reported:\n{report}\n\nReview their work: what got done and what is \
         missing. Do not do missing work yourself; create workers for it. {ask}"
    )
}

/// Split a manager reply into (prose for the user, plan). The plan is the JSON array
/// that ends the reply; with no usable plan the whole reply is prose.
pub fn split_plan(text: &str) -> (String, Vec<Task>) {
    let whole = (text.trim().to_string(), vec![]);
    let Some(end) = text.rfind(']') else { return whole };
    // the outermost array is the first "[" that parses through to the final "]"
    let found = text[..end]
        .match_indices('[')
        .find_map(|(start, _)| serde_json::from_str::<Value>(&text[start..=end]).ok().map(|v| (start, v)));
    let Some((start, Value::Array(items))) = found else { return whole };
    let field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let plan: Vec<Task> = items
        .iter()
        .map(|v| Task {
            worker: field(v, "worker").chars().filter(|c| c.is_alphanumeric() || "-_".contains(*c)).take(20).collect(),
            role: field(v, "role"),
            task: field(v, "task"),
            step: v.get("step").and_then(Value::as_i64).unwrap_or(1),
        })
        .filter(|t| !t.worker.is_empty() && !t.task.is_empty())
        .collect();
    if plan.is_empty() {
        return whole;
    }
    let prose = text[..start].trim().trim_end_matches("```json").trim_end_matches("```").trim();
    (prose.to_string(), plan)
}

fn tail(s: &str, max: usize) -> &str {
    let mut i = s.len().saturating_sub(max);
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

fn last_line(s: &str) -> &str {
    s.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output").trim()
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

/// Run a CLI agent to completion; return (ok, all output with escapes stripped).
pub async fn run_command(command: &str, prompt: &str) -> (bool, String) {
    let Some(argv) = build_argv(command, prompt) else {
        return (false, format!("bad command: {command}"));
    };
    // sh only merges stderr into stdout; the argv goes through "$@" untouched.
    let child = Command::new("sh")
        .args(["-c", "exec \"$@\" 2>&1", "sh"])
        .args(&argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => return (false, e.to_string()),
    };
    let mut out = String::new();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut buf = Vec::new();
    while reader.read_until(b'\n', &mut buf).await.unwrap_or(0) > 0 {
        out.push_str(&strip_ansi(String::from_utf8_lossy(&buf).trim_end()).replace('\t', "    "));
        out.push('\n');
        buf.clear();
    }
    match child.wait().await {
        Ok(status) if status.success() => (true, out),
        Ok(status) => (false, format!("{out}exited with {status}")),
        Err(e) => (false, format!("{out}{e}")),
    }
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
                let prompt = format!(
                    "You are {}, a worker on an AI team.\nYour role: {}\nTeam goal: {goal}\n\nYour task: {}{earlier}",
                    t.worker, t.role, t.task
                );
                let (command, tx, t) = (crew.worker.clone(), tx.clone(), (*t).clone());
                tokio::spawn(async move {
                    tx.send(Msg::Worker(t.worker.clone(), t.role.clone(), State::Working)).ok();
                    let (ok, out) = run_command(&command, &prompt).await;
                    tx.send(Msg::Worker(t.worker.clone(), t.role.clone(), if ok { State::Done } else { State::Failed })).ok();
                    if ok {
                        tx.send(Msg::Log(Kind::Good, format!("● {} finished", t.worker))).ok();
                    } else {
                        tx.send(Msg::Log(Kind::Bad, format!("✗ {} failed: {}", t.worker, last_line(&out)))).ok();
                    }
                    out
                })
            })
            .collect();
        for (t, h) in batch.iter().zip(handles) {
            let out = h.await.unwrap_or_default();
            report.push_str(&format!("\n## {} ({})\n{}\n", t.worker, t.role, tail(&out, 6_000)));
        }
    }
}

/// Ask the manager; show its prose to the user and return (ok, prose, plan).
async fn ask_manager(crew: &Crew, prompt: &str, tx: &UnboundedSender<Msg>) -> (bool, String, Vec<Task>) {
    tx.send(Msg::Manager(State::Working)).ok();
    let (ok, reply) = run_command(&crew.manager, prompt).await;
    tx.send(Msg::Manager(if ok { State::Done } else { State::Failed })).ok();
    if !ok {
        tx.send(Msg::Log(Kind::Bad, format!("The manager failed: {}", last_line(&reply)))).ok();
        return (false, String::new(), vec![]);
    }
    let (prose, plan) = split_plan(&reply);
    for line in prose.lines() {
        tx.send(Msg::Log(Kind::Out, line.to_string())).ok();
    }
    (true, prose, plan)
}

/// Plan, run the workers, review, and re-plan until the manager is satisfied or rounds run out.
pub async fn orchestrate(crew: Crew, goal: String, history: String, tx: UnboundedSender<Msg>, stop: Arc<AtomicBool>) {
    let log = |kind, text: String| tx.send(Msg::Log(kind, text)).ok();
    let (ok, prose, mut plan) = ask_manager(&crew, &manager_prompt(&goal, &history), &tx).await;
    if ok && plan.is_empty() {
        tx.send(Msg::Remember(format!("User: {goal}\nManager: {}", tail(&prose, 1_500)))).ok();
    }
    let mut report = String::new();
    let mut round = 1;
    while !plan.is_empty() {
        log(Kind::Info, format!("Round {round}: {} worker(s)", plan.len()));
        for t in &plan {
            log(Kind::Info, format!("  step {} · {} ({}): {}", t.step, t.worker, t.role, t.task));
        }
        report.push_str(&format!("\n# Round {round}\n"));
        run_plan(&crew, &goal, &plan, &mut report, &tx, &stop).await;
        if stop.load(Ordering::Relaxed) {
            log(Kind::Warn, format!("Stopped during round {round}."));
            break;
        }
        let last = round >= crew.max_rounds;
        let (ok, summary, next) = ask_manager(&crew, &review_prompt(&goal, tail(&report, 24_000), last), &tx).await;
        plan = if last || !ok { vec![] } else { next };
        if plan.is_empty() {
            log(Kind::Good, format!("Done after {round} round(s)."));
            tx.send(Msg::Remember(format!("User: {goal}\nManager (after {round} round(s) of work): {}", tail(&summary, 1_500)))).ok();
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

    #[test]
    fn splits_plan_from_prose() {
        let reply = "Here's the plan:\n```json\n[{\"worker\": \"coder\", \"role\": \"dev\", \"task\": \"x\"}, {\"worker\": \"\", \"task\": \"y\"}]\n```";
        let (prose, plan) = split_plan(reply);
        assert_eq!(prose, "Here's the plan:");
        assert_eq!(plan, vec![Task { worker: "coder".into(), role: "dev".into(), task: "x".into(), step: 1 }]);
        assert_eq!(split_plan("no json here"), ("no json here".into(), vec![]));
        assert!(split_plan("[not json]").1.is_empty());
        assert!(split_plan(r#"{"worker": "coder"}]"#).1.is_empty());
    }

    #[test]
    fn splits_plan_after_prose_with_brackets() {
        let review = "Done: [x] tests, see [1].\nNext:\n[{\"worker\": \"fix er!\", \"task\": \"fix [edge] case\", \"step\": 2}]";
        let (prose, plan) = split_plan(review);
        assert_eq!(prose, "Done: [x] tests, see [1].\nNext:");
        assert_eq!((plan[0].worker.as_str(), plan[0].task.as_str(), plan[0].step), ("fixer", "fix [edge] case", 2));
        let (prose, plan) = split_plan("All good, goal met [1].");
        assert!(plan.is_empty() && prose == "All good, goal met [1].");
    }

    #[test]
    fn keeps_prompt_as_one_arg() {
        assert_eq!(build_argv("claude -p {prompt}", "a b; rm -rf /").unwrap(), ["claude", "-p", "a b; rm -rf /"]);
        assert_eq!(build_argv("mytool --go", "hi").unwrap(), ["mytool", "--go", "hi"]);
        assert!(build_argv("unclosed 'quote", "hi").is_none());
    }

    #[test]
    fn finds_model() {
        assert_eq!(model_of(&manager_command("sonnet")), Some("sonnet"));
        assert_eq!(model_of(WORKER_COMMAND), Some("opencode/nemotron-3.5-lightning-free"));
        assert_eq!(model_of("x --model=opus {prompt}"), Some("opus"));
        assert_eq!(model_of(MANAGER_COMMAND), None);
    }

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\x1b[0m> plan \x1b[1;32mPONG\x1b[0m\r"), "> plan PONG");
    }

    #[test]
    fn manager_prompt_carries_history_and_delegates() {
        let p = manager_prompt("fix it", "User: hi\nManager: hello");
        assert!(p.contains("User: hi") && p.contains("create workers") && p.contains("do not do"));
    }

    /// Fake manager: invents coder, then on review a checker for round 2, then says done
    /// (only if the checker saw round 1's work).
    async fn run_crew(max_rounds: u32, goal: &str) -> Vec<Msg> {
        let script = r##"case "$1" in
            *"hello"*) echo 'Hi there!' ;;
            *"no prose"*) echo 'On it.'; echo '[{"worker": "coder", "role": "dev", "task": "write it"}]' ;;
            *"# Round 2"*"check it"*"write it"*) echo 'ALL DONE' ;;
            *"# Round 2"*) echo 'NO HISTORY' ;;
            *) echo 'Needs a check.'; echo '[{"worker": "checker", "role": "qa", "task": "check it"}]' ;;
            esac"##;
        let crew = Crew {
            max_rounds,
            manager: format!("sh -c {} sh {{prompt}}", shlex::try_quote(script).unwrap()),
            worker: "echo DID {prompt}".into(),
        };
        let (tx, mut rx) = unbounded_channel();
        orchestrate(crew, goal.into(), "User: earlier\nManager: ok".into(), tx, Arc::default()).await;
        let mut msgs = vec![];
        while let Ok(m) = rx.try_recv() {
            msgs.push(m);
        }
        msgs
    }

    fn chat(msgs: &[Msg]) -> String {
        msgs.iter()
            .filter_map(|m| match m {
                Msg::Log(_, t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn workers(msgs: &[Msg]) -> Vec<(&str, State)> {
        msgs.iter()
            .filter_map(|m| match m {
                Msg::Worker(n, _, s) => Some((n.as_str(), *s)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn replans_until_done() {
        let msgs = run_crew(3, "ship it").await;
        let chat = chat(&msgs);
        assert!(chat.contains("On it.") && !chat.contains("\"worker\""), "prose shown, JSON hidden");
        assert!(chat.contains("ALL DONE"), "round 2 saw round 1: {chat}");
        assert!(chat.contains("Done after 2 round(s)"));
        let w = workers(&msgs);
        assert!(w.contains(&("coder", State::Done)) && w.contains(&("checker", State::Done)));
        assert!(matches!(msgs.last(), Some(Msg::Finished)));
        assert!(msgs.iter().any(|m| matches!(m, Msg::Remember(r) if r.contains("ALL DONE"))));
    }

    #[tokio::test]
    async fn stops_at_round_cap() {
        let msgs = run_crew(1, "ship it").await;
        assert!(!workers(&msgs).iter().any(|(n, _)| *n == "checker"));
        assert!(chat(&msgs).contains("Done after 1 round(s)"));
    }

    #[tokio::test]
    async fn answers_chat_without_workers() {
        let msgs = run_crew(3, "hello").await;
        assert!(chat(&msgs).contains("Hi there!"));
        assert!(workers(&msgs).is_empty());
        assert!(msgs.iter().any(|m| matches!(m, Msg::Remember(r) if r.contains("Hi there!"))));
    }
}
