//! foreman: a terminal boss for a crew of AI agents.
//!
//! You only talk to the manager (Claude). It doesn't do the work itself: it
//! creates workers as needed, gives each a role and a task, runs them (in
//! parallel or in steps), and reviews and re-plans until the goal is met.
//!
//! The UI is tab based: the manager chat, an overview grid of all workers,
//! and one tab per worker with its task and full output.

mod crew;

use crew::{Crew, Kind, Msg, State, Task};
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

struct Theme {
    name: &'static str,
    accent: Color,
    primary: Color,
    secondary: Color,
    dim: Color,
    text: Color,
    sel: Color,
    good: Color,
    warn: Color,
    bad: Color,
    info: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

#[rustfmt::skip]
const THEMES: [Theme; 3] = [
    Theme { name: "tokyo-night", accent: rgb(0xff9e64), primary: rgb(0xbb9af7), secondary: rgb(0x7aa2f7), dim: rgb(0x565f89),
            text: rgb(0xc0caf5), sel: rgb(0x292e42), good: rgb(0x9ece6a), warn: rgb(0xe0af68), bad: rgb(0xf7768e), info: rgb(0x7dcfff) },
    Theme { name: "catppuccin", accent: rgb(0xfab387), primary: rgb(0xcba6f7), secondary: rgb(0x89b4fa), dim: rgb(0x6c7086),
            text: rgb(0xcdd6f4), sel: rgb(0x313244), good: rgb(0xa6e3a1), warn: rgb(0xf9e2af), bad: rgb(0xf38ba8), info: rgb(0x89dceb) },
    Theme { name: "gruvbox", accent: rgb(0xfe8019), primary: rgb(0xd3869b), secondary: rgb(0x83a598), dim: rgb(0x7c6f64),
            text: rgb(0xebdbb2), sel: rgb(0x3c3836), good: rgb(0xb8bb26), warn: rgb(0xfabd2f), bad: rgb(0xfb4934), info: rgb(0x8ec07c) },
];

const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
const MAX_OUTPUT_LINES: usize = 5_000;
/// Fixed tabs before the per-worker ones.
const MANAGER_TAB: usize = 0;
const OVERVIEW_TAB: usize = 1;

/// A model picker for the manager (Claude models) or the workers (opencode models).
struct Picker {
    for_manager: bool,
    models: Vec<(String, &'static str)>, // (name, tag)
    sel: usize,
}

/// One entry in the manager chat.
enum Entry {
    Line(Kind, String),
    Task(Task),
}

/// One worker of the current job, as the UI tracks it.
struct Worker {
    name: String,
    role: String,
    task: String,
    step: i64,
    state: Option<State>, // None while queued behind an earlier step
    started: Instant,
    took: Option<Duration>,
    output: Vec<String>,
    scroll_back: u16,
}

impl Worker {
    fn elapsed(&self) -> Option<Duration> {
        self.state.map(|_| self.took.unwrap_or_else(|| self.started.elapsed()))
    }
}

struct App {
    crew: Crew,
    chat: Vec<Entry>,
    workers: Vec<Worker>,
    tab: usize, // MANAGER_TAB, OVERVIEW_TAB, or 2 + worker index
    manager: Option<State>,
    thinking: Option<u64>, // rough tokens the manager has spent thinking on its current call
    live: String,          // the manager's reply as it streams in
    scroll_back: u16,      // lines the chat is scrolled up from the bottom
    input: String,
    cursor: usize, // in chars
    busy: bool,
    job: Option<JoinHandle<()>>,
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
        SPINNER[(self.started.elapsed().as_millis() / 150) as usize % SPINNER.len()]
    }

