//! foreman: a terminal boss for a crew of AI agents.
//!
//! A manager agent reads your goal, gives each worker a role and a task, the
//! workers run (in parallel or in steps), and the manager reviews and re-plans
//! until the goal is met. Every agent is a CLI command with {prompt} in it.

mod crew;

use crew::{Agent, BACKENDS, Crew, Kind, Msg, State};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashMap;
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

/// The "new worker" modal: name, role, backend picker, command.
#[derive(Default)]
struct Form {
    fields: [String; 3], // name, role, command
    backend: usize,
    focus: usize, // 0 name, 1 role, 2 backend, 3 command
    error: String,
}

struct App {
    crew: Crew,
    states: HashMap<String, State>,
    roles: HashMap<String, String>, // roles the manager handed out this run
    logs: HashMap<String, Vec<(Kind, String)>>,
    selected: usize,
    scroll_back: u16, // lines scrolled up from the bottom of the log
    input: String,
    busy: bool,
    stop: Arc<AtomicBool>,
    toast: Option<(String, Instant)>,
    form: Option<Form>,
    theme: usize,
    cwd: String,
    tx: UnboundedSender<Msg>,
    rt: tokio::runtime::Runtime,
    quit: bool,
}

impl App {
    fn agents(&self) -> Vec<&Agent> {
        std::iter::once(&self.crew.manager).chain(&self.crew.workers).collect()
    }

