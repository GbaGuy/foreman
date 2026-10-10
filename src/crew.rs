//! Config, prompts, and the plan → work → review loop. No UI here.
//!
//! The manager invents its workers per job: each plan entry names a worker, a
//! role and a task, and every worker is a fresh run of the `worker` command.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinSet;

/// The manager is Claude with read-only tools: it can look around but not do the work.
/// stream-json lets the UI show it thinking, reading files, and writing its reply live.
/// (`--tools=` needs the "=": the flag takes several values and would swallow the prompt.)
pub const MANAGER_COMMAND: &str =
    "claude -p --output-format stream-json --verbose --include-partial-messages --tools=Read,Grep,Glob {prompt}";
/// `--thinking` puts the worker's reasoning in its output, so you and the manager can both read it.
pub const WORKER_COMMAND: &str = "opencode run -m opencode/nemotron-3.5-lightning-free --auto --thinking {prompt}";
pub const CLAUDE_MODELS: [&str; 3] = ["opus", "sonnet", "haiku"];
const PLAN_FORMAT: &str = r#"[{"worker": "<short-name>", "role": "<role>", "task": "<task>", "step": 1}]"#;

pub fn manager_command(model: &str) -> String {
    MANAGER_COMMAND.replacen("claude -p", &format!("claude -p --model {model}"), 1)
}

