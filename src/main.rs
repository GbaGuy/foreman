//! foreman: a terminal boss for a crew of AI agents.
//!
//! You only talk to the manager (Claude). It doesn't do the work itself: it
//! creates workers as needed, gives each a role and a task, runs them (in
//! parallel or in steps), and reviews and re-plans until the goal is met.

mod crew;

use crew::{Crew, Kind, Msg, State};
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

/// A model picker for the manager (Claude models) or the workers (opencode models).
struct Picker {
    for_manager: bool,
    models: Vec<(String, &'static str)>, // (name, tag)
    sel: usize,
}

/// One worker of the current job, as the UI tracks it.
struct Worker {
    name: String,
    role: String,
    state: State,
    started: Instant,
    took: Option<Duration>,
    output: Vec<String>,
}

impl Worker {
    fn elapsed(&self) -> Duration {
        self.took.unwrap_or_else(|| self.started.elapsed())
    }
}

/// The worker output viewer: which worker, and how far it's scrolled up.
struct Viewer {
    worker: usize,
    scroll_back: u16,
}

struct App {
    crew: Crew,
    chat: Vec<(Kind, String)>,
    workers: Vec<Worker>,
    manager: Option<State>,
    scroll_back: u16, // lines scrolled up from the bottom of the chat
    input: String,
    cursor: usize, // in chars
    busy: bool,
    job: Option<JoinHandle<()>>,
    toast: Option<(String, Instant)>,
    picker: Option<Picker>,
    viewer: Option<Viewer>,
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

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Log(kind, text) => self.chat.push((kind, text)),
            Msg::Manager(state) => self.manager = Some(state),
            Msg::Worker(name, role, state) => {
                let i = match self.workers.iter().position(|w| w.name == name) {
                    Some(i) => i,
                    None => {
                        let output = Vec::new();
                        self.workers.push(Worker { name, role: role.clone(), state, started: Instant::now(), took: None, output });
                        self.workers.len() - 1
                    }
                };
                let w = &mut self.workers[i];
                if state == State::Working && w.state != State::Working {
                    // the manager reused a name in a later round: same worker, new run
                    if !w.output.is_empty() {
                        w.output.push(format!("── new task as {role} ──"));
                    }
                    (w.started, w.took) = (Instant::now(), None);
                } else if state != State::Working {
                    w.took = Some(w.started.elapsed());
                }
                (w.role, w.state) = (role, state);
            }
            Msg::Output(name, line) => {
                if let Some(w) = self.workers.iter_mut().find(|w| w.name == name) {
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
        if self.viewer.is_some() {
            return self.viewer_key(k);
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.input.chars().count();
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
            KeyCode::Tab => self.open_viewer(),
            KeyCode::Up => self.scroll_back = self.scroll_back.saturating_add(1),
            KeyCode::Down => self.scroll_back = self.scroll_back.saturating_sub(1),
            KeyCode::PageUp => self.scroll_back = self.scroll_back.saturating_add(10),
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(10),
            KeyCode::Esc => self.scroll_back = 0,
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
        self.scroll_back = 0;
        self.chat.extend([(Kind::Out, String::new()), (Kind::User, format!("you › {text}"))]);
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
        for w in self.workers.iter_mut().filter(|w| w.state == State::Working) {
            (w.state, w.took) = (State::Failed, Some(w.started.elapsed()));
            w.output.push("── stopped ──".into());
        }
        self.chat.push((Kind::Warn, "Stopped. Running workers were killed.".into()));
    }

    fn open_viewer(&mut self) {
        if self.workers.is_empty() {
            return self.toast("No workers yet");
        }
        // start on a worker that's busy, else the latest one
        let worker = self.workers.iter().position(|w| w.state == State::Working).unwrap_or(self.workers.len() - 1);
        self.viewer = Some(Viewer { worker, scroll_back: 0 });
    }

    fn viewer_key(&mut self, k: KeyEvent) {
        let n = self.workers.len();
        let v = self.viewer.as_mut().unwrap();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.viewer = None,
            KeyCode::Right | KeyCode::Tab => (v.worker, v.scroll_back) = ((v.worker + 1) % n, 0),
            KeyCode::Left | KeyCode::BackTab => (v.worker, v.scroll_back) = ((v.worker + n - 1) % n, 0),
            KeyCode::Up => v.scroll_back = v.scroll_back.saturating_add(1),
            KeyCode::Down => v.scroll_back = v.scroll_back.saturating_sub(1),
            KeyCode::PageUp => v.scroll_back = v.scroll_back.saturating_add(10),
            KeyCode::PageDown => v.scroll_back = v.scroll_back.saturating_sub(10),
            KeyCode::End => v.scroll_back = 0,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            _ => {}
        }
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
        let t = self.t();
        let strip = if self.workers.is_empty() { 0 } else { self.workers.len().min(6) as u16 + 2 };
        let [header, body, workers, input, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(strip),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(f.area());

        let status = if self.busy {
            Span::styled(format!(" {} working ", self.spinner()), Style::new().fg(t.warn))
        } else {
            Span::styled(" ○ idle ", Style::new().fg(t.dim))
        };
        f.render_widget(
            Line::from(vec![
                Span::styled(" ⚒ foreman ", Style::new().fg(t.sel).bg(t.accent).bold()),
                Span::styled(format!("  {}", shorten(&self.cwd, (header.width as usize).saturating_sub(28))), Style::new().fg(t.dim)),
            ]),
            header,
        );
        f.render_widget(Line::from(status).right_aligned(), header);

        self.draw_chat(f, body);
        if strip > 0 {
            self.draw_workers(f, workers);
        }
        self.draw_input(f, input);

        let mut keys = vec![("^O", "manager model"), ("^W", "worker model")];
        if !self.workers.is_empty() {
            keys.push(("Tab", "worker output"));
        }
        if self.busy {
            keys.push(("^S", "stop"));
        }
        keys.extend([("^T", "theme"), ("PgUp/PgDn", "scroll"), ("^Q", "quit")]);
        let spans: Vec<Span> = keys
            .iter()
            .flat_map(|(k, d)| [Span::styled(format!(" {k}"), Style::new().fg(t.accent).bold()), Span::styled(format!(" {d} "), Style::new().fg(t.dim))])
            .collect();
        // a fresh toast takes the right of the footer; the key hints get what's left
        let toast = self.toast.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(4));
        let width = toast.map_or(0, |(msg, _)| msg.chars().count() as u16 + 2);
        let [keys_area, toast_area] = Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).areas(footer);
        f.render_widget(Line::from(spans), keys_area);
        if let Some((msg, _)) = toast {
            f.render_widget(Line::styled(format!(" {msg} "), Style::new().fg(t.info)).right_aligned(), toast_area);
        }

        if let Some(v) = &self.viewer {
            self.draw_viewer(f, v);
        }
        if let Some(p) = &self.picker {
            self.draw_picker(f, p);
        }
    }

