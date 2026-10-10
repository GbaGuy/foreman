//! Config, prompts, and the plan → work → review loop. No UI here.
//!
//! The manager invents its workers per job: each plan entry names a worker, a
//! role, a task, and the files it owns. Workers in a step run in parallel
//! unless they claim the same file; foreman records which files each step
//! actually changed and reports that, and each worker's report, to the manager.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinSet;

/// The manager is Claude with read-only tools: it can look around and check work, but not do it.
/// JSON output carries its token usage. (`--tools=` needs the "=": the flag takes several
/// values and would swallow the prompt.)
pub const MANAGER_COMMAND: &str = "claude -p --output-format json --tools=Read,Grep,Glob {prompt}";
/// opencode's JSON events carry each action, the reply, and token usage per step. `--standalone`
/// gives each worker a private server, so killing a worker really stops it (with the shared
/// background service its session would keep running and writing files).
pub const WORKER_COMMAND: &str = "opencode run --standalone -m opencode/nemotron-3.5-lightning-free --auto --format json {prompt}";
pub const CLAUDE_MODELS: [&str; 3] = ["opus", "sonnet", "haiku"];
const PLAN_FORMAT: &str = r#"[{"worker": "<short-name>", "role": "<role>", "task": "<task>", "files": ["<file it may create or edit>"], "step": 1, "model": "<model>"}]"#;

pub fn manager_command(model: &str) -> String {
    format!("claude -p --model {model} --output-format json --tools=Read,Grep,Glob {{prompt}}")
}

pub fn worker_command(model: &str) -> String {
    format!("opencode run --standalone -m {model} --auto --format json {{prompt}}")
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
    /// A worker that uses more tokens than this is stopped: a model stuck in a loop burns them fast. 0 = no limit.
    #[serde(default = "default_max_tokens")]
    pub worker_max_tokens: u64,
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

/// opencode prints nothing while a model streams a big file write; slow free models take minutes.
fn default_max_tokens() -> u64 {
    500_000
}

fn default_idle() -> u64 {
    15
}

impl Crew {
    pub fn path() -> PathBuf {
        let home = std::env::var_os("HOME").unwrap_or_default();
        PathBuf::from(home).join(".config/foreman/crew.json")
    }

    pub fn load() -> Crew {
        std::fs::read_to_string(Self::path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| Crew {
            max_rounds: default_rounds(),
            manager: MANAGER_COMMAND.into(),
            worker: WORKER_COMMAND.into(),
            max_parallel: default_parallel(),
            worker_timeout_mins: default_timeout(),
            worker_idle_mins: default_idle(),
            worker_max_tokens: default_max_tokens(),
        })
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap())
    }
}

/// `command` with its model swapped for `model` (unchanged if `model` is empty or it names none).
pub fn with_model(command: &str, model: &str) -> String {
    match model_of(command) {
        Some(current) if !model.is_empty() => command.replacen(&format!(" {current}"), &format!(" {model}"), 1),
        _ => command.to_string(),
    }
}

/// The plan format, and which models the manager may give workers, with each one's track record.
fn plan_format(models: &[String]) -> String {
    let model = if models.is_empty() {
        "Leave out \"model\".".to_string()
    } else {
        let record = ModelRecord::load();
        let list: Vec<String> = models.iter().map(|m| format!("- {m}: {}", record.get(m).map_or("no record yet".into(), ModelRun::describe))).collect();
        format!(
            "\"model\" is the worker's AI model. Pick one from this list for each worker, or leave it out \
             for the default. Each comes with foreman's record of its past workers: prefer models that \
             finish their work, avoid ones that often fail or get stopped, and try an untried model on a \
             small task first.\n{}",
            list.join("\n")
        )
    };
    format!("{PLAN_FORMAT}\n{model}")
}

/// foreman's own state folder: `$XDG_STATE_HOME/foreman` or `~/.local/state/foreman`.
pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    base.join("foreman")
}

/// How a model's workers have done so far.
#[derive(Clone, Copy, Default, Serialize, Deserialize, Debug, PartialEq)]
pub struct ModelRun {
    pub runs: u32,
    pub finished: u32,
    pub tokens: u64,
}

impl ModelRun {
    fn describe(&self) -> String {
        let avg = self.tokens / u64::from(self.runs.max(1)) / 1_000;
        format!("{} workers, {} finished, {} failed or stopped, about {avg}k tokens each", self.runs, self.finished, self.runs - self.finished)
    }
}

/// Every model's record, kept in the state folder across sessions.
pub struct ModelRecord;

impl ModelRecord {
    fn path() -> PathBuf {
        state_dir().join("models.json")
    }

