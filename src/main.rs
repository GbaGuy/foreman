//! foreman: a terminal boss for a crew of AI agents.
//!
//! You only talk to the manager (Claude). It doesn't do the work itself: it
//! creates workers as needed, gives each a role and a task, runs them (in
//! parallel or in steps), and reviews and re-plans until the goal is met.

mod crew;

use crew::{Crew, Kind, Msg, State};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

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

/// A model picker for the manager (Claude models) or the workers (opencode models).
struct Picker {
    for_manager: bool,
    models: Vec<(String, &'static str)>, // (name, tag)
    sel: usize,
}

struct App {
    crew: Crew,
    chat: Vec<(Kind, String)>,
    workers: Vec<(String, String, State)>, // this run's workers: name, role, state
    manager: Option<State>,
    scroll_back: u16, // lines scrolled up from the bottom of the chat
    input: String,
    busy: bool,
    stop: Arc<AtomicBool>,
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

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Log(kind, text) => self.chat.push((kind, text)),
            Msg::Manager(state) => self.manager = Some(state),
            Msg::Worker(name, role, state) => match self.workers.iter_mut().find(|w| w.0 == name) {
                Some(w) => (w.1, w.2) = (role, state),
                None => self.workers.push((name, role, state)),
            },
            Msg::Notify(text) => self.toast(text),
            Msg::Remember(text) => {
                self.history.push_str(&text);
                self.history.push_str("\n\n");
                // ponytail: keep the last ~6k chars; summarise old turns if the manager needs longer memory
                let cut = self.history.len().saturating_sub(6_000);
                let cut = (cut..self.history.len()).find(|&i| self.history.is_char_boundary(i)).unwrap_or(0);
                self.history.drain(..cut);
            }
            Msg::Finished => self.busy = false,
        }
    }

    // ---- input ----------------------------------------------------------------
    fn key(&mut self, k: KeyEvent) {
        if self.picker.is_some() {
            return self.picker_key(k);
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('q' | 'c') if ctrl => self.quit = true,
            KeyCode::Char('o') if ctrl => self.open_picker(true),
            KeyCode::Char('w') if ctrl => self.open_picker(false),
            KeyCode::Char('s') if ctrl && self.busy => {
                self.stop.store(true, Ordering::Relaxed);
                self.toast("Stopping after the running workers finish");
            }
            KeyCode::Char('t') if ctrl => {
                self.theme = (self.theme + 1) % THEMES.len();
                self.toast(format!("theme: {}", self.t().name));
            }
            KeyCode::Char('u') if ctrl => self.input.clear(),
            KeyCode::PageUp | KeyCode::Up => self.scroll_back = self.scroll_back.saturating_add(if k.code == KeyCode::Up { 1 } else { 10 }),
            KeyCode::PageDown | KeyCode::Down => self.scroll_back = self.scroll_back.saturating_sub(if k.code == KeyCode::Down { 1 } else { 10 }),
            KeyCode::End => self.scroll_back = 0,
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace => _ = self.input.pop(),
            KeyCode::Char(c) if !ctrl => self.input.push(c),
            _ => {}
        }
    }

    fn submit(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.busy {
            return self.toast("The manager is still working");
        }
        self.stop.store(false, Ordering::Relaxed);
        self.workers.clear();
        self.scroll_back = 0;
        self.chat.extend([(Kind::Out, String::new()), (Kind::User, format!("you › {text}"))]);
        self.rt.spawn(crew::orchestrate(self.crew.clone(), text, self.history.clone(), self.tx.clone(), self.stop.clone()));
        self.busy = true;
        self.input.clear();
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

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(t.accent))
            .title(Span::styled(" to manager ", Style::new().fg(t.accent).bold()));
        let (text, style) = if self.input.is_empty() {
            ("Tell the manager what you need…", Style::new().fg(t.dim))
        } else {
            (self.input.as_str(), Style::new().fg(t.text))
        };
        let inner = block.inner(input);
        // keep the end of a long line in view
        let skip = self.input.chars().count().saturating_sub(inner.width.saturating_sub(1) as usize);
        f.render_widget(Paragraph::new(Span::styled(text.chars().skip(skip).collect::<String>(), style)).block(block), input);
        if self.picker.is_none() {
            let x = inner.x + (self.input.chars().count() - skip) as u16;
            f.set_cursor_position(Position::new(x, inner.y));
        }

        let keys = [("^O", "manager model"), ("^W", "worker model"), ("^S", "stop"), ("^T", "theme"), ("↑↓/PgUp/PgDn", "scroll"), ("^Q", "quit")];
        let spans: Vec<Span> = keys
            .iter()
            .flat_map(|(k, d)| [Span::styled(format!(" {k}"), Style::new().fg(t.accent).bold()), Span::styled(format!(" {d} "), Style::new().fg(t.dim))])
            .collect();
        f.render_widget(Line::from(spans), footer);
        if let Some((msg, at)) = &self.toast
            && at.elapsed() < Duration::from_secs(4)
        {
            f.render_widget(Line::styled(format!("{msg} "), Style::new().fg(t.info)).right_aligned(), footer);
        }

        if let Some(p) = &self.picker {
            self.draw_picker(f, p);
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
        let inner = block.inner(area).inner(ratatui::layout::Margin::new(1, 0));
        f.render_widget(block, area);

        let lines: Vec<Line> = if self.chat.is_empty() {
            vec![
                Line::styled("Hi, I'm the manager.", Style::new().fg(t.text).bold()),
                Line::styled("Tell me what you need. I'll create workers, give each a role and a task, and report back.", Style::new().fg(t.dim)),
            ]
        } else {
            self.chat
                .iter()
                .map(|(kind, text)| {
                    let style = match kind {
                        Kind::User => Style::new().fg(t.accent).bold(),
                        Kind::Out => Style::new().fg(t.text),
                        Kind::Info => Style::new().fg(t.secondary),
                        Kind::Good => Style::new().fg(t.good),
                        Kind::Warn => Style::new().fg(t.warn),
                        Kind::Bad => Style::new().fg(t.bad),
                    };
                    Line::styled(text.clone(), style)
                })
                .collect()
        };
        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        let bottom = (para.line_count(inner.width) as u16).saturating_sub(inner.height);
        f.render_widget(para.scroll((bottom.saturating_sub(self.scroll_back), 0)), inner);
    }

    fn draw_workers(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let model = crew::model_of(&self.crew.worker).unwrap_or("default model");
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(t.secondary))
            .title(Line::from(vec![Span::styled(" workers ", Style::new().fg(t.accent).bold()), Span::styled(format!("· {model} "), Style::new().fg(t.dim))]));
        let skip = self.workers.len().saturating_sub(6); // newest workers stay visible
        let lines: Vec<Line> = self.workers[skip..]
            .iter()
            .map(|(name, role, state)| {
                Line::from(vec![
                    Span::raw(" "),
                    self.icon(*state),
                    Span::styled(format!(" {name:<14}"), Style::new().fg(t.text).bold()),
                    Span::styled(role.clone(), Style::new().fg(t.dim)),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines).block(block), area);
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
        if event::poll(Duration::from_millis(80))?
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
        {
            app.key(k);
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
        busy: false,
        stop: Arc::default(),
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
    let result = run(&mut terminal, &mut app, &mut rx);
    ratatui::restore();
    result
}