    fn draw_input(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(t.accent))
            .title(Span::styled(" to manager ", Style::new().fg(t.accent).bold()));
        let inner = block.inner(area);
        if self.input.is_empty() {
            f.render_widget(Paragraph::new(Span::styled("Tell the manager what you need…", Style::new().fg(t.dim))).block(block), area);
        } else {
            // scroll horizontally so the cursor stays in view
            let width = inner.width.saturating_sub(1) as usize;
            let skip = self.cursor.saturating_sub(width);
            let shown: String = self.input.chars().skip(skip).take(width + 1).collect();
            f.render_widget(Paragraph::new(Span::styled(shown, Style::new().fg(t.text))).block(block), area);
        }
        if self.picker.is_none() && self.viewer.is_none() {
            let skip = self.cursor.saturating_sub(inner.width.saturating_sub(1) as usize);
            f.set_cursor_position(Position::new(inner.x + (self.cursor - skip) as u16, inner.y));
        }
    }

    fn icon(&self, state: State) -> Span<'static> {
        let t = self.t();
        match state {
            State::Working => Span::styled(self.spinner(), Style::new().fg(t.warn)),
            State::Done => Span::styled("●", Style::new().fg(t.good)),
            State::Failed => Span::styled("✗", Style::new().fg(t.bad)),
        }
    }

    fn draw_chat(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let model = crew::model_of(&self.crew.manager).unwrap_or("default model");
        let mut title = vec![Span::styled(" ★ manager ", Style::new().fg(t.accent).bold()), Span::styled(format!("· claude {model} "), Style::new().fg(t.dim))];
        if self.busy && self.manager == Some(State::Working) {
            title.extend([self.icon(State::Working), Span::styled(" thinking ", Style::new().fg(t.warn))]);
        }
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(t.primary)).title(Line::from(title));
        let inner = block.inner(area).inner(Margin::new(1, 0));
        f.render_widget(block, area);

        let lines: Vec<Line> = if self.chat.is_empty() {
            vec![
                Line::styled("Hi, I'm the manager.", Style::new().fg(t.text).bold()),
                Line::styled("Tell me what you need. I'll create workers, give each a role and a task, and report back.", Style::new().fg(t.dim)),
            ]
        } else {
            let mut in_code = false;
            self.chat
                .iter()
                .filter_map(|(kind, text)| match kind {
                    Kind::Out => markdown(text, &mut in_code, t),
                    Kind::User => Some(Line::styled(text.clone(), Style::new().fg(t.accent).bold())),
                    Kind::Info => Some(Line::styled(text.clone(), Style::new().fg(t.secondary))),
                    Kind::Good => Some(Line::styled(text.clone(), Style::new().fg(t.good))),
                    Kind::Warn => Some(Line::styled(text.clone(), Style::new().fg(t.warn))),
                    Kind::Bad => Some(Line::styled(text.clone(), Style::new().fg(t.bad))),
                })
                .collect()
        };
        render_scrolled(f, lines, inner, self.scroll_back);
    }

    fn draw_workers(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let model = crew::model_of(&self.crew.worker).unwrap_or("default model");
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(t.secondary))
            .title(Line::from(vec![Span::styled(" workers ", Style::new().fg(t.accent).bold()), Span::styled(format!("· {model} "), Style::new().fg(t.dim))]));
        let inner = block.inner(area).inner(Margin::new(1, 0));
        f.render_widget(block, area);
        let skip = self.workers.len().saturating_sub(6); // newest workers stay visible
        let shown = &self.workers[skip..];
        let names: Vec<Line> = shown
            .iter()
            .map(|w| {
                Line::from(vec![
                    self.icon(w.state),
                    Span::styled(format!(" {:<14}", w.name), Style::new().fg(t.text).bold()),
                    Span::styled(w.role.clone(), Style::new().fg(t.dim)),
                ])
            })
            .collect();
        let times: Vec<Line> = shown.iter().map(|w| Line::styled(fmt_duration(w.elapsed()), Style::new().fg(t.dim)).right_aligned()).collect();
        f.render_widget(Paragraph::new(names), inner);
        f.render_widget(Paragraph::new(times), inner);
    }

    fn draw_viewer(&self, f: &mut Frame, v: &Viewer) {
        let t = self.t();
        let Some(w) = self.workers.get(v.worker) else { return };
        let area = f.area().centered(Constraint::Percentage(90), Constraint::Percentage(85));
        f.render_widget(Clear, area);
        let title = Line::from(vec![
            Span::raw(" "),
            self.icon(w.state),
            Span::styled(format!(" {} ", w.name), Style::new().fg(t.accent).bold()),
            Span::styled(format!("· {} · {} ", w.role, fmt_duration(w.elapsed())), Style::new().fg(t.dim)),
        ]);
        let hint = format!(" {}/{} · ←/→ worker · ↑/↓ PgUp/PgDn scroll · Esc close ", v.worker + 1, self.workers.len());
        let block = Block::bordered()
            .border_type(BorderType::Thick)
            .border_style(Style::new().fg(t.secondary))
            .title(title)
            .title_bottom(Line::styled(hint, Style::new().fg(t.dim)).right_aligned());
        let inner = block.inner(area).inner(Margin::new(1, 0));
        f.render_widget(block, area);
        let lines: Vec<Line> = if w.output.is_empty() {
            vec![Line::styled("No output yet.", Style::new().fg(t.dim))]
        } else {
            w.output.iter().map(|l| Line::styled(l.as_str(), Style::new().fg(t.text))).collect()
        };
        render_scrolled(f, lines, inner, v.scroll_back);
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
                Event::Paste(text) if app.picker.is_none() && app.viewer.is_none() => app.insert(&text),
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
        manager: None,
        scroll_back: 0,
        input: String::new(),
        cursor: 0,
        busy: false,
        job: None,
        toast: None,
        picker: None,
        viewer: None,
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
    fn formats_and_shortens() {
        assert_eq!(fmt_duration(Duration::from_secs(42)), "42s");
        assert_eq!(fmt_duration(Duration::from_secs(187)), "3m 07s");
        assert_eq!(shorten("/a/b/c/deep", 6), "…/deep");
        assert_eq!(shorten("/short", 20), "/short");
    }
}
