//! foreman: a terminal boss for a crew of AI agents.
//!
//! You only talk to the manager (Claude). It doesn't do the work itself: it
//! creates workers as needed, sends each one its orders, and reviews their
//! replies, re-planning until the goal is met.
//!
//! The UI is a team chat in Claude Code's style: `you → manager`,
//! `manager → builder` (the orders), `builder → manager` (the reply). Tabs
//! switch between that chat, an overview of all workers, and one tab per
//! worker with its own conversation and work log.

mod crew;

use crew::{Crew, Kind, Msg, State, Task, Usage};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell as TableCell, Clear, List, ListItem, ListState, Padding, Paragraph, Row, Table, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

struct Theme {
    name: &'static str,
    accent: Color, // brand and the manager's name
    text: Color,
    dim: Color,
    faint: Color, // borders and rules
    good: Color,
    warn: Color,
    bad: Color,
    info: Color,
    violet: Color,
    bubble: Color,    // chat bubbles from the manager and workers
    mine: Color,      // your own chat bubbles
    on_accent: Color, // text on an accent-coloured pill
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

#[rustfmt::skip]
const THEMES: [Theme; 3] = [
    Theme { name: "claude", accent: rgb(0xd77757), text: rgb(0xe6e6e6), dim: rgb(0x999999), faint: rgb(0x505050),
            good: rgb(0x4eba65), warn: rgb(0xffc107), bad: rgb(0xff6b80), info: rgb(0xb1b9f9), violet: rgb(0xd0a8ff),
            bubble: rgb(0x2a2a2a), mine: rgb(0x1e4033), on_accent: rgb(0x1a1a1a) },
    Theme { name: "tokyo-night", accent: rgb(0xff9e64), text: rgb(0xc0caf5), dim: rgb(0x737aa2), faint: rgb(0x3b4261),
            good: rgb(0x9ece6a), warn: rgb(0xe0af68), bad: rgb(0xf7768e), info: rgb(0x7dcfff), violet: rgb(0xbb9af7),
            bubble: rgb(0x24283b), mine: rgb(0x1f3b36), on_accent: rgb(0x1a1b26) },
    Theme { name: "catppuccin", accent: rgb(0xfab387), text: rgb(0xcdd6f4), dim: rgb(0x7f849c), faint: rgb(0x45475a),
            good: rgb(0xa6e3a1), warn: rgb(0xf9e2af), bad: rgb(0xf38ba8), info: rgb(0x89dceb), violet: rgb(0xcba6f7),
            bubble: rgb(0x313244), mine: rgb(0x264035), on_accent: rgb(0x1e1e2e) },
];

/// Claude Code's spinner glyphs.
const SPINNER: [&str; 6] = ["·", "✢", "✳", "∗", "✻", "✽"];
const MAX_OUTPUT_LINES: usize = 5_000;
/// Fixed tabs before the per-worker ones.
const MANAGER_TAB: usize = 0;
const OVERVIEW_TAB: usize = 1;
const INFO_TAB: usize = 2;
const FIRST_WORKER_TAB: usize = 3;

/// Something on screen the mouse can click.
#[derive(Clone, Copy)]
enum Click {
    Tab(usize),
    Stop,
    Models(bool), // true = the manager's
    Theme,
    Quit,
    Send,
    Pick(usize), // a row of the model picker
    Picker,      // anywhere else inside the picker
}

/// A model picker for the manager (Claude models) or the workers (opencode models).
struct Picker {
    for_manager: bool,
    models: Vec<(String, &'static str)>, // (name, tag)
    sel: usize,
    offset: Cell<usize>, // first row the list showed, to map clicks to rows
}

/// One message or note in the team chat.
enum Entry {
    Line(Kind, String),
    /// The manager's orders to a worker, with that worker's latest state (None = queued).
    Task(Task, Option<State>),
    /// A message between the manager and a worker: a worker's report (the end of its
    /// output), or a question and answer while it works.
    Said {
        from: String,
        to: String,
        text: String,
    },
}

/// One worker of the current job, as the UI tracks it.
struct Worker {
    name: String,
    role: String,
    step: i64,
    state: Option<State>, // None while queued behind an earlier step
    started: Instant,
    took: Option<Duration>,
    output: Vec<String>,
    usage: Usage,
    scroll_back: u16,
    model: String, // the model the manager picked; empty = the default
    files: Vec<String>,
}

impl Worker {
    fn elapsed(&self) -> Option<Duration> {
        self.state.map(|_| self.took.unwrap_or_else(|| self.started.elapsed()))
    }
}

struct App {
    crew: Crew,
    chat: Vec<Entry>,
    times: Vec<String>,                // when each chat entry was posted, "14:05"
    tz: i64,                           // local UTC offset in seconds, for the clock
    hits: RefCell<Vec<(Rect, Click)>>, // what the last frame drew where, for mouse clicks
    workers: Vec<Worker>,
    tab: usize, // MANAGER_TAB, OVERVIEW_TAB, INFO_TAB, or FIRST_WORKER_TAB + worker index
    manager: Option<State>,
    manager_usage: Usage, // the manager's tokens this session
    scroll_back: u16,     // lines the chat is scrolled up from the bottom
    input: String,
    cursor: usize, // in chars
    busy: bool,
    job: Option<JoinHandle<()>>,
    job_started: Instant,
    toast: Option<(String, Instant)>,
    picker: Option<Picker>,
    history: String, // past user/manager exchanges, fed back to the manager
    theme: usize,
    cwd: String,
    started: Instant,
    tx: UnboundedSender<Msg>,
    rt: tokio::runtime::Runtime,
    quit: bool,
}

impl App {
    fn t(&self) -> &'static Theme {
        &THEMES[self.theme]
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }

    fn spinner(&self) -> &'static str {
        SPINNER[(self.started.elapsed().as_millis() / 120) as usize % SPINNER.len()]
    }