    pub fn load() -> HashMap<String, ModelRun> {
        std::fs::read_to_string(Self::path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    /// Add one worker's outcome. Workers finish in parallel, so the read-modify-write is locked.
    pub fn add(model: &str, finished: bool, tokens: u64) {
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock();
        let mut all = Self::load();
        let run = all.entry(model.to_string()).or_default();
        (run.runs, run.finished, run.tokens) = (run.runs + 1, run.finished + u32::from(finished), run.tokens + tokens);
        std::fs::create_dir_all(state_dir()).ok();
        std::fs::write(Self::path(), serde_json::to_string_pretty(&all).unwrap_or_default()).ok();
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

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub enum State {
    Working,
    Done,
    Failed,
}

/// How a chat line should look; the UI maps these to theme colours.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub enum Kind {
    User,
    Rule, // a section divider, e.g. "Round 1 · 2 workers"
    Out,
    Good,
    Warn,
    Bad,
}

/// Tokens an agent used (input includes cached input) and what its CLI says it cost.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cost: f64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Usage) {
        (self.input, self.output, self.cost) = (self.input + o.input, self.output + o.output, self.cost + o.cost);
    }
}

#[derive(Debug)]
pub enum Msg {
    Log(Kind, String),
    Manager(State),
    Assign(Task),           // the manager handed out a task (the worker is queued)
    Worker(String, State),  // name, state
    Output(String, String), // a worker's output line: name, line
    Usage(String, Usage),   // more tokens used by "manager" or a worker
    Notify(String),
    /// A message between a worker and the manager mid-task: from, to, text.
    Said(String, String, String),
    /// A user/manager exchange to keep as conversation history.
    Remember(String),
    Finished,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub worker: String,
    pub role: String,
    pub task: String,
    pub files: Vec<String>, // files this worker may create or edit
    pub step: i64,
    pub model: String, // the opencode model the manager picked; empty means the configured one
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

pub fn manager_prompt(message: &str, history: &str, models: &[String]) -> String {
    let history = if history.is_empty() { String::new() } else { format!("Conversation so far:\n{history}\n\n") };
    format!(
        "You are the manager of a team of AI agents working in the current directory. You do not do \
         the work yourself and you do not use tools to write, edit or run anything. Instead you create \
         workers as needed: each worker is a fresh AI agent that can read and write files and run \
         commands here. You give each one a short name, a role, a concrete self-contained task, and the \
         list of files it owns: the only files it may create or edit. You keep track of who owns what.\n\n\
         {history}The user says: {message}\n\n\
         If this needs any work done (code, files, research, writing, checking), reply with ONLY a JSON \
         array creating the workers, no prose. Create as many workers as the job needs: split it into \
         small, focused tasks (one per file, feature or check) and give each its own worker, so they \
         run in parallel. Don't pile work onto one worker that several could share. Tasks with the same step run in parallel; a later step sees the reports \
         of earlier steps, so anything that checks work (review, testing) must have a higher step than the \
         work it checks. Unless the job is trivial, end with a checker worker that runs and tests the \
         result and reports whether it really works. Two workers in the same step must never own the same file; give each its own \
         files (a worker that only reads or runs things owns none):\n{}\n\n\
         Only if it is a quick question or chat that needs no work, answer it yourself in plain text \
         with no JSON.",
        plan_format(models)
    )
}

pub fn review_prompt(goal: &str, report: &str, last_round: bool, models: &[String]) -> String {
    let last = if last_round { " This was the last round, so say clearly what is left; the JSON tells foreman the job isn't done." } else { "" };
    let ask = format!(
        "Start with a short summary for the user. Only call the goal fully met when a worker has actually \
         run or tested the result (ran the tests, built it, ran the program) and reported it working, and \
         the files back that up; then include no JSON. If anything is wrong, unfinished, or not yet \
         checked that way, end your reply with a JSON array creating workers for the next round (fixers, \
         and a checker in a later step that runs and tests the result), in this format:\n{}{last}",
        plan_format(models)
    );
    format!(
        "Goal: {goal}\n\nYour workers' reports, with foreman's record of which files each step really \
         changed:\n{report}\n\n\
         Review their work. Don't take their word for it: use your read tools to check the actual files, \
         and look for mistakes, files changed by a worker that didn't own them, work that overwrote other \
         work, and claims that don't match the files. Do not fix anything yourself; create workers to fix \
         it. {ask}"
    )
}

/// Split a manager reply into (prose for the user, plan). The plan is the JSON array
/// that ends the reply; with no usable plan the whole reply is prose.
pub fn split_plan(text: &str) -> (String, Vec<Task>) {
    let whole = (text.trim().to_string(), vec![]);
    let Some(end) = text.rfind(']') else {
        return whole;
    };
    // the outermost array is the first "[" that parses through to the final "]"
    let found = text[..end].match_indices('[').find_map(|(start, _)| serde_json::from_str::<Value>(&text[start..=end]).ok().map(|v| (start, v)));
    let Some((start, Value::Array(items))) = found else {
        return whole;
    };
    let field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let plan: Vec<Task> = items
        .iter()
        .map(|v| Task {
            worker: field(v, "worker").chars().filter(|c| c.is_alphanumeric() || "-_".contains(*c)).take(20).collect(),
            role: field(v, "role"),
            task: field(v, "task"),
            files: v["files"].as_array().into_iter().flatten().filter_map(Value::as_str).map(clean_path).collect(),
            step: v.get("step").and_then(Value::as_i64).unwrap_or(1),
            model: field(v, "model"),
        })
        .filter(|t| !t.worker.is_empty() && !t.task.is_empty())
        .collect();
    if plan.is_empty() {
        return whole;
    }
    let prose = text[..start].trim().trim_end_matches("```json").trim_end_matches("```").trim();
    (prose.to_string(), plan)
}

/// "./src/a.rs" and "src/a.rs" are the same file.
fn clean_path(p: &str) -> String {
    p.trim().trim_start_matches("./").to_string()
}

/// Split a step into groups that can run in parallel: no two tasks in a group own the same file.
fn conflict_free<'a>(batch: &[&'a Task]) -> Vec<Vec<&'a Task>> {
    let mut groups: Vec<Vec<&Task>> = vec![];
    for &t in batch {
        let clash = |g: &Vec<&Task>| g.iter().any(|o| o.files.iter().any(|f| t.files.contains(f)));
        match groups.iter_mut().find(|g| !clash(g)) {
            Some(g) => g.push(t),
            None => groups.push(vec![t]),
        }
    }
    groups
}

/// Every file under `dir` with its size and modified time; skips VCS, build and dependency folders.
pub fn snapshot(dir: &Path) -> HashMap<String, (u64, SystemTime)> {
    const SKIP: [&str; 8] = [".git", "target", "node_modules", ".venv", "venv", "__pycache__", "dist", "build"];
    // ponytail: a full walk per step, capped; switch to git status or a watcher for huge trees
    const MAX: usize = 20_000;
    let mut files = HashMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                if !SKIP.contains(&name.as_str()) && !name.starts_with('.') {
                    stack.push(e.path());
                }
            } else if files.len() < MAX {
                let rel = e.path().strip_prefix(dir).unwrap_or(&e.path()).to_string_lossy().to_string();
                files.insert(rel, (meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
            }
        }
    }
    files
}