    fn t(&self) -> &'static Theme {
        &THEMES[self.theme]
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }

    fn select(&mut self, name: &str) {
        if let Some(i) = self.agents().iter().position(|a| a.name == name) {
            self.selected = i;
            self.scroll_back = 0;
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Log(name, kind, text) => self.logs.entry(name).or_default().push((kind, text)),
            Msg::State(name, state) => _ = self.states.insert(name, state),
            Msg::Role(name, role) => _ = self.roles.insert(name, role),
            Msg::Notify(text) => self.toast(text),
            Msg::Finished => self.busy = false,
        }
    }

    // ---- input ----------------------------------------------------------------
    fn key(&mut self, k: KeyEvent) {
        if self.form.is_some() {
            return self.form_key(k);
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let n = self.agents().len();
        match k.code {
            KeyCode::Char('q' | 'c') if ctrl => self.quit = true,
            KeyCode::Char('n') if ctrl => self.form = Some(Form { fields: [String::new(), String::new(), BACKENDS[0].1.into()], ..Default::default() }),
            KeyCode::Char('r') if ctrl => self.remove_worker(),
            KeyCode::Char('s') if ctrl && self.busy => {
                self.stop.store(true, Ordering::Relaxed);
                self.toast("Stopping after the running agents finish");
            }
            KeyCode::Char('t') if ctrl => {
                self.theme = (self.theme + 1) % THEMES.len();
                self.toast(format!("theme: {}", self.t().name));
            }
            KeyCode::Char('u') if ctrl => self.input.clear(),
            KeyCode::Tab | KeyCode::Down => (self.selected, self.scroll_back) = ((self.selected + 1) % n, 0),
            KeyCode::BackTab | KeyCode::Up => (self.selected, self.scroll_back) = ((self.selected + n - 1) % n, 0),
            KeyCode::PageUp => self.scroll_back = self.scroll_back.saturating_add(10),
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(10),
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
            return self.toast("The crew is still working");
        }
        let tx = self.tx.clone();
        if let Some(rest) = text.strip_prefix('@') {
            let (name, msg) = rest.split_once(' ').unwrap_or((rest, ""));
            let Some(agent) = self.crew.find(name).cloned().filter(|_| !msg.trim().is_empty()) else {
                let names: Vec<_> = self.agents().iter().map(|a| a.name.clone()).collect();
                return self.toast(format!("Usage: @<agent> message — agents: {}", names.join(", ")));
            };
            let msg = msg.trim().to_string();
            self.select(&agent.name);
            self.rt.spawn(async move {
                crew::run_agent(agent, msg, tx.clone()).await;
                tx.send(Msg::Finished).ok();
            });
        } else {
            self.select(&self.crew.manager.name.clone());
            self.stop.store(false, Ordering::Relaxed);
            self.roles.clear();
            self.rt.spawn(crew::orchestrate(self.crew.clone(), text, tx, self.stop.clone()));
        }
        self.busy = true;
        self.input.clear();
    }

    fn remove_worker(&mut self) {
        if self.busy || self.selected == 0 {
            return self.toast("Select a worker (not the manager) while idle");
        }
        let gone = self.crew.workers.remove(self.selected - 1);
        self.logs.remove(&gone.name);
        self.selected -= 1;
        self.save();
        self.toast(format!("Removed {}", gone.name));
    }

    fn save(&mut self) {
        if let Err(e) = self.crew.save() {
            self.toast(format!("Could not save crew: {e}"));
        }
    }

    fn form_key(&mut self, k: KeyEvent) {
        let form = self.form.as_mut().unwrap();
        let text_field = match form.focus {
            0 => Some(0),
            1 => Some(1),
            3 => Some(2),
            _ => None,
        };
        match k.code {
            KeyCode::Esc => self.form = None,
            KeyCode::Tab | KeyCode::Down => form.focus = (form.focus + 1) % 4,
            KeyCode::BackTab | KeyCode::Up => form.focus = (form.focus + 3) % 4,
            KeyCode::Left | KeyCode::Right if form.focus == 2 => {
                let step = if k.code == KeyCode::Left { BACKENDS.len() - 1 } else { 1 };
                form.backend = (form.backend + step) % BACKENDS.len();
                form.fields[2] = BACKENDS[form.backend].1.into();
            }
            KeyCode::Backspace => _ = text_field.map(|i| form.fields[i].pop()),
            KeyCode::Char(c) => _ = text_field.map(|i| form.fields[i].push(c)),
            KeyCode::Enter => {
                let [name, role, command] = form.fields.clone().map(|f| f.trim().to_string());
                let valid_name = name.len() <= 24
                    && name.starts_with(|c: char| c.is_ascii_lowercase())
                    && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
                if !valid_name || self.crew.find(&name).is_some() {
                    form.error = "name: unique, lowercase, letters/digits/-/_".into();
                } else if crew::build_argv(&command, "x").is_none() {
                    form.error = "command is empty or badly quoted".into();
                } else {
                    self.crew.workers.push(Agent { name: name.clone(), command, role });
                    self.form = None;
                    self.save();
                    self.toast(format!("Hired {name}"));
                }
            }
            _ => {}
        }
    }

    // ---- drawing --------------------------------------------------------------
    fn draw(&self, f: &mut Frame) {
        let t = self.t();
        let [header, body, input, footer] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(5), Constraint::Length(3), Constraint::Length(1)]).areas(f.area());
        let [side, main] = Layout::horizontal([Constraint::Length(34), Constraint::Min(20)]).areas(body);

        let status = if self.busy { Span::styled(" ◐ working ", Style::new().fg(t.warn)) } else { Span::styled(" ○ idle ", Style::new().fg(t.dim)) };
        f.render_widget(
            Line::from(vec![
                Span::styled(" ⚒ foreman ", Style::new().fg(t.sel).bg(t.accent).bold()),
                Span::styled(format!("  {}", self.cwd), Style::new().fg(t.dim)),
            ]),
            header,
        );
        f.render_widget(Line::from(status).right_aligned(), header);

        self.draw_crew(f, side);
        self.draw_output(f, main);

        let at = self.input.starts_with('@');
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(t.accent))
            .title(Span::styled(if at { " message " } else { " goal " }, Style::new().fg(t.accent).bold()));
        let (text, style) = if self.input.is_empty() {
            ("Goal for the manager…  (or @worker message to talk to one directly)", Style::new().fg(t.dim))
        } else {
            (self.input.as_str(), Style::new().fg(t.text))
        };
        let inner = block.inner(input);
        // keep the end of a long line in view
        let skip = self.input.chars().count().saturating_sub(inner.width.saturating_sub(1) as usize);
        f.render_widget(Paragraph::new(Span::styled(text.chars().skip(skip).collect::<String>(), style)).block(block), input);
        if self.form.is_none() {
            let x = inner.x + (self.input.chars().count() - skip) as u16;
            f.set_cursor_position(Position::new(x, inner.y));
        }

        let keys = [("^N", "add"), ("^R", "remove"), ("^S", "stop"), ("^T", "theme"), ("Tab", "switch"), ("PgUp/Dn", "scroll"), ("^Q", "quit")];
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

        if let Some(form) = &self.form {
            self.draw_form(f, form);
        }
    }

    fn icon(&self, name: &str) -> Span<'static> {
        let t = self.t();
        match self.states.get(name).copied().unwrap_or(State::Idle) {
            State::Idle => Span::styled("○", Style::new().fg(t.dim)),
            State::Working => Span::styled("◐", Style::new().fg(t.warn)),
            State::Done => Span::styled("●", Style::new().fg(t.good)),
            State::Failed => Span::styled("✗", Style::new().fg(t.bad)),
        }
    }

    fn draw_crew(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let items: Vec<ListItem> = self
            .agents()
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let role = self.roles.get(&a.name).filter(|r| !r.is_empty()).unwrap_or(&a.role);
                let mut top = vec![self.icon(&a.name), Span::styled(format!(" {}", a.name), Style::new().fg(t.text).bold())];
                if i == 0 {
                    top.push(Span::styled(" ★", Style::new().fg(t.primary)));
                }
                let backend = a.command.split_whitespace().next().unwrap_or_default();
                ListItem::new(vec![
                    Line::from(top),
                    Line::styled(format!("  {}", if role.is_empty() { "—" } else { role }), Style::new().fg(t.secondary)),
                    Line::styled(format!("  {backend}"), Style::new().fg(t.dim).italic()),
                    Line::default(),
                ])
            })
            .collect();
        let list = List::new(items)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(t.primary))
                    .title(Span::styled(" crew ", Style::new().fg(t.accent).bold())),
            )
            .highlight_style(Style::new().bg(t.sel))
            .highlight_symbol("▌");
        f.render_stateful_widget(list, area, &mut ListState::default().with_selected(Some(self.selected)));
    }

    fn draw_output(&self, f: &mut Frame, area: Rect) {
        let t = self.t();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(t.secondary))
            .title(Span::styled(" output ", Style::new().fg(t.accent).bold()));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [tabs, _, log] = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)]).areas(inner);

        let titles: Vec<Line> = self.agents().iter().map(|a| Line::from(vec![self.icon(&a.name), Span::raw(format!(" {}", a.name))])).collect();
        f.render_widget(
            Tabs::new(titles)
                .select(self.selected)
                .style(Style::new().fg(t.dim))
                .highlight_style(Style::new().fg(t.accent).bold().add_modifier(Modifier::UNDERLINED))
                .divider(Span::styled("│", Style::new().fg(t.sel))),
            tabs,
        );

        let name = &self.agents()[self.selected].name;
        let lines: Vec<Line> = match self.logs.get(name) {
            Some(log) => log
                .iter()
                .flat_map(|(kind, text)| {
                    let style = match kind {
                        Kind::Out => Style::new().fg(t.text),
                        Kind::Prompt => Style::new().fg(t.info).bold(),
                        Kind::Info => Style::new().fg(t.primary),
                        Kind::Good => Style::new().fg(t.good).bold(),
                        Kind::Warn => Style::new().fg(t.warn),
                        Kind::Bad => Style::new().fg(t.bad),
                    };
                    // a blank line before each new prompt keeps runs apart
                    let gap = (*kind == Kind::Prompt).then(Line::default);
                    gap.into_iter().chain([Line::styled(text.clone(), style)])
                })
                .collect(),
            None if self.selected == 0 => vec![Line::styled("Ready. Type a goal below; I'll give the workers roles and tasks.", Style::new().fg(t.dim))],
            None => vec![Line::styled("No output yet.", Style::new().fg(t.dim))],
        };
        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        let bottom = (para.line_count(log.width) as u16).saturating_sub(log.height);
        f.render_widget(para.scroll((bottom.saturating_sub(self.scroll_back), 0)), log);
    }

    fn draw_form(&self, f: &mut Frame, form: &Form) {
        let t = self.t();
        let area = f.area().centered(Constraint::Length(64), Constraint::Length(17));
        f.render_widget(Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Thick)
            .border_style(Style::new().fg(t.primary))
            .title(Span::styled(" New worker ", Style::new().fg(t.accent).bold()));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let rows = Layout::vertical([Constraint::Length(3); 4].into_iter().chain([Constraint::Length(1); 2])).split(inner.inner(ratatui::layout::Margin::new(1, 0)));
        let backend = format!("◀ {} ▶", BACKENDS[form.backend].0);
        let values = [form.fields[0].as_str(), form.fields[1].as_str(), backend.as_str(), form.fields[2].as_str()];
        let labels = [" name ", " role ", " backend  ←/→ ", " command "];
        for i in 0..4 {
            let focused = form.focus == i;
            let color = if focused { t.accent } else { t.dim };
            let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(color)).title(Span::styled(labels[i], Style::new().fg(color)));
            let field_inner = block.inner(rows[i]);
            f.render_widget(Paragraph::new(Span::styled(values[i], Style::new().fg(t.text))).block(block), rows[i]);
            if focused && i != 2 {
                f.set_cursor_position(Position::new(field_inner.x + values[i].chars().count() as u16, field_inner.y));
            }
        }
        f.render_widget(Line::styled(form.error.as_str(), Style::new().fg(t.bad)), rows[4]);
        f.render_widget(Line::styled("Tab next · Enter add · Esc cancel", Style::new().fg(t.dim)).right_aligned(), rows[5]);
    }
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, rx: &mut UnboundedReceiver<Msg>) -> std::io::Result<()> {
    while !app.quit {
        while let Ok(msg) = rx.try_recv() {
            app.handle(msg);
        }
        terminal.draw(|f| app.draw(f))?;
        if event::poll(Duration::from_millis(50))?
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
        states: HashMap::new(),
        roles: HashMap::new(),
        logs: HashMap::new(),
        selected: 0,
        scroll_back: 0,
        input: String::new(),
        busy: false,
        stop: Arc::default(),
        toast: None,
        form: None,
        theme: 0,
        cwd: if home.is_empty() { cwd.clone() } else { cwd.replacen(&home, "~", 1) },
        tx,
        rt: tokio::runtime::Runtime::new()?,
        quit: false,
    };
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, &mut rx);
    ratatui::restore();
    result
}