    /// Add to the chat, stamped with the time.
    fn post(&mut self, entry: Entry) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64) + self.tz;
        self.times.push(format!("{:02}:{:02}", now.rem_euclid(86_400) / 3_600, now.rem_euclid(3_600) / 60));
        self.chat.push(entry);
    }

    fn click(&self, area: Rect, what: Click) {
        self.hits.borrow_mut().push((area, what));
    }

    fn worker(&self, name: &str) -> Option<&Worker> {
        self.workers.iter().find(|w| w.name == name)
    }

    fn worker_mut(&mut self, name: &str) -> Option<&mut Worker> {
        self.workers.iter_mut().find(|w| w.name == name)
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Log(kind, text) => self.post(Entry::Line(kind, text)),
            Msg::Manager(state) => self.manager = Some(state),
            Msg::Assign(task) => {
                self.post(Entry::Task(task.clone(), None));
                match self.worker_mut(&task.worker) {
                    // the manager reused a name in a later round: same tab, new orders
                    Some(w) => {
                        w.output.push(format!("── new task as {} ──", task.role));
                        (w.role, w.step, w.state, w.took, w.model, w.files) = (task.role, task.step, None, None, task.model, task.files);
                    }
                    None => self.workers.push(Worker {
                        name: task.worker,
                        role: task.role,
                        step: task.step,
                        state: None,
                        started: Instant::now(),
                        took: None,
                        output: Vec::new(),
                        usage: Usage::default(),
                        scroll_back: 0,
                        model: task.model,
                        files: task.files,
                    }),
                }
            }
            Msg::Worker(name, state) => {
                // update the worker's latest orders in the chat
                if let Some(Entry::Task(_, s)) = self.chat.iter_mut().rev().find(|e| matches!(e, Entry::Task(t, _) if t.worker == name)) {
                    *s = Some(state);
                }
                let Some(w) = self.worker_mut(&name) else {
                    return;
                };
                if state == State::Working {
                    (w.started, w.took, w.state) = (Instant::now(), None, Some(state));
                    return;
                }
                (w.took, w.state) = (Some(w.started.elapsed()), Some(state));
                let text = w.output[reply_start(&w.output)..].join("\n").trim().to_string();
                if !text.is_empty() {
                    self.post(Entry::Said { from: name, to: "manager".into(), text });
                }
            }
            Msg::Output(name, line) => {
                if let Some(w) = self.worker_mut(&name) {
                    w.output.push(line);
                    if w.output.len() > MAX_OUTPUT_LINES {
                        w.output.drain(..1_000);
                    }
                }
            }
            Msg::Usage(name, u) if name == "manager" => self.manager_usage += u,
            Msg::Usage(name, u) => {
                if let Some(w) = self.worker_mut(&name) {
                    w.usage += u;
                }
            }
            Msg::Notify(text) => self.toast(text),
            Msg::Said(from, to, text) => self.post(Entry::Said { from, to, text }),
            Msg::Remember(text) => {
                self.history.push_str(&text);
                self.history.push_str("\n\n");
                // ponytail: keep the last ~6k chars; summarise old turns if the manager needs longer memory
                let cut = self.history.len().saturating_sub(6_000);
                let cut = (cut..self.history.len()).find(|&i| self.history.is_char_boundary(i)).unwrap_or(0);
                self.history.drain(..cut);
            }
            Msg::Finished => (self.busy, self.job) = (false, None),
        }
    }

    // ---- input ----------------------------------------------------------------
    fn key(&mut self, k: KeyEvent) {
        if self.picker.is_some() {
            return self.picker_key(k);
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.input.chars().count();
        let tabs = FIRST_WORKER_TAB + self.workers.len();
        match k.code {
            KeyCode::Char('q' | 'c') if ctrl => self.quit = true,
            KeyCode::Char('o') if ctrl => self.open_picker(true),
            KeyCode::Char('w') if ctrl => self.open_picker(false),
            KeyCode::Char('s') if ctrl => self.stop(),
            KeyCode::Char('t') if ctrl => {
                self.theme = (self.theme + 1) % THEMES.len();
                self.toast(format!("theme: {}", self.t().name));
            }
            KeyCode::Char('u') if ctrl => (self.input, self.cursor) = (String::new(), 0),
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Tab => self.tab = (self.tab + 1) % tabs,
            KeyCode::BackTab => self.tab = (self.tab + tabs - 1) % tabs,
            KeyCode::Up => self.scroll(1),
            KeyCode::Down => self.scroll(-1),
            KeyCode::PageUp => self.scroll(10),
            KeyCode::PageDown => self.scroll(-10),
            KeyCode::Esc => self.scroll(i32::MIN),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.input.remove(self.byte_at(self.cursor));
            }
            KeyCode::Delete if self.cursor < len => _ = self.input.remove(self.byte_at(self.cursor)),
            KeyCode::Char(c) if !ctrl => self.insert(&c.to_string()),
            _ => {}
        }
    }

    /// Scroll the current tab up (positive) or down; i32::MIN jumps back to the bottom.
    fn scroll(&mut self, by: i32) {
        let target = match self.tab {
            MANAGER_TAB => &mut self.scroll_back,
            OVERVIEW_TAB | INFO_TAB => return,
            i => &mut self.workers[i - FIRST_WORKER_TAB].scroll_back,
        };
        *target = if by == i32::MIN { 0 } else { (*target as i32 + by).clamp(0, u16::MAX as i32) as u16 };
    }

    fn mouse(&mut self, m: MouseEvent) {
        let hit = self.hits.borrow().iter().rev().find(|(r, _)| r.contains(Position::new(m.column, m.row))).map(|(_, c)| *c);
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                match self.picker.as_mut() {
                    Some(p) if up => p.sel = p.sel.saturating_sub(1),
                    Some(p) => p.sel = (p.sel + 1).min(p.models.len() - 1),
                    None => self.scroll(if up { 3 } else { -3 }),
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.picker.is_some() => match hit {
                Some(Click::Pick(i)) => {
                    self.picker.as_mut().unwrap().sel = i;
                    self.pick();
                }
                Some(Click::Picker) => {}
                _ => self.picker = None, // a click outside closes it
            },
            MouseEventKind::Down(MouseButton::Left) => match hit {
                Some(Click::Tab(i)) if i < FIRST_WORKER_TAB + self.workers.len() => self.tab = i,
                Some(Click::Stop) => self.stop(),
                Some(Click::Models(manager)) => self.open_picker(manager),
                Some(Click::Theme) => {
                    self.theme = (self.theme + 1) % THEMES.len();
                    self.toast(format!("theme: {}", self.t().name));
                }
                Some(Click::Quit) => self.quit = true,
                Some(Click::Send) => self.submit(),
                _ => {}
            },
            _ => {}
        }
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.input.char_indices().nth(char_idx).map_or(self.input.len(), |(i, _)| i)
    }

    /// Insert at the cursor; pasted line breaks become spaces so a paste never sends early.
    fn insert(&mut self, text: &str) {
        let text: String = text.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).filter(|c| !c.is_control()).collect();
        let at = self.byte_at(self.cursor);
        self.input.insert_str(at, &text);
        self.cursor += text.chars().count();
    }

    fn submit(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.busy {
            return self.toast("The manager is still working (ctrl+s stops it)");
        }
        self.workers.clear();
        (self.tab, self.scroll_back) = (MANAGER_TAB, 0);
        self.post(Entry::Line(Kind::User, text.clone()));
        self.job = Some(self.rt.spawn(crew::orchestrate(self.crew.clone(), text, self.history.clone(), self.tx.clone())));
        (self.busy, self.job_started) = (true, Instant::now());
        (self.input, self.cursor) = (String::new(), 0);
    }

    /// Abort the job; dropping it kills the manager and any running workers.
    fn stop(&mut self) {
        let Some(job) = self.job.take() else { return };
        job.abort();
        self.busy = false;
        self.manager = None;
        for w in self.workers.iter_mut().filter(|w| w.state == Some(State::Working)) {
            (w.state, w.took) = (Some(State::Failed), Some(w.started.elapsed()));
            w.output.push("── stopped ──".into());
        }
        for entry in &mut self.chat {
            if let Entry::Task(_, s @ Some(State::Working)) = entry {
                *s = Some(State::Failed);
            }
        }
        self.post(Entry::Line(Kind::Warn, "Interrupted by you. Running workers were stopped.".into()));
    }

    fn open_picker(&mut self, for_manager: bool) {
        if self.busy {
            return self.toast("Wait until the manager is idle");
        }
        let models = if for_manager { Ok(crew::CLAUDE_MODELS.iter().map(|m| (m.to_string(), "claude")).collect()) } else { crew::opencode_models() };
        match models {
            Ok(models) => {
                let current = if for_manager { &self.crew.manager } else { &self.crew.worker };
                let current = crew::model_of(current).unwrap_or_default();
                let sel = models.iter().position(|(m, _)| m == current).unwrap_or(0);
                self.picker = Some(Picker { for_manager, models, sel, offset: Cell::new(0) });
            }
            Err(e) => self.toast(e),
        }
    }

    fn picker_key(&mut self, k: KeyEvent) {
        let p = self.picker.as_mut().unwrap();
        let n = p.models.len();
        match k.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Down | KeyCode::Tab => p.sel = (p.sel + 1) % n,
            KeyCode::Up | KeyCode::BackTab => p.sel = (p.sel + n - 1) % n,
            KeyCode::PageDown => p.sel = (p.sel + 10).min(n - 1),
            KeyCode::PageUp => p.sel = p.sel.saturating_sub(10),
            KeyCode::Enter => self.pick(),
            _ => {}
        }
    }

    /// Use the picker's selected model.
    fn pick(&mut self) {
        let Some(p) = self.picker.take() else { return };
        let (model, for_manager) = (p.models[p.sel].0.clone(), p.for_manager);
        // ponytail: rewrites the whole command, dropping custom flags; edit crew.json for those
        if for_manager {
            self.crew.manager = crew::manager_command(&model);
        } else {
            self.crew.worker = crew::worker_command(&model);
        }
        if let Err(e) = self.crew.save() {
            return self.toast(format!("Could not save: {e}"));
        }
        self.toast(format!("{} now use {model}", if for_manager { "The manager will" } else { "Workers will" }));
    }

    // ---- drawing --------------------------------------------------------------
    fn draw(&self, f: &mut Frame) {
        self.hits.borrow_mut().clear();
        let [tabs, _, body, status, input, footer] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(5), Constraint::Length(1), Constraint::Length(3), Constraint::Length(1)])
                .areas(f.area());
        self.draw_tabs(f, tabs);
        let body = body.inner(Margin::new(1, 0));
        match self.tab {
            MANAGER_TAB if self.chat.is_empty() && !self.busy => self.draw_welcome(f, body),
            MANAGER_TAB => self.render_chat(f, self.team_chat(body.width), body),
            OVERVIEW_TAB => self.draw_overview(f, body),
            INFO_TAB => self.draw_info(f, body),
            i => self.draw_worker_tab(f, body, i - FIRST_WORKER_TAB),
        }
        self.draw_status(f, status.inner(Margin::new(1, 0)));
        self.draw_input(f, input);
        self.draw_footer(f, footer.inner(Margin::new(2, 0)));
        if let Some(p) = &self.picker {
            self.draw_picker(f, p);
        }
    }

    /// A worker's name colour: stable per name, so it reads the same in every message and tab.
    fn color_of(&self, name: &str) -> Color {
        let t = self.t();
        let palette = [t.info, t.good, t.violet, t.warn];
        palette[name.bytes().map(usize::from).sum::<usize>() % palette.len()]
    }

    fn name(&self, who: &str) -> Span<'static> {
        let t = self.t();
        let color = match who {
            "you" => t.text,
            "manager" => t.accent,
            worker => self.color_of(worker),
        };
        Span::styled(who.to_string(), Style::new().fg(color).bold())
    }

    /// A chat bubble, WhatsApp style: yours on the right in green, the manager's and workers'
    /// on the left with the sender's name on top. `note` is an extra line in the info colour,
    /// `foot` sits before the time in the bottom corner.
    /// `fold` > 0 folds the text to that many lines, for the team chat; the worker's tab has it all.
    #[allow(clippy::too_many_arguments)]
    fn bubble(&self, from: &str, to: &str, text: &str, md: bool, note: Option<String>, foot: Vec<Span<'static>>, time: &str, width: u16, fold: usize) -> Vec<Line<'static>> {
        let t = self.t();
        let mine = from == "you";
        let bg = if mine { t.mine } else { t.bubble };
        let room = ((width as usize) * 4 / 5).max(24) - 4;
        let mut rows: Vec<Line<'static>> = vec![];
        if !mine {
            rows.push(Line::from(vec![self.name(from), Span::styled(format!(" → {to}"), Style::new().fg(t.dim))]));
        }
        let mut in_code = false;
        let mut body: Vec<Line<'static>> =
            wrap(text, room).into_iter().filter_map(|l| if md { markdown(&l, &mut in_code, t) } else { Some(Line::styled(l, Style::new().fg(t.text))) }).collect();
        if fold > 0 && body.len() > fold + 1 {
            let more = body.len() - fold;
            body.truncate(fold);
            body.push(Line::styled(format!("… {more} more lines · click to read"), Style::new().fg(t.dim).italic()));
        }
        rows.extend(body);
        if let Some(note) = note {
            rows.extend(wrap(&note, room).into_iter().map(|l| Line::styled(l, Style::new().fg(t.info))));
        }
        let mut foot = foot;
        foot.push(Span::styled(format!("  {time}"), Style::new().fg(t.dim)));
        if mine {
            foot.push(Span::styled(" ✓✓", Style::new().fg(t.info)));
        }
        let foot = Line::from(foot);
        let inner = rows.iter().map(Line::width).chain([foot.width()]).max().unwrap_or(0).min(room);
        let pad = if mine { (width as usize).saturating_sub(inner + 5) } else { 0 };
        let edge = |l: &str, mid: &str, r: &str| Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(format!("{l}{}{r}", mid.repeat(inner + 2)), Style::new().fg(bg))]);
        let fill = |line: Line<'static>, right: bool| {
            let gap = Span::styled(" ".repeat(inner.saturating_sub(line.width())), Style::new().bg(bg));
            let content = line.spans.into_iter().map(|s| {
                let style = s.style.bg(bg);
                s.style(style)
            });
            let mut spans = vec![Span::raw(" ".repeat(pad)), Span::styled(" ", Style::new().bg(bg))];
            if right {
                spans.push(gap.clone());
            }
            spans.extend(content);
            if !right {
                spans.push(gap);
            }
            spans.push(Span::styled(" ", Style::new().bg(bg)));
            Line::from(spans)
        };
        let mut out = vec![Line::default(), edge("▗", "▄", "▖")];
        out.extend(rows.into_iter().map(|l| fill(l, false)));
        out.push(fill(foot, true));
        out.push(edge("▝", "▀", "▘"));
        out
    }

    /// A centred note in the chat, like WhatsApp's date chips: rounds, file changes, warnings.
    fn chip(&self, text: &str, color: Color, width: u16) -> Vec<Line<'static>> {
        let t = self.t();
        wrap(text, (width as usize).saturating_sub(8))
            .into_iter()
            .map(|l| Line::from(Span::styled(format!(" {} ", l.trim()), Style::new().fg(color).bg(t.bubble))).centered())
            .collect()
    }

    /// Where a worker stands, for the corner of its orders: `◼ working 12s · 3.1k tokens`.
    fn status_chip(&self, task: &Task, state: Option<State>) -> Vec<Span<'static>> {
        let t = self.t();
        let worker = self.worker(&task.worker);
        let time = worker.and_then(Worker::elapsed).map(fmt_duration).unwrap_or_default();
        let tokens = worker.map(|w| tokens(w.usage)).filter(|t| !t.is_empty()).map(|t| format!(" · {t}")).unwrap_or_default();
        let (text, color) = match state {
            None => (format!("○ queued for step {}", task.step), t.dim),
            Some(State::Working) => (format!("◼ working {time}{tokens}"), t.warn),
            Some(State::Done) => (format!("✓ done in {time}{tokens}"), t.good),
            Some(State::Failed) => (format!("✗ failed{tokens}"), t.bad),
        };
        vec![Span::styled(text, Style::new().fg(color))]
    }

    /// The manager's orders to a worker, as a bubble.
    fn task_bubble(&self, task: &Task, state: Option<State>, time: &str, width: u16, fold: usize) -> Vec<Line<'static>> {
        let files = if task.files.is_empty() { "owns no files (read and run only)".to_string() } else { format!("owns {}", task.files.join(", ")) };
        let model = if task.model.is_empty() { String::new() } else { format!(" · model {}", task.model) };
        let to = format!("{} · {}", task.worker, task.role);
        self.bubble("manager", &to, &task.task, false, Some(format!("{files}{model}")), self.status_chip(task, state), time, width, fold)
    }

    /// The team chat on the manager tab: bubbles, with notes centred between them. Each line
    /// carries what clicking it opens: a worker's bubble opens that worker's tab.
    fn team_chat(&self, width: u16) -> Vec<(Line<'static>, Option<Click>)> {
        let t = self.t();
        let mut lines = vec![];
        let tab_of = |name: &str| self.workers.iter().position(|w| w.name == name).map(|i| Click::Tab(FIRST_WORKER_TAB + i));
        let mut add = |new: Vec<Line<'static>>, click: Option<Click>| lines.extend(new.into_iter().map(|l| (l, click)));
        let mut i = 0;
        while i < self.chat.len() {
            let time = self.times.get(i).map_or("", String::as_str);
            match &self.chat[i] {
                Entry::Line(Kind::User, text) => add(self.bubble("you", "manager", text, false, None, vec![], time, width, 0), None),
                Entry::Line(Kind::Out, _) => {
                    // the manager's consecutive lines are one message
                    let mut text = vec![];
                    while let Some(Entry::Line(Kind::Out, l)) = self.chat.get(i) {
                        text.push(l.as_str());
                        i += 1;
                    }
                    add(self.bubble("manager", "you", &text.join("\n"), true, None, vec![], time, width, 0), None);
                    continue;
                }
                Entry::Line(Kind::Rule, text) => {
                    add(vec![Line::default()], None);
                    add(self.chip(text, t.accent, width), None);
                }
                Entry::Task(task, state) => add(self.task_bubble(task, *state, time, width, 4), tab_of(&task.worker)),
                Entry::Said { from, to, text } => {
                    let worker = if from == "manager" { to } else { from };
                    add(self.bubble(from, to, text, true, None, vec![], time, width, 6), tab_of(worker));
                }
                Entry::Line(kind, text) => {
                    let color = match kind {
                        Kind::Good => t.dim,
                        Kind::Warn => t.warn,
                        _ => t.bad,
                    };
                    add(vec![Line::default()], None);
                    add(self.chip(text, color, width), None);
                }
            }
            i += 1;
        }
        lines
    }

    /// The chat, pinned to the bottom unless scrolled up. Its lines never wrap, so each is one
    /// row and can be made clickable.
    fn render_chat(&self, f: &mut Frame, lines: Vec<(Line<'static>, Option<Click>)>, area: Rect) {
        let top = lines.len().saturating_sub(area.height as usize).saturating_sub(self.scroll_back as usize);
        for (row, (line, click)) in lines.into_iter().skip(top).take(area.height as usize).enumerate() {
            let at = Rect::new(area.x, area.y + row as u16, area.width, 1);
            if let Some(click) = click {
                self.click(at, click);
            }
            f.render_widget(line, at);
        }
    }

    fn draw_welcome(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let width = area.width.min(64);
        let [boxed, _, tips] = Layout::vertical([Constraint::Length(7), Constraint::Length(1), Constraint::Min(0)]).areas(area);
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(t.accent)).padding(Padding::horizontal(1));
        let welcome = vec![
            Line::from(vec![Span::styled("✻ ", Style::new().fg(t.accent)), Span::styled("Welcome to foreman!", Style::new().fg(t.text).bold())]),
            Line::default(),
            Line::styled("  Talk to the manager. It creates AI workers, sends each", Style::new().fg(t.dim).italic()),
            Line::styled("  one its orders, checks their replies, and reports back.", Style::new().fg(t.dim).italic()),
            Line::styled(format!("  cwd: {}", shorten(&self.cwd, width as usize - 12)), Style::new().fg(t.dim)),
        ];
        f.render_widget(Paragraph::new(welcome).block(block), Rect { width, ..boxed });
        let tip = |n: &str, text: &str| Line::from(vec![Span::styled(format!(" {n}. "), Style::new().fg(t.dim)), Span::styled(text.to_string(), Style::new().fg(t.dim))]);
        let lines = vec![
            Line::styled(" Tips for getting started:", Style::new().fg(t.dim)),
            Line::default(),
            tip("1", "Describe a goal; you'll see the manager give each worker its orders here"),
            tip("2", "Workers reply to the manager when they finish; it checks their work"),
            tip("3", "Click a tab (or press tab) to see all workers live, the Info page, or one worker's log"),
        ];
        f.render_widget(Paragraph::new(lines), tips);
    }

    /// `✻ foreman`, then the tabs as pills (the current one filled), and the folder on the right.
    /// Worker tabs that don't fit fold into a `+3` pill that opens the Workers overview.
    fn draw_tabs(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        f.render_widget(Block::new().style(Style::new().bg(t.bubble)), area);
        let brand = Span::styled(" ✻ foreman ", Style::new().fg(t.accent).bg(t.bubble).bold());
        let mut x = area.x + brand.width() as u16 + 1;
        let mut spans = vec![brand, Span::styled(" ", Style::new().bg(t.bubble))];
        let mut titles: Vec<(Option<Span>, String, Color)> =
            vec![(None, "Chat".into(), t.accent), (None, format!("Workers {}", self.workers.len()), t.accent), (None, "Info".into(), t.accent)];
        titles.extend(self.workers.iter().map(|w| (Some(self.dot(w.state)), w.name.clone(), self.color_of(&w.name))));
        let n = titles.len();
        for (i, (dot, name, color)) in titles.into_iter().enumerate() {
            let selected = i == self.tab;
            let style = if selected { Style::new().fg(t.on_accent).bg(color).bold() } else { Style::new().fg(t.dim).bg(t.faint) };
            let mut pill = vec![Span::styled(" ", style)];
            if let Some(dot) = dot {
                let s = if selected { style } else { dot.style.bg(t.faint) };
                pill.push(dot.style(s));
                pill.push(Span::styled(" ", style));
            }
            pill.push(Span::styled(format!("{name} "), style));
            let w = pill.iter().map(Span::width).sum::<usize>() as u16;
            // keep room for a "+N" pill when more workers follow
            if i >= FIRST_WORKER_TAB && x + w + 8 > area.right() && i + 1 < n {
                let more = Span::styled(format!(" +{} ", n - i), Style::new().fg(t.text).bg(t.faint).bold());
                self.click(Rect::new(x, area.y, more.width() as u16, 1), Click::Tab(OVERVIEW_TAB));
                spans.push(more);
                break;
            }
            self.click(Rect::new(x, area.y, w, 1), Click::Tab(i));
            x += w + 1;
            spans.extend(pill);
            spans.push(Span::styled(" ", Style::new().bg(t.bubble)));
        }
        let tabs = Line::from(spans);
        // the folder goes on the right only if it fits beside the tabs
        let room = (area.width as usize).saturating_sub(tabs.width() + 4);
        f.render_widget(tabs, area);
        if room > 12 {
            f.render_widget(Line::styled(format!("{} ", shorten(&self.cwd, room)), Style::new().fg(t.dim).bg(t.bubble)).right_aligned(), area);
        }
    }

    /// A worker's state as one steady glyph: queued, a spinner while working, done, or failed.
    fn dot(&self, state: Option<State>) -> Span<'static> {
        let t = self.t();
        match state {
            None => Span::styled("○", Style::new().fg(t.dim)),
            Some(State::Working) => Span::styled(self.spinner(), Style::new().fg(t.warn)),
            Some(State::Done) => Span::styled("✓", Style::new().fg(t.good)),
            Some(State::Failed) => Span::styled("✗", Style::new().fg(t.bad)),
        }
    }

    /// What's happening, at a glance: the phase, how many workers are in each state, the time, and Stop.
    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        if !self.busy {
            let last = self.chat.iter().rev().find_map(|e| match e {
                Entry::Line(Kind::Good | Kind::Warn, l) if l.starts_with("Done after") || l.starts_with("Not finished") => Some(l.as_str()),
                _ => None,
            });
            let text = last.map_or("● ready".to_string(), |l| format!("● ready · last job: {}", l.to_lowercase()));
            return f.render_widget(Span::styled(text, Style::new().fg(t.dim)), area);
        }
        let count = |s: Option<State>| self.workers.iter().filter(|w| w.state == s).count();
        let what = match (self.manager == Some(State::Working), self.workers.is_empty()) {
            (true, true) => "The manager is planning",
            (true, false) => "The manager is reviewing",
            _ => "Workers are on it",
        };
        let mut line =
            Line::from(vec![Span::styled(format!("{} ", self.spinner()), Style::new().fg(t.accent)), Span::styled(format!("{what}  "), Style::new().fg(t.accent).bold())]);
        for (n, label, color) in [
            (count(Some(State::Working)), "working", t.warn),
            (count(Some(State::Done)), "done", t.good),
            (count(None), "queued", t.dim),
            (count(Some(State::Failed)), "failed", t.bad),
        ] {
            if n > 0 {
                line.spans.push(Span::styled(format!(" {n} {label} "), Style::new().fg(color).bg(t.bubble)));
                line.spans.push(Span::raw(" "));
            }
        }
        line.spans.push(Span::styled(format!(" {}  ", fmt_duration(self.job_started.elapsed())), Style::new().fg(t.dim)));
        let stop = Span::styled(" ■ Stop ", Style::new().fg(t.on_accent).bg(t.bad).bold());
        self.click(Rect::new(area.x + line.width() as u16, area.y, stop.width() as u16, 1), Click::Stop);
        line.spans.push(stop);
        f.render_widget(line, area);
    }

    /// The message box: an accent border while you can send, and a Send button.
    fn draw_input(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let ready = !self.busy && self.picker.is_none();
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(if ready { t.accent } else { t.faint }));
        let inner = block.inner(area);
        let send =
            Span::styled(" ⏎ Send ", if ready && !self.input.trim().is_empty() { Style::new().fg(t.on_accent).bg(t.accent).bold() } else { Style::new().fg(t.dim).bg(t.bubble) });
        let [prompt, text, _, button] =
            Layout::horizontal([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1), Constraint::Length(send.width() as u16)]).areas(inner.inner(Margin::new(1, 0)));
        f.render_widget(block, area);
        f.render_widget(Span::styled("❯", Style::new().fg(if ready { t.accent } else { t.dim })), prompt);
        f.render_widget(send, button);
        self.click(button, Click::Send);
        if self.input.is_empty() {
            let hint = if self.busy { "The team is working. Wait, or press Stop" } else { "Message the manager, e.g. \"build a todo app in todo.py, with tests\"" };
            f.render_widget(Span::styled(hint, Style::new().fg(t.dim)), text);
        } else {
            // scroll horizontally so the cursor stays in view
            let width = text.width.saturating_sub(1) as usize;
            let skip = self.cursor.saturating_sub(width);
            let shown: String = self.input.chars().skip(skip).take(width + 1).collect();
            f.render_widget(Span::styled(shown, Style::new().fg(t.text)), text);
        }
        if self.picker.is_none() {
            let skip = self.cursor.saturating_sub(text.width.saturating_sub(1) as usize);
            f.set_cursor_position(Position::new(text.x + (self.cursor - skip) as u16, text.y));
        }
    }

    /// Clickable buttons on the left (with their keys), the models and tokens on the right (or a notice, briefly).
    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let mut buttons = vec![("Manager model", "^O", Click::Models(true)), ("Worker model", "^W", Click::Models(false)), ("Theme", "^T", Click::Theme)];
        if self.busy {
            buttons.insert(0, ("Stop", "^S", Click::Stop));
        }
        buttons.push(("Quit", "^Q", Click::Quit));
        let mut spans = vec![];
        let mut x = area.x;
        for (label, key, what) in buttons {
            let style = if matches!(what, Click::Stop) { Style::new().fg(t.on_accent).bg(t.bad) } else { Style::new().fg(t.text).bg(t.bubble) };
            let button = [Span::styled(format!(" {label} "), style), Span::styled(format!("{key} "), style.fg(t.dim))];
            let w = button.iter().map(Span::width).sum::<usize>() as u16;
            self.click(Rect::new(x, area.y, w, 1), what);
            x += w + 1;
            spans.extend(button);
            spans.push(Span::raw(" "));
        }
        let toast = self.toast.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(4));
        let right = match toast {
            Some((msg, _)) => Span::styled(msg.clone(), Style::new().fg(t.info)),
            None => {
                let manager = crew::model_of(&self.crew.manager).unwrap_or("default");
                let mut workers = Usage::default();
                for w in &self.workers {
                    workers += w.usage;
                }
                let used = |u: Usage| {
                    if u == Usage::default() { String::new() } else { format!(" {}", tokens(u)) }
                };
                let workers = if workers == Usage::default() { String::new() } else { format!(" · workers{}", used(workers)) };
                Span::styled(format!("claude {manager}{}{workers}", used(self.manager_usage)), Style::new().fg(t.dim))
            }
        };
        // the right side keeps its width; the buttons get the rest
        let [left, _, right_area] = Layout::horizontal([Constraint::Min(0), Constraint::Length(2), Constraint::Length(right.width() as u16)]).areas(area);
        f.render_widget(Line::from(spans), left);
        f.render_widget(Line::from(right), right_area);
    }

    /// Everything about the session in one place: the job, models, usage, settings, and every worker.
    fn draw_info(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let mut workers = Usage::default();
        for w in &self.workers {
            workers += w.usage;
        }
        let mut total = workers;
        total += self.manager_usage;
        let last_ask = self.chat.iter().rposition(|e| matches!(e, Entry::Line(Kind::User, _))).unwrap_or(0);
        let round = self.chat[last_ask..].iter().filter(|e| matches!(e, Entry::Line(Kind::Rule, _))).count();
        let job = match (self.busy, round) {
            (true, 0) => format!("running {} · the manager is planning", fmt_duration(self.job_started.elapsed())),
            (true, r) => format!("running {} · round {r} of up to {}", fmt_duration(self.job_started.elapsed()), self.crew.max_rounds),
            (false, 0) => "idle".into(),
            (false, r) => format!("idle · the last job took {r} round{}", if r == 1 { "" } else { "s" }),
        };
        let manager_state = match self.manager {
            Some(State::Working) => "working",
            Some(State::Done) => "idle",
            Some(State::Failed) => "failed last time",
            None => "not started",
        };
        let or_none = |s: String| {
            if s.is_empty() { "none yet".to_string() } else { s }
        };
        let rows = [
            ("Job", job),
            ("Folder", self.cwd.clone()),
            ("Manager", format!("claude {} · {manager_state} · {}", crew::model_of(&self.crew.manager).unwrap_or("default"), or_none(tokens(self.manager_usage)))),
            ("Workers", format!("{} this job · default model {}", self.workers.len(), crew::model_of(&self.crew.worker).unwrap_or("default"))),
            ("Worker usage", or_none(tokens(workers))),
            ("Total usage", or_none(tokens(total))),
            (
                "Limits",
                format!(
                    "{} rounds · {} workers at once · {}m per worker · stop after {}m quiet · {} tokens per worker",
                    self.crew.max_rounds,
                    self.crew.max_parallel,
                    self.crew.worker_timeout_mins,
                    self.crew.worker_idle_mins,
                    if self.crew.worker_max_tokens == 0 { "unlimited".to_string() } else { format!("{}k", self.crew.worker_max_tokens / 1000) }
                ),
            ),
            ("Config", Crew::path().display().to_string()),
            ("Theme", self.t().name.to_string()),
        ];
        let summary: Vec<Line> =
            rows.into_iter().map(|(k, v)| Line::from(vec![Span::styled(format!("{k:>13}  "), Style::new().fg(t.dim)), Span::styled(v, Style::new().fg(t.text))])).collect();
        let changes: Vec<&str> = self.chat[last_ask..]
            .iter()
            .filter_map(|e| match e {
                Entry::Line(_, l) => l.strip_prefix("files changed: "),
                _ => None,
            })
            .collect();
        let block = |title: &str| {
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(t.faint))
                .padding(Padding::horizontal(1))
                .title(Span::styled(format!(" {title} "), Style::new().fg(t.accent).bold()))
        };
        let table_h = (self.workers.len() as u16 + 3).max(4);
        let changes_h = if changes.is_empty() { 0 } else { changes.len() as u16 + 2 };
        let [top, table_area, changes_area] =
            Layout::vertical([Constraint::Length(summary.len() as u16 + 2), Constraint::Length(table_h), Constraint::Length(changes_h)]).areas(area);
        f.render_widget(Paragraph::new(summary).block(block("Session")), top);

        let header = Row::new(["Worker", "Role", "Step", "Model", "State", "Time", "Tokens", "Owns"]).style(Style::new().fg(t.dim).bold());
        let rows: Vec<Row> = self
            .workers
            .iter()
            .map(|w| {
                let (state, color) = match w.state {
                    None => ("queued", t.dim),
                    Some(State::Working) => ("working", t.warn),
                    Some(State::Done) => ("done", t.good),
                    Some(State::Failed) => ("failed", t.bad),
                };
                let model = if w.model.is_empty() { "default".to_string() } else { w.model.rsplit('/').next().unwrap_or_default().to_string() };
                Row::new(vec![
                    TableCell::from(Span::styled(w.name.clone(), Style::new().fg(self.color_of(&w.name)).bold())),
                    TableCell::from(w.role.clone()),
                    TableCell::from(w.step.to_string()),
                    TableCell::from(model),
                    TableCell::from(Span::styled(state, Style::new().fg(color))),
                    TableCell::from(w.elapsed().map(fmt_duration).unwrap_or_default()),
                    TableCell::from(tokens(w.usage)),
                    TableCell::from(w.files.join(", ")),
                ])
                .style(Style::new().fg(t.text))
            })
            .collect();
        let widths = [
            Constraint::Length(14),
            Constraint::Length(18),
            Constraint::Length(4),
            Constraint::Length(30),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(22),
            Constraint::Min(10),
        ];
        let table_block = block("Workers · click one to open it");
        let inner = table_block.inner(table_area);
        for i in 0..self.workers.len() {
            let y = inner.y + 1 + i as u16;
            if y < inner.bottom() {
                self.click(Rect::new(inner.x, y, inner.width, 1), Click::Tab(FIRST_WORKER_TAB + i));
            }
        }
        f.render_widget(Table::new(rows, widths).header(header).block(table_block), table_area);
        if !changes.is_empty() {
            let lines: Vec<Line> = changes.iter().map(|c| Line::styled(c.to_string(), Style::new().fg(t.text))).collect();
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block("Files changed this job")), changes_area);
        }
    }

    // ---- workers --------------------------------------------------------------
    /// A worker's header: its name in a pill of its colour, its state, role, time, tokens and model.
    fn worker_title(&self, w: &Worker) -> Line<'static> {
        let t = self.t();
        let (state, color) = match w.state {
            None => (format!("queued for step {}", w.step), t.dim),
            Some(State::Working) => ("working".into(), t.warn),
            Some(State::Done) => ("done".into(), t.good),
            Some(State::Failed) => ("failed".into(), t.bad),
        };
        let mut facts = vec![w.role.clone()];
        facts.extend(w.elapsed().map(fmt_duration));
        facts.extend(Some(tokens(w.usage)).filter(|s| !s.is_empty()));
        facts.extend(Some(w.model.rsplit('/').next().unwrap_or_default().to_string()).filter(|s| !s.is_empty()));
        Line::from(vec![
            Span::styled(format!(" {} ", w.name), Style::new().fg(t.on_accent).bg(self.color_of(&w.name)).bold()),
            Span::raw(" "),
            self.dot(w.state),
            Span::styled(format!(" {state}"), Style::new().fg(color)),
            Span::styled(format!("  ·  {} ", facts.join("  ·  ")), Style::new().fg(t.dim)),
        ])
    }

    /// A worker's work log: its actions and output, behind a `│` gutter.
    fn work_log(&self, lines: &[String]) -> Vec<Line<'static>> {
        let t = self.t();
        lines
            .iter()
            .filter_map(|l| worker_line(l, t))
            .map(|mut line| {
                line.spans.insert(0, Span::styled("  │ ", Style::new().fg(t.faint)));
                line
            })
            .collect()
    }

    /// All workers at once: busy ones first, then the newest, up to six panes.
    fn draw_overview(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        if self.workers.is_empty() {
            let text = vec![
                Line::styled("No workers yet.", Style::new().fg(t.text).bold()),
                Line::styled("Message the manager; the workers it creates show up here, live.", Style::new().fg(t.dim)),
            ];
            return f.render_widget(Paragraph::new(text), area);
        }
        let mut shown: Vec<usize> = (0..self.workers.len()).collect();
        shown.sort_by_key(|&i| (self.workers[i].state != Some(State::Working), std::cmp::Reverse(i)));
        shown.truncate(6);
        shown.sort();
        let n = shown.len();
        let cols = match n {
            1 => 1,
            2..=4 => 2,
            _ => 3,
        };
        let rows = n.div_ceil(cols);
        let row_areas = Layout::vertical(vec![Constraint::Ratio(1, rows as u32); rows]).split(area);
        for (r, row) in row_areas.iter().enumerate() {
            let in_row = (n - r * cols).min(cols);
            let cells = Layout::horizontal(vec![Constraint::Ratio(1, in_row as u32); in_row]).split(*row);
            for (c, cell) in cells.iter().enumerate() {
                let w = &self.workers[shown[r * cols + c]];
                self.click(*cell, Click::Tab(FIRST_WORKER_TAB + shown[r * cols + c]));
                let mut title = self.worker_title(w);
                title.spans.insert(0, Span::raw(" "));
                let border = match w.state {
                    Some(State::Working) => t.warn,
                    Some(State::Done) => t.good,
                    Some(State::Failed) => t.bad,
                    None => t.faint,
                };
                let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(border)).title(title);
                let inner = block.inner(*cell).inner(Margin::new(1, 0));
                f.render_widget(block, *cell);
                // only the tail matters in a pane, and wrapping thousands of lines every frame is slow
                let tail = &w.output[w.output.len().saturating_sub(200)..];
                let mut lines: Vec<Line> = tail.iter().filter_map(|l| worker_line(l, t)).collect();
                if lines.is_empty() {
                    let note = if w.state.is_none() { "Queued: starts when the earlier steps finish" } else { "Starting…" };
                    lines.push(Line::styled(note, Style::new().fg(t.dim)));
                }
                render_scrolled(f, lines, inner, 0);
            }
        }
    }

    /// One worker's own thread: the manager's orders, its work log while it works, and its messages.
    fn draw_worker_tab(&self, f: &mut Frame, area: Rect, i: usize) {
        let w = &self.workers[i];
        let [title, _, body] = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)]).areas(area);
        f.render_widget(self.worker_title(w), title);

        let mine = |e: &Entry| matches!(e, Entry::Task(t, _) if t.worker == w.name) || matches!(e, Entry::Said { from, to, .. } if *from == w.name || *to == w.name);
        let entries: Vec<(&Entry, &str)> = self.chat.iter().zip(self.times.iter().map(String::as_str)).filter(|(e, _)| mine(e)).collect();
        let last_task = entries.iter().rposition(|(e, _)| matches!(e, Entry::Task(..)));
        // the log is everything before the reply; while working that's all of it
        let log_end = if matches!(w.state, Some(State::Done | State::Failed)) { reply_start(&w.output) } else { w.output.len() };
        let mut lines = vec![];
        for (n, (entry, time)) in entries.iter().enumerate() {
            match entry {
                Entry::Task(task, state) => {
                    lines.extend(self.task_bubble(task, *state, time, body.width, 0));
                    // ponytail: the whole log goes under the latest orders; split it per round if workers get reused a lot
                    if Some(n) == last_task {
                        lines.push(Line::default());
                        lines.extend(self.work_log(&w.output[..log_end]));
                    }
                }
                Entry::Said { from, to, text } => lines.extend(self.bubble(from, to, text, true, None, vec![], time, body.width, 0)),
                Entry::Line(..) => {}
            }
        }
        render_scrolled(f, lines, body, w.scroll_back);
    }

    /// Claude Code style selection list: `❯` on the chosen row.
    fn draw_picker(&self, f: &mut Frame, p: &Picker) {
        let t = self.t();
        let height = if p.for_manager { Constraint::Length(p.models.len() as u16 + 4) } else { Constraint::Percentage(70) };
        let area = f.area().centered(Constraint::Length(80), height);
        f.render_widget(Clear, area);
        let items: Vec<ListItem> = p
            .models
            .iter()
            .enumerate()
            .map(|(i, (m, tag))| {
                let selected = i == p.sel;
                let style = if selected { Style::new().fg(t.accent) } else { Style::new().fg(t.text) };
                ListItem::new(Line::from(vec![
                    Span::styled(if selected { "❯ " } else { "  " }, style),
                    Span::styled(m.clone(), style),
                    Span::styled(if tag.is_empty() { String::new() } else { format!("  {tag}") }, Style::new().fg(if *tag == "free" { t.good } else { t.dim })),
                ]))
            })
            .collect();
        let title = if p.for_manager { " Select the manager's Claude model " } else { " Select the workers' opencode model " };
        let list = List::new(items).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(t.accent))
                .padding(Padding::uniform(1))
                .title(Span::styled(title, Style::new().fg(t.text).bold()))
                .title_bottom(Line::styled(" click, or ↑↓ and enter · esc or click outside to cancel ", Style::new().fg(t.dim))),
        );
        let mut state = ListState::default().with_selected(Some(p.sel)).with_offset(p.offset.get());
        f.render_stateful_widget(list, area, &mut state);
        p.offset.set(state.offset());
        self.click(area, Click::Picker);
        let rows = area.inner(Margin::new(2, 2)); // border + padding
        for row in 0..rows.height {
            let i = state.offset() + row as usize;
            if i < p.models.len() {
                self.click(Rect::new(rows.x, rows.y + row, rows.width, 1), Click::Pick(i));
            }
        }
    }
}