    fn worker_mut(&mut self, name: &str) -> Option<&mut Worker> {
        self.workers.iter_mut().find(|w| w.name == name)
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Log(kind, text) => self.chat.push(Entry::Line(kind, text)),
            Msg::Manager(state) => (self.manager, self.thinking, self.live) = (Some(state), None, String::new()),
            Msg::Thinking(n) => self.thinking = Some(self.thinking.unwrap_or(0) + n),
            Msg::ManagerText(text) => self.live.push_str(&text),
            Msg::Assign(task) => {
                self.chat.push(Entry::Task(task.clone()));
                match self.worker_mut(&task.worker) {
                    // the manager reused a name in a later round: same tab, new task
                    Some(w) => {
                        w.output.push(format!("── new task as {} ──", task.role));
                        (w.role, w.task, w.step, w.state, w.took) = (task.role, task.task, task.step, None, None);
                    }
                    None => self.workers.push(Worker {
                        name: task.worker,
                        role: task.role,
                        task: task.task,
                        step: task.step,
                        state: None,
                        started: Instant::now(),
                        took: None,
                        output: Vec::new(),
                        scroll_back: 0,
                    }),
                }
            }
            Msg::Worker(name, state) => {
                if let Some(w) = self.worker_mut(&name) {
                    if state == State::Working {
                        (w.started, w.took) = (Instant::now(), None);
                    } else {
                        w.took = Some(w.started.elapsed());
                    }
                    w.state = Some(state);
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
            Msg::Notify(text) => self.toast(text),
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
        let tabs = 2 + self.workers.len();
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
            OVERVIEW_TAB => return,
            i => &mut self.workers[i - 2].scroll_back,
        };
        *target = if by == i32::MIN { 0 } else { (*target as i32 + by).clamp(0, u16::MAX as i32) as u16 };
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
            return self.toast("The manager is still working (Ctrl+S stops it)");
        }
        self.workers.clear();
        (self.tab, self.scroll_back) = (MANAGER_TAB, 0);
        self.chat.push(Entry::Line(Kind::User, text.clone()));
        self.job = Some(self.rt.spawn(crew::orchestrate(self.crew.clone(), text, self.history.clone(), self.tx.clone())));
        self.busy = true;
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
        self.chat.push(Entry::Line(Kind::Warn, "Stopped. Running workers were killed.".into()));
    }