/// Files added, changed or deleted between two snapshots, sorted.
pub fn changed(before: &HashMap<String, (u64, SystemTime)>, after: &HashMap<String, (u64, SystemTime)>) -> Vec<String> {
    let mut out: Vec<String> = after.iter().filter(|(f, v)| before.get(*f) != Some(v)).map(|(f, _)| f.clone()).collect();
    out.extend(before.keys().filter(|f| !after.contains_key(*f)).cloned());
    out.sort();
    out
}

/// Who owned each changed file: (lines for the chat, files nobody owned).
fn attribute(files: &[String], group: &[&Task]) -> (String, Vec<String>) {
    let mut parts = vec![];
    let mut unowned = vec![];
    for f in files {
        let owners: Vec<&str> = group.iter().filter(|t| t.files.contains(f)).map(|t| t.worker.as_str()).collect();
        if owners.is_empty() {
            unowned.push(f.clone());
            parts.push(format!("{f} (no owner)"));
        } else {
            parts.push(format!("{f} ({})", owners.join(", ")));
        }
    }
    (parts.join(" · "), unowned)
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
            let tag = if l.ends_with("-free") {
                "free"
            } else if l.starts_with("local-") {
                "local"
            } else {
                ""
            };
            (l, tag)
        })
        .collect();
    models.sort_by_key(|(_, tag)| ["free", "local", ""].iter().position(|t| t == tag));
    if models.is_empty() { Err("opencode listed no models".into()) } else { Ok(models) }
}

/// What one line of an agent CLI's output means. Plain text passes through; the JSON
/// formats of `claude -p --output-format json` and `opencode run --format json` are decoded.
#[derive(Debug, PartialEq)]
pub enum Event {
    Lines(Vec<String>),
    /// opencode finished a step: its usage, and whether the model stopped there (its last step).
    Usage(Usage, bool),
    /// claude's final answer: (text, is_error, usage).
    Result(String, bool, Usage),
}

pub fn parse_event(line: &str) -> Event {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Event::Lines(vec![line.to_string()]);
    };
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    match v["type"].as_str() {
        Some("result") => {
            let u = &v["usage"];
            let usage = Usage {
                input: n(&u["input_tokens"]) + n(&u["cache_creation_input_tokens"]) + n(&u["cache_read_input_tokens"]),
                output: n(&u["output_tokens"]),
                cost: v["total_cost_usd"].as_f64().unwrap_or(0.0),
            };
            Event::Result(v["result"].as_str().unwrap_or_default().to_string(), v["is_error"].as_bool().unwrap_or(false), usage)
        }
        Some("step_finish") => {
            let t = &v["part"]["tokens"];
            Event::Usage(
                Usage {
                    input: n(&t["input"]) + n(&t["cache"]["read"]) + n(&t["cache"]["write"]),
                    output: n(&t["output"]) + n(&t["reasoning"]),
                    cost: v["part"]["cost"].as_f64().unwrap_or(0.0),
                },
                v["part"]["reason"] == "stop",
            )
        }
        Some("text") => Event::Lines(v["part"]["text"].as_str().unwrap_or_default().lines().map(String::from).collect()),
        Some("tool_use") => {
            let part = &v["part"];
            let tool = part["tool"].as_str().unwrap_or("tool");
            let mut name = tool.to_string();
            name[..1].make_ascii_uppercase();
            let input = &part["state"]["input"];
            let arg = ["filePath", "path", "command", "pattern", "url", "query"].iter().find_map(|k| input[*k].as_str()).unwrap_or("");
            let mut lines = vec![format!("⚙ {name} {}", arg.lines().next().unwrap_or("")).trim_end().to_string()];
            if part["state"]["status"] == "error" {
                lines.push(format!("Error: {}", part["state"]["error"].as_str().unwrap_or("tool failed")));
            }
            Event::Lines(lines)
        }
        Some(_) => Event::Lines(vec![]), // step_start, reasoning, …: nothing to show
        None => Event::Lines(vec![line.to_string()]),
    }
}

/// Run a CLI agent to completion, handing each output line to `on_line` and each usage update
/// to `on_usage`; kill it if it goes quiet for longer than `idle`.
/// Returns (ok, output as text, total usage).
pub async fn run_command(
    command: &str,
    prompt: &str,
    dir: &Path,
    idle: Option<Duration>,
    max_tokens: Option<u64>,
    on_line: impl Fn(&str),
    on_usage: impl Fn(Usage),
) -> (bool, String, Usage) {
    let Some(argv) = build_argv(command, prompt) else {
        return (false, format!("bad command: {command}"), Usage::default());
    };
    // sh only merges stderr into stdout; the argv goes through "$@" untouched.
    let child = Command::new("sh")
        .args(["-c", "exec \"$@\" 2>&1", "sh"])
        .args(&argv)
        .current_dir(dir)
        .env("FOREMAN_SOCK", ask_socket())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => return (false, e.to_string(), Usage::default()),
    };
    let (mut out, mut usage, mut result, mut errored) = (String::new(), Usage::default(), None, false);
    let mut finished = false; // the agent said its model stopped normally
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
                    return (false, out, usage);
                }
            },
        };
        if n == 0 {
            break;
        }
        let raw = strip_ansi(String::from_utf8_lossy(&buf).trim_end()).replace('\t', "    ");
        buf.clear();
        match parse_event(&raw) {
            Event::Lines(lines) => {
                for line in lines {
                    on_line(&line);
                    out.push_str(&line);
                    out.push('\n');
                }
            }
            Event::Usage(u, stop) => {
                (usage, finished) = (Usage { input: usage.input + u.input, output: usage.output + u.output, cost: usage.cost + u.cost }, stop);
                on_usage(u);
                if max_tokens.is_some_and(|max| usage.input + usage.output > max) {
                    child.kill().await.ok();
                    out.push_str(&format!("used over {} tokens (worker_max_tokens), stopped: it may be stuck in a loop\n", max_tokens.unwrap_or(0)));
                    return (false, out, usage);
                }
            }
            Event::Result(text, is_error, u) => {
                (usage, errored) = (Usage { input: usage.input + u.input, output: usage.output + u.output, cost: usage.cost + u.cost }, is_error);
                on_usage(u);
                result = Some(text);
            }
        }
    }
    let out = result.unwrap_or(out);
    match child.wait().await {
        // opencode sometimes exits 1 after its model finished fine; trust the finish over the exit code
        Ok(status) if (status.success() || finished) && !errored => (true, out, usage),
        Ok(status) => (false, format!("{out}\nexited with {status}"), usage),
        Err(e) => (false, format!("{out}\n{e}"), usage),
    }
}