/// Is this a line of a worker's activity (a tool call, a banner) rather than prose it wrote?
fn is_activity(line: &str) -> bool {
    let s = line.trim_start();
    (s.starts_with("> ") && s.contains(" · ")) || ["Thinking:", "Error:", "→ ", "← ", "⚙ ", "✱ ", "✓ ", "$ ", "──"].iter().any(|p| s.starts_with(p))
}

/// Where a worker's reply starts: the prose after its last action.
fn reply_start(output: &[String]) -> usize {
    output.iter().rposition(|l| is_activity(l)).map_or(0, |i| i + 1)
}

/// `Read notes.md` → `⏺ Read(notes.md)`, with the dot in `color`.
fn tool_line(action: &str, color: Color, t: &Theme) -> Line<'static> {
    let (tool, arg) = action.split_once(' ').unwrap_or((action, ""));
    let mut spans = vec![Span::styled("⏺ ", Style::new().fg(color)), Span::styled(tool.to_string(), Style::new().fg(t.text).bold())];
    if !arg.is_empty() {
        spans.push(Span::styled(format!("({arg})"), Style::new().fg(t.text)));
    }
    Line::from(spans)
}

/// One line of a worker's output, Claude Code style. Model banners (`> build · model`) are dropped.
fn worker_line(line: &str, t: &Theme) -> Option<Line<'static>> {
    let s = line.trim_start();
    if s.starts_with("> ") && s.contains(" · ") {
        return None;
    }
    if let Some(thought) = s.strip_prefix("Thinking:") {
        return Some(Line::from(vec![Span::styled("✻ ", Style::new().fg(t.dim)), Span::styled(thought.trim().to_string(), Style::new().fg(t.dim).italic())]));
    }
    if let Some(cmd) = s.strip_prefix("$ ") {
        return Some(tool_line(&format!("Bash {cmd}"), t.good, t));
    }
    if let Some(action) = ["→ ", "← ", "⚙ ", "✱ ", "✓ "].iter().find_map(|p| s.strip_prefix(p)) {
        return Some(tool_line(action, t.good, t));
    }
    if s.starts_with("Error") || s.starts_with("✗") || s.starts_with("no output for") || s.starts_with("timed out") {
        return Some(Line::from(vec![Span::styled("⎿  ", Style::new().fg(t.dim)), Span::styled(s.to_string(), Style::new().fg(t.bad))]));
    }
    if s.starts_with("──") {
        return Some(Line::styled(s.to_string(), Style::new().fg(t.faint)));
    }
    Some(Line::styled(line.to_string(), Style::new().fg(t.dim)))
}