pub fn worker_command(model: &str) -> String {
    format!("opencode run -m {model} --auto --thinking {{prompt}}")
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Crew {
    #[serde(default = "default_rounds")]
    pub max_rounds: u32,
    pub manager: String,
    pub worker: String,
    /// How many workers may run at once.
    #[serde(default = "default_parallel")]
    pub max_parallel: usize,
    /// A worker running longer than this is killed.
    #[serde(default = "default_timeout")]
    pub worker_timeout_mins: u64,
    /// A worker that prints nothing for this long is killed (some agent CLIs hang after finishing).
    #[serde(default = "default_idle")]
    pub worker_idle_mins: u64,
}

fn default_rounds() -> u32 {
    3
}

fn default_parallel() -> usize {
    4
}

fn default_timeout() -> u64 {
    20
}

fn default_idle() -> u64 {
    5
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
            .unwrap_or_else(|| Crew {
                max_rounds: default_rounds(),
                manager: MANAGER_COMMAND.into(),
                worker: WORKER_COMMAND.into(),
                max_parallel: default_parallel(),
                worker_timeout_mins: default_timeout(),
                worker_idle_mins: default_idle(),
            })
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
    Think, // the manager's live activity: files it reads, searches it runs
    Rule,  // a section divider, e.g. "Round 1 · 2 workers"
    Out,
    Good,
    Warn,
    Bad,
}

#[derive(Debug)]
pub enum Msg {
    Log(Kind, String),
    Manager(State),
    Thinking(u64),       // the manager is thinking; roughly this many more tokens
    ManagerText(String), // a chunk of the manager's reply as it's written
    Assign(Task),                  // the manager handed out a task (the worker is queued)
    Worker(String, State),         // name, state
    Output(String, String),        // a worker's output line: name, line
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
         work (review, testing) must have a higher step than the work it checks. Never give two workers in \
         the same step the same file to edit; they would overwrite each other:\n{PLAN_FORMAT}\n\n\
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
        "Goal: {goal}\n\nYour workers reported (including their reasoning):\n{report}\n\n\
         Review their work. Don't take their word for it: use your read tools to check the actual files \
         they made or changed, and look for mistakes in their reasoning, files they overwrote, and claims \
         that don't match the files. Do not fix anything yourself; create workers to fix it. {ask}"
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

/// Run a CLI agent to completion, handing each output line to `on_line`; kill it if it
/// goes quiet for longer than `idle`. Return (ok, all output with escapes stripped).
pub async fn run_command(command: &str, prompt: &str, idle: Option<Duration>, on_line: impl Fn(&str)) -> (bool, String) {
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
    loop {
        let read = reader.read_until(b'\n', &mut buf);
        let n = match idle {
            None => read.await.unwrap_or(0),
            Some(limit) => match tokio::time::timeout(limit, read).await {
                Ok(n) => n.unwrap_or(0),
                Err(_) => {
                    child.kill().await.ok();
                    out.push_str(&format!("no output for {}s, stopped\n", limit.as_secs()));
                    return (false, out);
                }
            },
        };
        if n == 0 {
            break;
        }
        let line = strip_ansi(String::from_utf8_lossy(&buf).trim_end()).replace('\t', "    ");
        on_line(&line);
        out.push_str(&line);
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
/// Dropping this future (when the user stops the job) aborts the workers, which kills their processes.
async fn run_plan(crew: &Crew, goal: &str, plan: &[Task], report: &mut String, tx: &UnboundedSender<Msg>) {
    let mut steps: Vec<i64> = plan.iter().map(|t| t.step).collect();
    steps.sort();
    steps.dedup();
    let slots = Arc::new(Semaphore::new(crew.max_parallel.max(1)));
    let timeout = Duration::from_secs(crew.worker_timeout_mins.max(1) * 60);
    let idle = Some(Duration::from_secs(crew.worker_idle_mins.max(1) * 60));
    for step in steps {
        let batch: Vec<&Task> = plan.iter().filter(|t| t.step == step).collect();
        // ponytail: only the last 12k chars of history go to workers; summarise rounds if that loses context
        let earlier = if report.trim().is_empty() {
            String::new()
        } else {
            format!("\n\nWork done so far by the team:\n{}", tail(report, 12_000))
        };
        let mut set = JoinSet::new();
        for (i, t) in batch.iter().enumerate() {
            let prompt = format!(
                "You are {}, a worker on an AI team.\nYour role: {}\nTeam goal: {goal}\n\nYour task: {}{earlier}",
                t.worker, t.role, t.task
            );
            let (command, tx, t, slots) = (crew.worker.clone(), tx.clone(), (*t).clone(), slots.clone());
            set.spawn(async move {
                let _slot = slots.acquire_owned().await;
                tx.send(Msg::Worker(t.worker.clone(), State::Working)).ok();
                let echo = |line: &str| _ = tx.send(Msg::Output(t.worker.clone(), line.to_string()));
                let (ok, out) = tokio::time::timeout(timeout, run_command(&command, &prompt, idle, echo))
                    .await
                    .unwrap_or_else(|_| (false, format!("timed out after {} min", timeout.as_secs() / 60)));
                tx.send(Msg::Worker(t.worker.clone(), if ok { State::Done } else { State::Failed })).ok();
                // success shows in the round's to-do list; a failure also gets a line saying why
                if !ok {
                    tx.send(Msg::Log(Kind::Bad, format!("{} failed: {}", t.worker, last_line(&out)))).ok();
                }
                (i, out)
            });
        }
        let mut outs = vec![String::new(); batch.len()];
        while let Some(Ok((i, out))) = set.join_next().await {
            outs[i] = out;
        }
        for (t, out) in batch.iter().zip(outs) {
            report.push_str(&format!("\n## {} ({})\n{}\n", t.worker, t.role, tail(&out, 6_000)));
        }
    }
}

/// What one line of Claude's stream-json output means for the UI.
#[derive(Debug, PartialEq)]
pub enum Stream {
    Thinking(u64),
    Tool(String),
    Text(String),
}

pub fn parse_stream(line: &str) -> Vec<Stream> {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![] };
    match v["type"].as_str() {
        Some("stream_event") => {
            let ev = &v["event"];
            let item = match (ev["type"].as_str(), ev["delta"]["type"].as_str()) {
                (Some("content_block_start"), _) if ev["content_block"]["type"] == "thinking" => Some(Stream::Thinking(0)),
                (_, Some("thinking_delta")) => Some(Stream::Thinking(ev["delta"]["estimated_tokens"].as_u64().unwrap_or(0))),
                (_, Some("text_delta")) => ev["delta"]["text"].as_str().map(|t| Stream::Text(t.into())),
                _ => None,
            };
            item.into_iter().collect()
        }
        Some("assistant") => v["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| {
                let arg = ["file_path", "pattern", "path", "query"].iter().find_map(|k| b["input"][*k].as_str()).unwrap_or("");
                Stream::Tool(format!("{} {arg}", b["name"].as_str().unwrap_or("tool")).trim().to_string())
            })
            .collect(),
        _ => vec![],
    }
}

/// The final answer in a stream-json transcript: (ok, text).
pub fn stream_result(out: &str) -> Option<(bool, String)> {
    out.lines().rev().find_map(|l| {
        let v: Value = serde_json::from_str(l).ok()?;
        (v["type"] == "result").then(|| (!v["is_error"].as_bool().unwrap_or(false), v["result"].as_str().unwrap_or("").to_string()))
    })
}

/// Ask the manager; show its prose to the user and return (ok, prose, plan).
async fn ask_manager(crew: &Crew, prompt: &str, tx: &UnboundedSender<Msg>) -> (bool, String, Vec<Task>) {
    tx.send(Msg::Manager(State::Working)).ok();
    let streaming = crew.manager.contains("stream-json");
    let cwd = std::env::current_dir().map(|d| format!("{}/", d.display())).unwrap_or_default();
    let live = |line: &str| {
        for item in parse_stream(line) {
            let msg = match item {
                Stream::Thinking(n) => Msg::Thinking(n),
                Stream::Text(t) => Msg::ManagerText(t),
                Stream::Tool(t) => Msg::Log(Kind::Think, format!("→ {}", t.replace(&cwd, ""))),
            };
            tx.send(msg).ok();
        }
    };
    let (ok, out) = run_command(&crew.manager, prompt, None, live).await;
    let (ok, reply) = match streaming.then(|| stream_result(&out)).flatten() {
        Some((good, text)) => (ok && good, text),
        None => (ok, out),
    };
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
pub async fn orchestrate(crew: Crew, goal: String, history: String, tx: UnboundedSender<Msg>) {
    let log = |kind, text: String| tx.send(Msg::Log(kind, text)).ok();
    let (ok, prose, mut plan) = ask_manager(&crew, &manager_prompt(&goal, &history), &tx).await;
    if ok && plan.is_empty() {
        tx.send(Msg::Remember(format!("User: {goal}\nManager: {}", tail(&prose, 1_500)))).ok();
    }
    let mut report = String::new();
    let mut round = 1;
    while !plan.is_empty() {
        let n = plan.len();
        log(Kind::Rule, format!("Round {round} · {n} worker{}", if n == 1 { "" } else { "s" }));
        for t in &plan {
            tx.send(Msg::Assign(t.clone())).ok();
        }
        report.push_str(&format!("\n# Round {round}\n"));
        run_plan(&crew, &goal, &plan, &mut report, &tx).await;
        let last = round >= crew.max_rounds;
        let (ok, summary, next) = ask_manager(&crew, &review_prompt(&goal, tail(&report, 24_000), last), &tx).await;
        plan = if last || !ok { vec![] } else { next };
        if plan.is_empty() {
            log(Kind::Good, format!("Done after {round} round{}", if round == 1 { "" } else { "s" }));
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
        assert!(manager_command("opus").starts_with("claude -p --model opus --output-format stream-json"));
    }

    #[test]
    fn parses_claude_stream() {
        let think = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":50}}}"#;
        let start = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}}"#;
        let text = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi"}}}"#;
        let tools = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/p/a.txt"}},{"type":"tool_use","name":"Grep","input":{"pattern":"fn main"}}]}}"#;
        assert_eq!(parse_stream(think), vec![Stream::Thinking(50)]);
        assert_eq!(parse_stream(start), vec![Stream::Thinking(0)]);
        assert_eq!(parse_stream(text), vec![Stream::Text("Hi".into())]);
        assert_eq!(parse_stream(tools), vec![Stream::Tool("Read /p/a.txt".into()), Stream::Tool("Grep fn main".into())]);
        assert!(parse_stream("not json").is_empty() && parse_stream(r#"{"type":"system"}"#).is_empty());
        let out = format!("{text}\n{}\n", r#"{"type":"result","is_error":false,"result":"Final"}"#);
        assert_eq!(stream_result(&out), Some((true, "Final".into())));
        assert_eq!(stream_result("plain"), None);
    }

    /// A stream-json manager: its tool calls and thinking reach the UI, and the plan
    /// comes from the final "result" event.
    #[tokio::test]
    async fn streams_manager_activity() {
        let script = r##"case "$1" in
            *"no prose"*)
              echo '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","estimated_tokens":30}}}'
              echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"notes.md"}}]}}'
              echo '{"type":"result","is_error":false,"result":"[{"worker": "coder", "task": "go"}]"}' ;;
            *) echo '{"type":"result","is_error":false,"result":"All done."}' ;;
            esac"##;
        let crew = Crew {
            max_rounds: 2,
            // $0 is "stream-json", which marks the command as streaming
            manager: format!("sh -c {} stream-json {{prompt}}", shlex::try_quote(script).unwrap()),
            worker: "echo DID {prompt}".into(),
            max_parallel: 4,
            worker_timeout_mins: 1,
            worker_idle_mins: 1,
        };
        let (tx, mut rx) = unbounded_channel();
        orchestrate(crew, "ship it".into(), String::new(), tx).await;
        let mut msgs = vec![];
        while let Ok(m) = rx.try_recv() {
            msgs.push(m);
        }
        assert!(msgs.iter().any(|m| matches!(m, Msg::Thinking(30))));
        assert!(chat(&msgs).contains("→ Read notes.md") && chat(&msgs).contains("All done."));
        assert!(workers(&msgs).contains(&("coder", State::Done)));
    }

    /// A CLI that prints its answer and then hangs is killed once it goes quiet, output kept.
    #[tokio::test]
    async fn kills_quiet_command() {
        let start = std::time::Instant::now();
        let (ok, out) = run_command("sh -c 'echo answer; sleep 5' sh {prompt}", "p", Some(Duration::from_millis(300)), |_| ()).await;
        assert!(!ok && out.starts_with("answer\n") && out.contains("no output for"));
        assert!(start.elapsed() < Duration::from_secs(2));
        let (ok, out) = run_command("echo fine {prompt}", "p", Some(Duration::from_secs(5)), |_| ()).await;
        assert!(ok && out == "fine p\n");
    }

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\x1b[0m> plan \x1b[1;32mPONG\x1b[0m\r"), "> plan PONG");
    }

    #[test]
    fn manager_prompt_carries_history_and_delegates() {
        let p = manager_prompt("fix it", "User: hi\nManager: hello");
        assert!(p.contains("User: hi") && p.contains("create workers") && p.contains("do not do"));
        assert!(p.contains("same file"));
        assert!(review_prompt("g", "r", false).contains("check the actual files"));
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
            max_parallel: 4,
            worker_timeout_mins: 1,
            worker_idle_mins: 1,
        };
        let (tx, mut rx) = unbounded_channel();
        orchestrate(crew, goal.into(), "User: earlier\nManager: ok".into(), tx).await;
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
                Msg::Worker(n, s) => Some((n.as_str(), *s)),
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
        assert!(chat.contains("Done after 2 rounds"));
        let w = workers(&msgs);
        assert!(w.contains(&("coder", State::Done)) && w.contains(&("checker", State::Done)));
        assert!(msgs.iter().any(|m| matches!(m, Msg::Output(n, l) if n == "coder" && l.starts_with("DID"))));
        assert!(matches!(msgs.last(), Some(Msg::Finished)));
        assert!(msgs.iter().any(|m| matches!(m, Msg::Remember(r) if r.contains("ALL DONE"))));
    }

    /// Three 0.3s workers with one slot must take about 0.9s; with three slots about 0.3s.
    #[tokio::test]
    async fn caps_parallel_workers() {
        let plan: Vec<Task> = ["a", "b", "c"]
            .iter()
            .map(|n| Task { worker: n.to_string(), role: String::new(), task: "t".into(), step: 1 })
            .collect();
        for (slots, min_ms, max_ms) in [(1, 850, 2_000), (3, 250, 800)] {
            let crew = Crew {
                max_rounds: 1,
                manager: String::new(),
                worker: "sh -c 'sleep 0.3' sh {prompt}".into(),
                max_parallel: slots,
                worker_timeout_mins: 1,
                worker_idle_mins: 1,
            };
            let (tx, _rx) = unbounded_channel();
            let start = std::time::Instant::now();
            run_plan(&crew, "g", &plan, &mut String::new(), &tx).await;
            let ms = start.elapsed().as_millis();
            assert!((min_ms..max_ms).contains(&ms), "{slots} slot(s) took {ms}ms");
        }
    }

    /// Aborting a job (Ctrl+S) must kill its workers before they finish.
    #[tokio::test]
    async fn abort_kills_workers() {
        let marker = std::env::temp_dir().join(format!("foreman-abort-{}", std::process::id()));
        let crew = Crew {
            max_rounds: 1,
            manager: String::new(),
            worker: format!("sh -c 'sleep 0.6; touch {}' sh {{prompt}}", marker.display()),
            max_parallel: 4,
            worker_timeout_mins: 1,
            worker_idle_mins: 1,
        };
        let plan = vec![Task { worker: "w".into(), role: String::new(), task: "t".into(), step: 1 }];
        let (tx, _rx) = unbounded_channel();
        let job = tokio::spawn(async move { run_plan(&crew, "g", &plan, &mut String::new(), &tx).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        job.abort();
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(!marker.exists(), "worker kept running after abort");
    }

    #[tokio::test]
    async fn stops_at_round_cap() {
        let msgs = run_crew(1, "ship it").await;
        assert!(!workers(&msgs).iter().any(|(n, _)| *n == "checker"));
        assert!(chat(&msgs).contains("Done after 1 round"));
    }

    #[tokio::test]
    async fn answers_chat_without_workers() {
        let msgs = run_crew(3, "hello").await;
        assert!(chat(&msgs).contains("Hi there!"));
        assert!(workers(&msgs).is_empty());
        assert!(msgs.iter().any(|m| matches!(m, Msg::Remember(r) if r.contains("Hi there!"))));
    }
}