    fn open_picker(&mut self, for_manager: bool) {
        if self.busy {
            return self.toast("Wait until the manager is idle");
        }
        let models = if for_manager {
            Ok(crew::CLAUDE_MODELS.iter().map(|m| (m.to_string(), "claude")).collect())
        } else {
            crew::opencode_models()
        };
        match models {
            Ok(models) => {
                let current = if for_manager { &self.crew.manager } else { &self.crew.worker };
                let current = crew::model_of(current).unwrap_or_default();
                let sel = models.iter().position(|(m, _)| m == current).unwrap_or(0);
                self.picker = Some(Picker { for_manager, models, sel });
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
            KeyCode::Enter => {
                let (model, for_manager) = (p.models[p.sel].0.clone(), p.for_manager);
                self.picker = None;
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
            _ => {}
        }
    }

    // ---- drawing --------------------------------------------------------------
    fn draw(&self, f: &mut Frame) {
        let [tabs, sub, body, input, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(f.area());
        self.draw_tabs(f, tabs);
        self.draw_subtitle(f, sub);
        match self.tab {
            MANAGER_TAB => self.draw_chat(f, body),
            OVERVIEW_TAB => self.draw_overview(f, body),
            i => self.draw_worker_tab(f, body, i - 2),
        }
        self.draw_input(f, input);
        self.draw_footer(f, footer);
        if let Some(p) = &self.picker {
            self.draw_picker(f, p);
        }
    }

    fn icon(&self, state: Option<State>) -> Span<'static> {
        let t = self.t();
        match state {
            None => Span::styled("○", Style::new().fg(t.dim)),
            Some(State::Working) => Span::styled(self.spinner(), Style::new().fg(t.warn)),
            Some(State::Done) => Span::styled("●", Style::new().fg(t.good)),
            Some(State::Failed) => Span::styled("✗", Style::new().fg(t.bad)),
        }
    }

    /// The brand, then one tab per view: manager, overview, each worker. The selected tab is lifted.
    fn draw_tabs(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let manager_icon = match (self.busy, self.manager) {
            (true, Some(State::Working)) => self.icon(Some(State::Working)),
            _ => Span::styled("★", Style::new().fg(t.primary)),
        };
        let mut titles: Vec<Vec<Span>> = vec![
            vec![manager_icon, Span::raw(" Manager")],
            vec![Span::styled("▦", Style::new().fg(t.secondary)), Span::raw(format!(" Workers {}", self.workers.len()))],
        ];
        titles.extend(self.workers.iter().map(|w| vec![self.icon(w.state), Span::raw(format!(" {}", w.name))]));

        let mut spans = vec![Span::styled(" ⚒ foreman ", Style::new().fg(t.sel).bg(t.accent).bold()), Span::raw(" ")];
        for (i, title) in titles.into_iter().enumerate() {
            let selected = i == self.tab;
            let (fg, bg) = if selected { (t.accent, t.sel) } else { (t.dim, Color::Reset) };
            spans.push(Span::styled(" ", Style::new().bg(bg)));
            for s in title {
                let style = if s.style.fg.is_some() { s.style.bg(bg) } else { Style::new().fg(fg).bg(bg) };
                spans.push(Span::styled(s.content, if selected { style.bold() } else { style }));
            }
            spans.push(Span::styled(" ", Style::new().bg(bg)));
            if i == OVERVIEW_TAB && !self.workers.is_empty() {
                spans.push(Span::styled(" │", Style::new().fg(t.sel)));
            }
        }
        // ponytail: many workers can overflow a narrow terminal; Tab still reaches them, the bar just clips
        f.render_widget(Line::from(spans), area);
        let status = if self.busy {
            Span::styled(format!(" {} working ", self.spinner()), Style::new().fg(t.warn))
        } else {
            Span::styled(" ○ idle ", Style::new().fg(t.dim))
        };
        f.render_widget(Line::from(status).right_aligned(), area);
    }

    fn draw_subtitle(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let manager = crew::model_of(&self.crew.manager).unwrap_or("default");
        let worker = crew::model_of(&self.crew.worker).unwrap_or("default");
        let worker = worker.rsplit('/').next().unwrap_or(worker);
        let models = format!("manager claude {manager} · workers {worker} ");
        let room = (area.width as usize).saturating_sub(models.chars().count() + 4);
        f.render_widget(Line::styled(format!(" {}", shorten(&self.cwd, room)), Style::new().fg(t.dim)), area);
        f.render_widget(Line::styled(models, Style::new().fg(t.dim)).right_aligned(), area);
    }

    fn draw_input(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(t.accent))
            .title(Span::styled(" ❯ to manager ", Style::new().fg(t.accent).bold()));
        let inner = block.inner(area);
        if self.input.is_empty() {
            let hint = if self.busy { "The manager is working… (Ctrl+S stops it)" } else { "Tell the manager what you need…" };
            f.render_widget(Paragraph::new(Span::styled(hint, Style::new().fg(t.dim))).block(block), area);
        } else {
            // scroll horizontally so the cursor stays in view
            let width = inner.width.saturating_sub(1) as usize;
            let skip = self.cursor.saturating_sub(width);
            let shown: String = self.input.chars().skip(skip).take(width + 1).collect();
            f.render_widget(Paragraph::new(Span::styled(shown, Style::new().fg(t.text))).block(block), area);
        }
        if self.picker.is_none() {
            let skip = self.cursor.saturating_sub(inner.width.saturating_sub(1) as usize);
            f.set_cursor_position(Position::new(inner.x + (self.cursor - skip) as u16, inner.y));
        }
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let mut keys = vec![("Tab", "next tab")];
        if self.tab != OVERVIEW_TAB {
            keys.push(("↑↓ PgUp/PgDn", "scroll"));
        }
        if self.busy {
            keys.push(("^S", "stop"));
        }
        keys.extend([("^O", "manager model"), ("^W", "worker model"), ("^T", "theme"), ("^Q", "quit")]);
        let spans: Vec<Span> = keys
            .iter()
            .flat_map(|(k, d)| [Span::styled(format!(" {k}"), Style::new().fg(t.accent).bold()), Span::styled(format!(" {d} "), Style::new().fg(t.dim))])
            .collect();
        // a fresh toast takes the right of the footer; the key hints get what's left
        let toast = self.toast.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(4));
        let width = toast.map_or(0, |(msg, _)| msg.chars().count() as u16 + 2);
        let [keys_area, toast_area] = Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).areas(area);
        f.render_widget(Line::from(spans), keys_area);
        if let Some((msg, _)) = toast {
            f.render_widget(Line::styled(format!(" {msg} "), Style::new().fg(t.info)).right_aligned(), toast_area);
        }
    }