/// Greedy word wrap, keeping line breaks and indentation; a bullet's wrapped lines line up after the "- ".
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut out = vec![];
    for line in text.lines() {
        let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        let bullet = ["- ", "* "].iter().any(|b| line.trim_start().starts_with(b));
        let hang = if bullet { format!("{indent}  ") } else { indent.clone() };
        let (mut cur, mut fresh) = (indent.clone(), true);
        for word in line.split_whitespace() {
            if !fresh && cur.chars().count() + 1 + word.chars().count() > width {
                out.push(std::mem::replace(&mut cur, hang.clone()));
                fresh = true;
            }
            if !fresh {
                cur.push(' ');
            }
            cur.push_str(word);
            fresh = false;
        }
        out.push(cur);
    }
    out
}

/// Wrap lines into `area`, pinned to the bottom unless scrolled up by `scroll_back` lines.
fn render_scrolled(f: &mut Frame, lines: Vec<Line>, area: Rect, scroll_back: u16) {
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    let bottom = (para.line_count(area.width) as u16).saturating_sub(area.height);
    f.render_widget(para.scroll((bottom.saturating_sub(scroll_back), 0)), area);
}

/// Just enough Markdown for chat replies: headings, bullets, **bold**, `code`, and ``` blocks.
/// Fence lines are hidden; `in_code` carries code-block state across lines.
fn markdown(text: &str, in_code: &mut bool, t: &Theme) -> Option<Line<'static>> {
    let trimmed = text.trim_start();
    if trimmed.starts_with("```") {
        *in_code = !*in_code;
        return None;
    }
    if *in_code {
        return Some(Line::styled(format!("  {text}"), Style::new().fg(t.info)));
    }
    let base = Style::new().fg(t.text);
    if let Some(heading) = trimmed.strip_prefix('#') {
        let heading = heading.trim_start_matches('#').trim();
        return Some(Line::from(inline(heading, base.bold(), t)));
    }
    let indent = &text[..text.len() - trimmed.len()];
    let mut spans = match trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
        Some(item) => {
            let mut s = vec![Span::raw(format!("{indent}  ")), Span::styled("• ", Style::new().fg(t.dim))];
            s.extend(inline(item, base, t));
            s
        }
        None => inline(text, base, t),
    };
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    Some(Line::from(spans))
}