/// Make opencode enforce what a worker owns: its file tools may only touch those files (with
/// `--auto`, an explicit deny still holds). Other agent CLIs are left alone; foreman's record of
/// changed files still flags them. The shell isn't covered, so the record stays the backstop.
pub fn own_files(command: &str, files: &[String], dir: &Path) -> String {
    if !command.trim_start().starts_with("opencode") {
        return command.to_string();
    }
    let mut edit = serde_json::Map::from_iter([("*".to_string(), Value::from("deny"))]);
    for f in files {
        edit.insert(f.clone(), "allow".into());
        edit.insert(dir.join(f).display().to_string(), "allow".into());
    }
    let config = serde_json::json!({ "permission": { "edit": edit } });
    let var = shlex::try_quote(&format!("OPENCODE_CONFIG_CONTENT={config}")).map(|q| q.into_owned()).unwrap_or_default();
    format!("env {var} {command}")
}

/// Where this foreman listens for workers' questions while a job runs (one job at a time).
pub fn ask_socket() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join(format!("foreman-{}.sock", std::process::id()))
}

/// The command a worker runs to ask the manager something.
fn ask_command() -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "foreman".into());
    shlex::try_quote(&exe).map(|q| q.into_owned()).unwrap_or(exe)
}

/// `foreman ask <worker> <question>`: send a question to the running job's manager and print the answer.
pub fn ask_client(worker: &str, question: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    let sock = std::env::var_os("FOREMAN_SOCK").ok_or("foreman ask only works inside a foreman worker")?;
    let mut conn = std::os::unix::net::UnixStream::connect(&sock).map_err(|e| format!("can't reach the manager: {e}"))?;
    let line = serde_json::json!({ "worker": worker, "question": question });
    conn.write_all(format!("{line}\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut answer = String::new();
    conn.read_to_string(&mut answer).map_err(|e| e.to_string())?;
    Ok(answer)
}

fn ask_prompt(goal: &str, team: &[Task], worker: &str, question: &str) -> String {
    let plan: String = team.iter().map(|t| format!("- {} ({}), step {}, owns [{}]: {}\n", t.worker, t.role, t.step, t.files.join(", "), t.task)).collect();
    format!(
        "You are the manager of a team of AI workers. Team goal: {goal}\n\nThe current plan:\n{plan}\n\
         Your worker {worker} is mid-task and asks you: {question}\n\n\
         Answer it directly and briefly so it can carry on. If it is reporting \
         that it's done or asking you to review its work, don't review it now: tell it to stop and give its final \
         report, and you'll review everything when the round ends. Use your read tools to check files if that \
         helps. You can't change the plan now: if the answer needs other work, tell the worker to put it \
         in its report."
    )
}

/// Answer workers' questions with the manager until dropped (the JoinSet aborts the listener).
fn listen_for_questions(crew: &Crew, goal: &str, team: Arc<Mutex<Vec<Task>>>, tx: &UnboundedSender<Msg>) -> Option<JoinSet<()>> {
    let path = ask_socket();
    std::fs::remove_file(&path).ok();
    let listener = tokio::net::UnixListener::bind(&path).ok()?;
    let (crew, goal, tx) = (crew.clone(), goal.to_string(), tx.clone());
    let mut set = JoinSet::new();
    set.spawn(async move {
        let mut asks = JoinSet::new(); // dropped with the listener, so stopping the job stops answers too
        while let Ok((mut conn, _)) = listener.accept().await {
            let (crew, goal, team, tx) = (crew.clone(), goal.clone(), team.clone(), tx.clone());
            asks.spawn(async move {
                let (read, mut write) = conn.split();
                let mut line = String::new();
                BufReader::new(read).read_line(&mut line).await.ok();
                let v: Value = serde_json::from_str(&line).unwrap_or_default();
                let (worker, question) = (v["worker"].as_str().unwrap_or("worker").to_string(), v["question"].as_str().unwrap_or_default());
                tx.send(Msg::Said(worker.clone(), "manager".into(), question.to_string())).ok();
                let prompt = ask_prompt(&goal, &team.lock().unwrap(), &worker, question);
                let count = |u: Usage| _ = tx.send(Msg::Usage("manager".into(), u));
                let (ok, answer, _) = run_command(&crew.manager, &prompt, Path::new("."), None, None, |_| (), count).await;
                let answer = if ok { answer.trim().to_string() } else { "The manager couldn't answer; use your best judgement and say so in your report.".into() };
                tx.send(Msg::Said("manager".into(), worker, answer.clone())).ok();
                write.write_all(answer.as_bytes()).await.ok();
            });
        }
    });
    Some(set)
}

/// Removes the question socket when the job ends or is stopped.
struct SocketFile(PathBuf);
impl Drop for SocketFile {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

fn worker_prompt(t: &Task, goal: &str, earlier: &str) -> String {
    let files = if t.files.is_empty() {
        "You own no files: do not create or edit any (reading and running things is fine).".to_string()
    } else {
        format!(
            "Files you own: {}. Only create or edit these; other workers may be editing other files at \
             the same time. If you need a change anywhere else, don't make it; say so in your report.",
            t.files.join(", ")
        )
    };
    format!(
        "You are {}, a worker on an AI team run by a manager.\nYour role: {}\nTeam goal: {goal}\n\n\
         Your task from the manager: {}\n\n{files}\n\n\
         You can run shell commands (make folders, install, build, run tests) with your shell tool. \
         You can talk to the manager while you work: if you are blocked, unsure what it wants, or need \
         something from another worker, run `{} ask {} \"<your question>\"` with your shell tool and it \
         prints the manager's answer. Only ask real questions; don't ask about things you can decide \
         yourself, and never use it to report progress or say you're done: your final reply is your report.\n\n\
         When you are done, end with a short report to the manager: what you did, which files you \
         changed, and anything you could not do or that needs another worker.{earlier}",
        t.worker,
        t.role,
        t.task,
        ask_command(),
        t.worker
    )
}

/// Run one round step by step; append each worker's report and the file changes to `report`.
/// Dropping this future (when the user stops the job) aborts the workers, which kills their processes.
async fn run_plan(crew: &Crew, goal: &str, plan: &[Task], dir: &Path, report: &mut String, tx: &UnboundedSender<Msg>) {
    let mut steps: Vec<i64> = plan.iter().map(|t| t.step).collect();
    steps.sort();
    steps.dedup();
    let slots = Arc::new(Semaphore::new(crew.max_parallel.max(1)));
    let timeout = Duration::from_secs(crew.worker_timeout_mins.max(1) * 60);
    let idle = Some(Duration::from_secs(crew.worker_idle_mins.max(1) * 60));
    let max_tokens = Some(crew.worker_max_tokens).filter(|&n| n > 0);
    for step in steps {
        let batch: Vec<&Task> = plan.iter().filter(|t| t.step == step).collect();
        let groups = conflict_free(&batch);
        if groups.len() > 1 {
            let msg = format!("Some step {step} workers own the same files, so they'll take turns instead of running together.");
            tx.send(Msg::Log(Kind::Warn, msg)).ok();
        }
        for group in groups {
            // ponytail: only the last 12k chars of history go to workers; summarise rounds if that loses context
            let earlier = if report.trim().is_empty() { String::new() } else { format!("\n\nReports from the team so far:\n{}", tail(report, 12_000)) };
            let before = snapshot(dir);
            let mut set = JoinSet::new();
            for (i, t) in group.iter().enumerate() {
                let prompt = worker_prompt(t, goal, &earlier);
                let command = own_files(&with_model(&crew.worker, &t.model), &t.files, dir);
                let model = model_of(&command).unwrap_or_default().to_string();
                let (tx, t, slots, dir) = (tx.clone(), (*t).clone(), slots.clone(), dir.to_path_buf());
                set.spawn(async move {
                    let _slot = slots.acquire_owned().await;
                    tx.send(Msg::Worker(t.worker.clone(), State::Working)).ok();
                    let echo = |line: &str| _ = tx.send(Msg::Output(t.worker.clone(), line.to_string()));
                    let count = |u: Usage| _ = tx.send(Msg::Usage(t.worker.clone(), u));
                    let (ok, out, used) = tokio::time::timeout(timeout, run_command(&command, &prompt, &dir, idle, max_tokens, echo, count))
                        .await
                        .unwrap_or_else(|_| (false, format!("timed out after {} min", timeout.as_secs() / 60), Usage::default()));
                    if !model.is_empty() {
                        ModelRecord::add(&model, ok, used.input + used.output);
                    }
                    tx.send(Msg::Worker(t.worker.clone(), if ok { State::Done } else { State::Failed })).ok();
                    if !ok {
                        tx.send(Msg::Log(Kind::Bad, format!("{} failed: {}", t.worker, last_line(&out)))).ok();
                    }
                    (i, out)
                });
            }
            let mut outs = vec![String::new(); group.len()];
            while let Some(Ok((i, out))) = set.join_next().await {
                outs[i] = out;
            }
            for (t, out) in group.iter().zip(outs) {
                report.push_str(&format!("\n## {} ({}), owns: {}\n{}\n", t.worker, t.role, t.files.join(", "), tail(&out, 6_000)));
            }
            // the manager's ledger: which files really changed, and whether their owner did it
            let files = changed(&before, &snapshot(dir));
            if !files.is_empty() {
                let (summary, unowned) = attribute(&files, &group);
                let kind = if unowned.is_empty() { Kind::Good } else { Kind::Warn };
                tx.send(Msg::Log(kind, format!("files changed: {summary}"))).ok();
                report.push_str(&format!("\n[foreman] Files changed by these workers: {summary}\n"));
            }
        }
    }
}

/// Ask the manager; show its prose to the user and return (ok, prose, plan).
async fn ask_manager(crew: &Crew, prompt: &str, dir: &Path, tx: &UnboundedSender<Msg>) -> (bool, String, Vec<Task>) {
    tx.send(Msg::Manager(State::Working)).ok();
    let count = |u: Usage| _ = tx.send(Msg::Usage("manager".into(), u));
    let (ok, reply, _) = run_command(&crew.manager, prompt, dir, None, None, |_| (), count).await;
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
    let dir = std::env::current_dir().unwrap_or_default();
    // the manager may pick among opencode's free models, when workers run opencode
    let models = if crew.worker.trim_start().starts_with("opencode") && model_of(&crew.worker).is_some() {
        let all = tokio::task::spawn_blocking(opencode_models).await.ok().and_then(Result::ok).unwrap_or_default();
        all.into_iter().filter(|(_, tag)| *tag == "free").map(|(m, _)| m).collect()
    } else {
        vec![]
    };
    let team = Arc::new(Mutex::new(vec![]));
    let _listener = listen_for_questions(&crew, &goal, team.clone(), &tx);
    let _socket = SocketFile(ask_socket());
    orchestrate_in(crew, goal, history, &models, &team, &dir, tx).await;
}

async fn orchestrate_in(crew: Crew, goal: String, history: String, models: &[String], team: &Mutex<Vec<Task>>, dir: &Path, tx: UnboundedSender<Msg>) {
    let log = |kind, text: String| tx.send(Msg::Log(kind, text)).ok();
    let (ok, prose, mut plan) = ask_manager(&crew, &manager_prompt(&goal, &history, models), dir, &tx).await;
    if ok && plan.is_empty() {
        tx.send(Msg::Remember(format!("User: {goal}\nManager: {}", tail(&prose, 1_500)))).ok();
    }
    let mut report = String::new();
    let mut round = 1;
    while !plan.is_empty() {
        let n = plan.len();
        log(Kind::Rule, format!("Round {round} · {n} worker{}", if n == 1 { "" } else { "s" }));
        for t in &mut plan {
            // only a model from the list foreman gave the manager; anything else gets the default
            if !models.contains(&t.model) {
                t.model.clear();
            }
            tx.send(Msg::Assign(t.clone())).ok();
        }
        *team.lock().unwrap() = plan.clone(); // what the manager sees when a worker asks it something
        report.push_str(&format!("\n# Round {round}\n"));
        run_plan(&crew, &goal, &plan, dir, &mut report, &tx).await;
        let last = round >= crew.max_rounds;
        let (ok, summary, next) = ask_manager(&crew, &review_prompt(&goal, tail(&report, 24_000), last, models), dir, &tx).await;
        plan = if last || !ok { vec![] } else { next.clone() };
        if ok && last && !next.is_empty() {
            log(Kind::Warn, format!("Not finished: stopped after {round} rounds (max_rounds) with work left"));
            tx.send(Msg::Remember(format!("User: {goal}\nManager (stopped unfinished after {round} round(s)): {}", tail(&summary, 1_500)))).ok();
            tx.send(Msg::Notify("Crew stopped with work left".into())).ok();
        } else if plan.is_empty() {
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
    use std::sync::Mutex;
    use tokio::sync::mpsc::unbounded_channel;

    fn task(worker: &str, files: &[&str], step: i64) -> Task {
        Task { worker: worker.into(), role: String::new(), task: "t".into(), files: files.iter().map(|f| f.to_string()).collect(), step, model: String::new() }
    }

    fn crew(manager: String, worker: &str, max_rounds: u32) -> Crew {
        Crew { max_rounds, manager, worker: worker.into(), max_parallel: 4, worker_timeout_mins: 1, worker_idle_mins: 1, worker_max_tokens: 0 }
    }

    /// A temp folder per test, removed afterwards.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> TempDir {
            let p = std::env::temp_dir().join(format!("foreman-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn splits_plan_from_prose() {
        let reply = "Here's the plan:\n```json\n[{\"worker\": \"coder\", \"role\": \"dev\", \"task\": \"x\", \"files\": [\"./src/a.rs\", \"b.rs\"]}, {\"worker\": \"\", \"task\": \"y\"}]\n```";
        let (prose, plan) = split_plan(reply);
        assert_eq!(prose, "Here's the plan:");
        assert_eq!(plan, vec![Task { worker: "coder".into(), role: "dev".into(), task: "x".into(), files: vec!["src/a.rs".into(), "b.rs".into()], step: 1, model: String::new() }]);
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
        assert!(plan[0].files.is_empty());
        let (prose, plan) = split_plan("All good, goal met [1].");
        assert!(plan.is_empty() && prose == "All good, goal met [1].");
    }

    #[test]
    fn keeps_parallel_workers_off_each_others_files() {
        let (a, b, c, d) = (task("a", &["x.py"], 1), task("b", &["y.py"], 1), task("c", &["x.py"], 1), task("d", &[], 1));
        let groups = conflict_free(&[&a, &b, &c, &d]);
        let names: Vec<Vec<&str>> = groups.iter().map(|g| g.iter().map(|t| t.worker.as_str()).collect()).collect();
        assert_eq!(names, vec![vec!["a", "b", "d"], vec!["c"]]);
    }

    #[test]
    fn tracks_and_attributes_file_changes() {
        let tmp = TempDir::new("snap");
        std::fs::write(tmp.0.join("keep.txt"), "same").unwrap();
        std::fs::write(tmp.0.join("gone.txt"), "x").unwrap();
        std::fs::create_dir_all(tmp.0.join(".git")).unwrap();
        let before = snapshot(&tmp.0);
        std::fs::remove_file(tmp.0.join("gone.txt")).unwrap();
        std::fs::write(tmp.0.join("new.py"), "print(1)").unwrap();
        std::fs::write(tmp.0.join(".git/ignored"), "x").unwrap();
        let files = changed(&before, &snapshot(&tmp.0));
        assert_eq!(files, ["gone.txt", "new.py"]);
        let owner = task("builder", &["new.py"], 1);
        let (summary, unowned) = attribute(&files, &[&owner]);
        assert_eq!(summary, "gone.txt (no owner) · new.py (builder)");
        assert_eq!(unowned, ["gone.txt"]);
    }

    #[test]
    fn decodes_agent_events() {
        let claude = r#"{"type":"result","is_error":false,"result":"Hi","total_cost_usd":0.1,"usage":{"input_tokens":2,"cache_creation_input_tokens":10,"cache_read_input_tokens":100,"output_tokens":7}}"#;
        assert_eq!(parse_event(claude), Event::Result("Hi".into(), false, Usage { input: 112, output: 7, cost: 0.1 }));
        let step = r#"{"type":"step_finish","part":{"cost":0,"tokens":{"input":17118,"output":38,"reasoning":50,"cache":{"read":5,"write":0}}}}"#;
        assert_eq!(parse_event(step), Event::Usage(Usage { input: 17123, output: 88, cost: 0.0 }, false));
        let stop = r#"{"type":"step_finish","part":{"reason":"stop","tokens":{}}}"#;
        assert!(matches!(parse_event(stop), Event::Usage(_, true)));
        let text = r#"{"type":"text","part":{"text":"Done.\nAll good."}}"#;
        assert_eq!(parse_event(text), Event::Lines(vec!["Done.".into(), "All good.".into()]));
        let tool = r#"{"type":"tool_use","part":{"tool":"write","state":{"status":"completed","input":{"path":"hi.txt"}}}}"#;
        assert_eq!(parse_event(tool), Event::Lines(vec!["⚙ Write hi.txt".into()]));
        let failed = r#"{"type":"tool_use","part":{"tool":"bash","state":{"status":"error","input":{"command":"pytest"},"error":"boom"}}}"#;
        assert_eq!(parse_event(failed), Event::Lines(vec!["⚙ Bash pytest".into(), "Error: boom".into()]));
        assert_eq!(parse_event(r#"{"type":"step_start"}"#), Event::Lines(vec![]));
        assert_eq!(parse_event("plain text"), Event::Lines(vec!["plain text".into()]));
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
        assert_eq!(model_of(&worker_command("opencode/x")), Some("opencode/x"));
        assert_eq!(model_of("x --model=opus {prompt}"), Some("opus"));
        assert_eq!(with_model(WORKER_COMMAND, "opencode/y-free"), worker_command("opencode/y-free"));
        assert_eq!(with_model(WORKER_COMMAND, ""), WORKER_COMMAND);
        // opencode workers get an edit permission limited to their own files
        let owned = own_files(WORKER_COMMAND, &["a.py".into()], Path::new("/w"));
        let argv = build_argv(&owned, "p").unwrap();
        assert_eq!(argv[0], "env");
        let config: Value = serde_json::from_str(argv[1].strip_prefix("OPENCODE_CONFIG_CONTENT=").unwrap()).unwrap();
        assert_eq!(config["permission"]["edit"], serde_json::json!({"*": "deny", "a.py": "allow", "/w/a.py": "allow"}));
        assert_eq!(own_files("claude -p {prompt}", &[], Path::new("/w")), "claude -p {prompt}");
        assert_eq!(model_of(MANAGER_COMMAND), None);
    }

    /// A CLI that prints its answer and then hangs is killed once it goes quiet, output kept.
    #[tokio::test]
    async fn kills_quiet_command() {
        let dir = std::env::temp_dir();
        let start = std::time::Instant::now();
        let (ok, out, _) = run_command("sh -c 'echo answer; sleep 5' sh {prompt}", "p", &dir, Some(Duration::from_millis(300)), None, |_| (), |_| ()).await;
        assert!(!ok && out.starts_with("answer\n") && out.contains("no output for"));
        assert!(start.elapsed() < Duration::from_secs(2));
        let (ok, out, _) = run_command("echo fine {prompt}", "p", &dir, Some(Duration::from_secs(5)), None, |_| (), |_| ()).await;
        assert!(ok && out == "fine p\n");
    }

    /// A JSON-speaking agent: its usage is summed and its final result becomes the output.
    #[tokio::test]
    async fn counts_tokens() {
        let script = r#"echo '{"type":"step_finish","part":{"tokens":{"input":10,"output":2}}}'; echo '{"type":"result","is_error":false,"result":"ok","usage":{"input_tokens":5,"output_tokens":1}}'"#;
        let command = format!("sh -c {} sh {{prompt}}", shlex::try_quote(script).unwrap());
        let seen = Mutex::new(0);
        let (ok, out, usage) = run_command(&command, "p", &std::env::temp_dir(), None, None, |_| (), |u| *seen.lock().unwrap() += u.input).await;
        assert!(ok && out == "ok");
        assert_eq!((usage.input, usage.output, *seen.lock().unwrap()), (15, 3, 15));
    }

    /// A worker past its token budget is stopped and says why.
    #[tokio::test]
    async fn stops_worker_over_token_budget() {
        let script = r#"echo '{"type":"step_finish","part":{"tokens":{"input":600,"output":2}}}'; sleep 5; echo late"#;
        let command = format!("sh -c {} sh {{prompt}}", shlex::try_quote(script).unwrap());
        let started = std::time::Instant::now();
        let (ok, out, _) = run_command(&command, "p", &std::env::temp_dir(), None, Some(500), |_| (), |_| ()).await;
        assert!(!ok && out.contains("worker_max_tokens") && !out.contains("late"));
        assert!(started.elapsed() < Duration::from_secs(3), "killed right away");
    }

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\x1b[0m> plan \x1b[1;32mPONG\x1b[0m\r"), "> plan PONG");
    }

    #[test]
    fn prompts_carry_history_ownership_and_reports() {
        let p = manager_prompt("fix it", "User: hi\nManager: hello", &[]);
        assert!(
            p.contains("User: hi") && p.contains("create") && p.contains("do not do") && p.contains("never own the same file") && p.contains("as many workers as the job needs")
        );
        assert!(review_prompt("g", "r", false, &[]).contains("check the actual files"));
        assert!(manager_prompt("x", "", &["opencode/a-free".into()]).contains("- opencode/a-free: "));
        let run = ModelRun { runs: 4, finished: 3, tokens: 400_000 };
        assert_eq!(run.describe(), "4 workers, 3 finished, 1 failed or stopped, about 100k tokens each");
        assert!(worker_prompt(&task("b", &[], 1), "g", "").contains("shell tool"));
        assert!(worker_prompt(&task("b", &[], 1), "g", "").contains(" ask b "));
        let w = worker_prompt(&task("b", &["calc.py"], 1), "g", "");
        assert!(w.contains("Files you own: calc.py") && w.contains("report to the manager"));
        assert!(worker_prompt(&task("t", &[], 2), "g", "").contains("own no files"));
    }

    /// Three 0.3s workers with one slot must take about 0.9s; with three slots about 0.3s.
    #[tokio::test]
    async fn caps_parallel_workers() {
        let plan = vec![task("a", &[], 1), task("b", &[], 1), task("c", &[], 1)];
        for (slots, min_ms, max_ms) in [(1, 850, 2_000), (3, 250, 800)] {
            let mut crew = crew(String::new(), "sh -c 'sleep 0.3' sh {prompt}", 1);
            crew.max_parallel = slots;
            let (tx, _rx) = unbounded_channel();
            let start = std::time::Instant::now();
            run_plan(&crew, "g", &plan, &std::env::temp_dir(), &mut String::new(), &tx).await;
            let ms = start.elapsed().as_millis();
            assert!((min_ms..max_ms).contains(&ms), "{slots} slot(s) took {ms}ms");
        }
    }

    /// Aborting a job (Ctrl+S) must kill its workers before they finish.
    #[tokio::test]
    async fn abort_kills_workers() {
        let tmp = TempDir::new("abort");
        let crew = crew(String::new(), "sh -c 'sleep 0.6; touch marker' sh {prompt}", 1);
        let plan = vec![task("w", &[], 1)];
        let (tx, _rx) = unbounded_channel();
        let dir = tmp.0.clone();
        let job = tokio::spawn(async move { run_plan(&crew, "g", &plan, &dir, &mut String::new(), &tx).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        job.abort();
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(!tmp.0.join("marker").exists(), "worker kept running after abort");
    }

    /// Fake manager: invents coder (owning out.txt), then on review a checker for round 2, then
    /// says done (only if the checker saw round 1's report and foreman's file record).
    async fn run_crew(max_rounds: u32, goal: &str) -> (Vec<Msg>, TempDir) {
        let script = r##"case "$1" in
            *"hello"*) echo 'Hi there!' ;;
            *"no prose"*) echo 'On it.'; echo '[{"worker": "coder", "role": "dev", "task": "write it", "files": ["out.txt"]}]' ;;
            *"# Round 2"*"check it"*"write it"*"out.txt (coder)"*) echo 'ALL DONE' ;;
            *"# Round 2"*) echo 'NO HISTORY' ;;
            *) echo 'Needs a check.'; echo '[{"worker": "checker", "role": "qa", "task": "check it"}]' ;;
            esac"##;
        let manager = format!("sh -c {} sh {{prompt}}", shlex::try_quote(script).unwrap());
        // workers echo their prompt and write out.txt, so file tracking has something to see
        let crew = crew(manager, "sh -c 'echo DID \"$1\" | tee out.txt' sh {prompt}", max_rounds);
        let tmp = TempDir::new(&format!("crew-{max_rounds}-{}", goal.len()));
        let (tx, mut rx) = unbounded_channel();
        orchestrate_in(crew, goal.into(), "User: earlier\nManager: ok".into(), &[], &Mutex::new(vec![]), &tmp.0, tx).await;
        let mut msgs = vec![];
        while let Ok(m) = rx.try_recv() {
            msgs.push(m);
        }
        (msgs, tmp)
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

    /// A worker asks mid-task over the socket and gets the (fake) manager's answer back.
    #[tokio::test]
    async fn workers_can_ask_the_manager() {
        let crew = crew("sh -c 'echo \"use port 8080\"' sh {prompt}".into(), "true", 1);
        let (tx, mut rx) = unbounded_channel();
        let _l = listen_for_questions(&crew, "g", Arc::new(Mutex::new(vec![])), &tx).unwrap();
        let _s = SocketFile(ask_socket());
        unsafe { std::env::set_var("FOREMAN_SOCK", ask_socket()) };
        let answer = tokio::task::spawn_blocking(|| ask_client("builder", "which port?")).await.unwrap().unwrap();
        assert_eq!(answer, "use port 8080");
        let said: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).filter_map(|m| if let Msg::Said(f, t, x) = m { Some((f, t, x)) } else { None }).collect();
        assert_eq!(said[0], ("builder".into(), "manager".into(), "which port?".into()));
        assert_eq!(said[1], ("manager".into(), "builder".into(), "use port 8080".into()));
    }

    #[tokio::test]
    async fn replans_until_done() {
        let (msgs, _tmp) = run_crew(3, "ship it").await;
        let chat = chat(&msgs);
        assert!(chat.contains("On it.") && !chat.contains("\"worker\""), "prose shown, JSON hidden");
        assert!(chat.contains("files changed: out.txt (coder)"), "{chat}");
        assert!(chat.contains("files changed: out.txt (no owner)"), "the checker owned nothing but wrote anyway: {chat}");
        assert!(chat.contains("ALL DONE"), "round 2 saw round 1: {chat}");
        assert!(chat.contains("Done after 2 rounds"));
        let w = workers(&msgs);
        assert!(w.contains(&("coder", State::Done)) && w.contains(&("checker", State::Done)));
        assert!(msgs.iter().any(|m| matches!(m, Msg::Output(n, l) if n == "coder" && l.starts_with("DID"))));
        assert!(matches!(msgs.last(), Some(Msg::Finished)));
        assert!(msgs.iter().any(|m| matches!(m, Msg::Remember(r) if r.contains("ALL DONE"))));
    }

    #[tokio::test]
    async fn stops_at_round_cap() {
        let (msgs, _tmp) = run_crew(1, "ship it").await;
        assert!(!workers(&msgs).iter().any(|(n, _)| *n == "checker"));
        assert!(chat(&msgs).contains("Not finished: stopped after 1 rounds"), "work was left, so not done");
    }

    #[tokio::test]
    async fn answers_chat_without_workers() {
        let (msgs, _tmp) = run_crew(3, "hello").await;
        assert!(chat(&msgs).contains("Hi there!"));
        assert!(workers(&msgs).is_empty());
        assert!(msgs.iter().any(|m| matches!(m, Msg::Remember(r) if r.contains("Hi there!"))));
    }
}