    // ---- manager tab ----------------------------------------------------------
    fn draw_chat(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(t.primary));
        let inner = block.inner(area).inner(Margin::new(1, 0));
        f.render_widget(block, area);
        render_scrolled(f, self.chat_lines(inner.width), inner, self.scroll_back);
    }

    fn chat_lines(&self, width: u16) -> Vec<Line<'static>> {
        let t = self.t();
        if self.chat.is_empty() && !self.busy {
            return welcome(t);
        }
        let label = || Line::from(vec![Span::styled("★ ", Style::new().fg(t.primary)), Span::styled("manager", Style::new().fg(t.primary).bold())]);
        let indent = |mut line: Line<'static>| {
            line.spans.insert(0, Span::raw("  "));
            line
        };
        let (mut lines, mut in_code, mut in_manager) = (vec![], false, false);
        for entry in &self.chat {
            let manager_entry = matches!(entry, Entry::Line(Kind::Out | Kind::Think, _));
            if manager_entry && !in_manager {
                lines.extend([Line::default(), label()]);
            }
            in_manager = manager_entry;
            match entry {
                Entry::Line(Kind::Out, text) => lines.extend(markdown(text, &mut in_code, t).map(indent)),
                Entry::Line(Kind::Think, text) => lines.push(indent(Line::styled(text.clone(), Style::new().fg(t.dim).italic()))),
                Entry::Line(Kind::User, text) => lines.extend([
                    Line::default(),
                    Line::from(vec![Span::styled("❯ ", Style::new().fg(t.accent).bold()), Span::styled(text.clone(), Style::new().fg(t.text).bold())]),
                ]),
                Entry::Line(Kind::Rule, text) => {
                    let fill = (width as usize).saturating_sub(text.chars().count() + 4);
                    lines.extend([Line::default(), Line::styled(format!("── {text} {}", "─".repeat(fill)), Style::new().fg(t.secondary))]);
                }
                Entry::Task(task) => lines.extend([
                    Line::from(vec![
                        Span::styled(format!(" {} ", task.step), Style::new().fg(t.secondary).bg(t.sel).bold()),
                        Span::styled(format!(" {}", task.worker), Style::new().fg(t.text).bold()),
                        Span::styled(format!(" · {}", task.role), Style::new().fg(t.dim)),
                    ]),
                    Line::styled(format!("    {}", task.task), Style::new().fg(t.dim)),
                ]),
                Entry::Line(kind, text) => {
                    let color = match kind {
                        Kind::Good => t.good,
                        Kind::Warn => t.warn,
                        Kind::Bad => t.bad,
                        _ => t.secondary,
                    };
                    lines.push(Line::styled(format!("  {text}"), Style::new().fg(color)));
                }
            }
        }
        if self.busy && self.manager == Some(State::Working) {
            if !in_manager {
                lines.extend([Line::default(), label()]);
            }
            lines.extend(self.live_lines().into_iter().map(indent));
        }
        lines
    }

    /// The manager's in-progress call: a thinking line, then its reply as it streams.
    fn live_lines(&self) -> Vec<Line<'static>> {
        let t = self.t();
        let thought = self.thinking.map(|n| format!(" (~{n} tokens)")).unwrap_or_default();
        let status = match (self.thinking.is_some(), self.live.is_empty()) {
            (_, false) => format!("💭 thought{thought}, writing…"),
            (true, true) => format!("{} 💭 thinking{thought}…", self.spinner()),
            (false, true) => format!("{} reading…", self.spinner()),
        };
        let mut lines = vec![Line::styled(status, Style::new().fg(t.warn).italic())];
        lines.extend(self.live.lines().map(|l| Line::styled(l.to_string(), Style::new().fg(t.dim))));
        lines
    }

    // ---- workers --------------------------------------------------------------
    fn worker_block(&self, w: &Worker) -> Block<'static> {
        let t = self.t();
        let color = match w.state {
            None => t.dim,
            Some(State::Working) => t.warn,
            Some(State::Done) => t.good,
            Some(State::Failed) => t.bad,
        };
        let time = w.elapsed().map_or(format!(" waits for step {} ", w.step), |d| format!(" {} ", fmt_duration(d)));
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(color))
            .title(Line::from(vec![
                Span::raw(" "),
                self.icon(w.state),
                Span::styled(format!(" {} ", w.name), Style::new().fg(t.text).bold()),
                Span::styled(format!("· {} ", w.role), Style::new().fg(t.dim)),
            ]))
            .title_top(Line::styled(time, Style::new().fg(t.dim)).right_aligned())
    }

    fn output_lines(&self, w: &Worker, last: usize) -> Vec<Line<'static>> {
        let t = self.t();
        let tail = &w.output[w.output.len().saturating_sub(last)..];
        if tail.is_empty() {
            let note = if w.state.is_none() { "queued: starts when the earlier steps finish" } else { "starting…" };
            return vec![Line::styled(note, Style::new().fg(t.dim).italic())];
        }
        tail.iter().map(|l| Line::styled(l.clone(), output_style(l, t))).collect()
    }

    /// All workers at once: busy ones first, then the newest, up to six panes.
    fn draw_overview(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        if self.workers.is_empty() {
            let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(t.secondary));
            let text = vec![
                Line::default(),
                Line::styled("No workers yet.", Style::new().fg(t.text).bold()).centered(),
                Line::styled("Give the manager a job and the workers it creates show up here, live.", Style::new().fg(t.dim)).centered(),
            ];
            return f.render_widget(Paragraph::new(text).block(block), area);
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
                let block = self.worker_block(w);
                let inner = block.inner(*cell).inner(Margin::new(1, 0));
                f.render_widget(block, *cell);
                // only the tail matters in a pane, and wrapping thousands of lines every frame is slow
                render_scrolled(f, self.output_lines(w, 200), inner, 0);
            }
        }
    }

    /// One worker: its task on top, then its full output with scrollback.
    fn draw_worker_tab(&self, f: &mut Frame, area: Rect, i: usize) {
        let t = self.t();
        let w = &self.workers[i];
        let block = self.worker_block(w).title_bottom(Line::styled(" ↑↓ PgUp/PgDn scroll · Esc jump to bottom ", Style::new().fg(t.dim)).right_aligned());
        let inner = block.inner(area).inner(Margin::new(1, 0));
        f.render_widget(block, area);
        let task = Paragraph::new(Line::from(vec![
            Span::styled(format!(" step {} ", w.step), Style::new().fg(t.secondary).bg(t.sel).bold()),
            Span::styled(format!(" {}", w.task), Style::new().fg(t.text)),
        ]))
        .wrap(Wrap { trim: false });
        let task_height = (task.line_count(inner.width) as u16).min(inner.height / 3).max(1);
        let [task_area, rule, out] = Layout::vertical([Constraint::Length(task_height), Constraint::Length(1), Constraint::Min(1)]).areas(inner);
        f.render_widget(task, task_area);
        f.render_widget(Line::styled("─".repeat(rule.width as usize), Style::new().fg(t.sel)), rule);
        render_scrolled(f, self.output_lines(w, MAX_OUTPUT_LINES), out, w.scroll_back);
    }

    fn draw_picker(&self, f: &mut Frame, p: &Picker) {
        let t = self.t();
        let height = if p.for_manager { Constraint::Length(p.models.len() as u16 + 2) } else { Constraint::Percentage(70) };
        let area = f.area().centered(Constraint::Length(96), height);
        f.render_widget(Clear, area);
        let items: Vec<ListItem> = p
            .models
            .iter()
            .map(|(m, tag)| {
                let color = match *tag {
                    "free" => t.good,
                    "claude" => t.accent,
                    _ => t.info,
                };
                ListItem::new(Line::from(vec![Span::styled(format!("{tag:<7}"), Style::new().fg(color).bold()), Span::styled(m.as_str(), Style::new().fg(t.text))]))
            })
            .collect();
        let title = if p.for_manager { " Claude model for the manager " } else { " opencode model for workers " };
        let list = List::new(items)
            .block(
                Block::bordered()
                    .border_type(BorderType::Thick)
                    .border_style(Style::new().fg(t.primary))
                    .title(Span::styled(title, Style::new().fg(t.accent).bold()))
                    .title_bottom(Line::styled(" ↑/↓ choose · Enter use · Esc cancel ", Style::new().fg(t.dim)).right_aligned()),
            )
            .highlight_style(Style::new().bg(t.sel).fg(t.accent))
            .highlight_symbol("▌");
        f.render_stateful_widget(list, area, &mut ListState::default().with_selected(Some(p.sel)));
    }
}