/// Split a line on `code` and **bold** markers into styled spans.
fn inline(text: &str, base: Style, t: &Theme) -> Vec<Span<'static>> {
    let (mut spans, mut cur, mut bold, mut code) = (vec![], String::new(), false, false);
    let mut chars = text.chars().peekable();
    let style = |bold: bool, code: bool| match (code, bold) {
        (true, _) => Style::new().fg(t.info),
        (false, true) => base.bold(),
        _ => base,
    };
    while let Some(c) = chars.next() {
        let toggle = if c == '`' {
            Some(true)
        } else if c == '*' && !code && chars.peek() == Some(&'*') {
            chars.next();
            Some(false)
        } else {
            None
        };
        match toggle {
            Some(is_code) => {
                if !cur.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut cur), style(bold, code)));
                }
                if is_code { code = !code } else { bold = !bold }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        spans.push(Span::styled(cur, style(bold, code)));
    }
    spans
}

/// `12.3k tokens`, plus the cost when the CLI reports one; empty when nothing was used.
fn tokens(u: Usage) -> String {
    let n = u.input + u.output;
    if n == 0 {
        return String::new();
    }
    let count = match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    };
    if u.cost >= 0.005 { format!("{count} tokens (${:.2})", u.cost) } else { format!("{count} tokens") }
}

fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 { format!("{s}s") } else { format!("{}m {:02}s", s / 60, s % 60) }
}

/// Keep the end of a long path: "…/deep/folder".
fn shorten(path: &str, max: usize) -> String {
    let n = path.chars().count();
    if n <= max { path.to_string() } else { format!("…{}", path.chars().skip(n + 1 - max.max(1)).collect::<String>()) }
}

/// The local UTC offset in seconds, from `date +%z` ("+0300"); 0 if that fails.
fn local_offset() -> i64 {
    let out = std::process::Command::new("date").arg("+%z").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let (sign, digits) = if let Some(d) = out.strip_prefix('-') { (-1, d) } else { (1, out.trim_start_matches('+')) };
    let n: i64 = digits.parse().unwrap_or(0);
    sign * (n / 100 * 3_600 + n % 100 * 60)
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, rx: &mut UnboundedReceiver<Msg>) -> std::io::Result<()> {
    while !app.quit {
        while let Ok(msg) = rx.try_recv() {
            app.handle(msg);
        }
        terminal.draw(|f| app.draw(f))?;
        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => app.key(k),
                Event::Paste(text) if app.picker.is_none() => app.insert(&text),
                Event::Mouse(m) => app.mouse(m),
                _ => {}
            }
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    // `foreman ask <worker> <question>`: how a worker talks to the manager mid-task
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("ask") {
        let (worker, question) = (args.get(2).cloned().unwrap_or_default(), args[3.min(args.len())..].join(" "));
        match crew::ask_client(&worker, &question) {
            Ok(answer) => println!("{answer}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    let (tx, mut rx) = unbounded_channel();
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = std::env::current_dir()?.display().to_string();
    let mut app = App {
        crew: Crew::load(),
        chat: Vec::new(),
        times: Vec::new(),
        tz: local_offset(),
        hits: RefCell::new(Vec::new()),
        workers: Vec::new(),
        tab: MANAGER_TAB,
        manager: None,
        manager_usage: Usage::default(),
        scroll_back: 0,
        input: String::new(),
        cursor: 0,
        busy: false,
        job: None,
        job_started: Instant::now(),
        toast: None,
        picker: None,
        history: String::new(),
        theme: 0,
        cwd: if home.is_empty() { cwd.clone() } else { cwd.replacen(&home, "~", 1) },
        started: Instant::now(),
        tx,
        rt: tokio::runtime::Runtime::new()?,
        quit: false,
    };
    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture)?;
    let result = run(&mut terminal, &mut app, &mut rx);
    execute!(std::io::stdout(), DisableBracketedPaste, DisableMouseCapture)?;
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn renders_markdown() {
        let t = &THEMES[0];
        let mut code = false;
        let line = markdown("Use **bold** and `x = 1` here", &mut code, t).unwrap();
        assert_eq!(text(&line), "Use bold and x = 1 here");
        assert_eq!(line.spans[1].style, Style::new().fg(t.text).bold());
        assert_eq!(line.spans[3].style, Style::new().fg(t.info));
        assert_eq!(text(&markdown("## Plan", &mut code, t).unwrap()), "Plan");
        assert_eq!(text(&markdown("  - item", &mut code, t).unwrap()), "    • item");
        assert!(markdown("```rust", &mut code, t).is_none() && code);
        assert_eq!(text(&markdown("let a = **b**;", &mut code, t).unwrap()), "  let a = **b**;", "no markup in code blocks");
        assert!(markdown("```", &mut code, t).is_none() && !code);
        assert_eq!(text(&markdown("2 * 3 = 6", &mut code, t).unwrap()), "2 * 3 = 6");
    }

    #[test]
    fn renders_worker_output_like_claude_code() {
        let t = &THEMES[0];
        let line = |s: &str| worker_line(s, t).map(|l| text(&l));
        assert_eq!(line("> build · nemotron-3.5-lightning-free"), None);
        assert_eq!(line("Thinking: read it first"), Some("✻ read it first".into()));
        assert_eq!(line("← Write calc.py"), Some("⏺ Write(calc.py)".into()));
        assert_eq!(line("$ python3 -m pytest"), Some("⏺ Bash(python3 -m pytest)".into()));
        assert_eq!(line("Error: nope"), Some("⎿  Error: nope".into()));
        assert_eq!(text(&tool_line("Read notes.md", t.good, t)), "⏺ Read(notes.md)");
    }

    /// The reply is the prose after a worker's last action.
    #[test]
    fn finds_worker_reply() {
        let out: Vec<String> = ["> build · m", "→ Read notes.md", "← Write calc.py", "Created file successfully: calc.py", "> build · m", "Done. calc.py has", "add and sub."]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(out[reply_start(&out)..].join(" "), "Done. calc.py has add and sub.");
        assert_eq!(reply_start(&["just text".to_string()]), 0);
        assert_eq!(reply_start(&["→ Read x".to_string()]), 1);
        let failed: Vec<String> = ["⚙ Write a.py", "Error: no tool named finish", "All done."].iter().map(|s| s.to_string()).collect();
        assert_eq!(failed[reply_start(&failed)..], ["All done."]);
    }

    #[test]
    fn wraps_text() {
        assert_eq!(wrap("one two three four", 10), ["one two", "three four"]);
        assert_eq!(wrap("- one two three four", 12), ["- one two", "  three four"]);
        assert_eq!(wrap("a\n  b c", 20), ["a", "  b c"]);
        assert!(wrap("", 20).is_empty());
    }

    #[test]
    fn formats_tokens() {
        assert_eq!(tokens(Usage::default()), "");
        assert_eq!(tokens(Usage { input: 800, output: 50, cost: 0.0 }), "850 tokens");
        assert_eq!(tokens(Usage { input: 109_528, output: 10, cost: 0.1077 }), "109.5k tokens ($0.11)");
        assert_eq!(tokens(Usage { input: 2_000_000, output: 0, cost: 0.0 }), "2.0M tokens");
    }

    #[test]
    fn formats_and_shortens() {
        assert_eq!(fmt_duration(Duration::from_secs(42)), "42s");
        assert_eq!(fmt_duration(Duration::from_secs(187)), "3m 07s");
        assert_eq!(shorten("/a/b/c/deep", 6), "…/deep");
        assert_eq!(shorten("/short", 20), "/short");
    }
}