fn welcome(t: &Theme) -> Vec<Line<'static>> {
    let tip = |key: &str, text: &str| {
        Line::from(vec![Span::styled(format!("  {key:<10}"), Style::new().fg(t.accent).bold()), Span::styled(text.to_string(), Style::new().fg(t.dim))])
    };
    vec![
        Line::default(),
        Line::from(vec![Span::styled("★ ", Style::new().fg(t.primary)), Span::styled("Hi, I'm the manager.", Style::new().fg(t.text).bold())]),
        Line::styled("  Tell me what you need. I'll create workers, give each a role and a task,", Style::new().fg(t.text)),
        Line::styled("  check their work, and report back.", Style::new().fg(t.text)),
        Line::default(),
        tip("Tab", "switch between me, the workers overview, and each worker"),
        tip("Ctrl+S", "stop a running job"),
        tip("Ctrl+O/W", "pick my model / the workers' model"),
    ]
}

/// Colour a worker output line by what it is: reasoning, a tool action, an error, or plain text.
fn output_style(line: &str, t: &Theme) -> Style {
    let s = line.trim_start();
    if s.starts_with("Thinking:") {
        Style::new().fg(t.dim).italic()
    } else if s.starts_with("> ") || s.starts_with("──") {
        Style::new().fg(t.dim)
    } else if s.starts_with("Error") || s.starts_with("✗") || s.starts_with("no output for") || s.starts_with("timed out") {
        Style::new().fg(t.bad)
    } else if ["→", "←", "$ ", "⚙", "✱", "✓", "⏺"].iter().any(|p| s.starts_with(p)) {
        Style::new().fg(t.info)
    } else {
        Style::new().fg(t.text)
    }
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
        return Some(Line::from(inline(heading, base.fg(t.accent).bold(), t)));
    }
    let indent = &text[..text.len() - trimmed.len()];
    let mut spans = match trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
        Some(item) => {
            let mut s = vec![Span::raw(format!("{indent}  ")), Span::styled("• ", Style::new().fg(t.accent))];
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

fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 { format!("{s}s") } else { format!("{}m {:02}s", s / 60, s % 60) }
}

/// Keep the end of a long path: "…/deep/folder".
fn shorten(path: &str, max: usize) -> String {
    let n = path.chars().count();
    if n <= max { path.to_string() } else { format!("…{}", path.chars().skip(n + 1 - max.max(1)).collect::<String>()) }
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
                _ => {}
            }
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let (tx, mut rx) = unbounded_channel();
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = std::env::current_dir()?.display().to_string();
    let mut app = App {
        crew: Crew::load(),
        chat: Vec::new(),
        workers: Vec::new(),
        tab: MANAGER_TAB,
        manager: None,
        thinking: None,
        live: String::new(),
        scroll_back: 0,
        input: String::new(),
        cursor: 0,
        busy: false,
        job: None,
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
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    let result = run(&mut terminal, &mut app, &mut rx);
    execute!(std::io::stdout(), DisableBracketedPaste)?;
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
    fn styles_worker_output() {
        let t = &THEMES[0];
        assert_eq!(output_style("Thinking: hmm", t), Style::new().fg(t.dim).italic());
        assert_eq!(output_style("← Write calc.py", t), Style::new().fg(t.info));
        assert_eq!(output_style("Error: nope", t), Style::new().fg(t.bad));
        assert_eq!(output_style("plain", t), Style::new().fg(t.text));
    }

    #[test]
    fn formats_and_shortens() {
        assert_eq!(fmt_duration(Duration::from_secs(42)), "42s");
        assert_eq!(fmt_duration(Duration::from_secs(187)), "3m 07s");
        assert_eq!(shorten("/a/b/c/deep", 6), "…/deep");
        assert_eq!(shorten("/short", 20), "/short");
    }
}
