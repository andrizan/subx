//! `subx tui`: a ratatui front end for the parts that are tedious on a command
//! line — writing encode profiles, checking tools, and launching encodes with
//! a live log. Everything it does goes through the same code as the CLI:
//! profiles are written to `subx.local.toml`, encodes and installs run the
//! `subx` executable itself as a child process.

pub mod profiles;
pub mod runner;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, List, ListItem, ListState, Paragraph, Row, Table, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::common::config::AppConfig;
use crate::common::tools::{self, Tool};
use profiles::{Field, Form};
use runner::Job;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Profiles,
    Encode,
    Tools,
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::Profiles, Tab::Encode, Tab::Tools];

    fn title(self) -> &'static str {
        match self {
            Tab::Profiles => "1 Profiles",
            Tab::Encode => "2 Encode",
            Tab::Tools => "3 Tools",
        }
    }
}

/// Fields of the encode launcher, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncField {
    Profile,
    Engine,
    Source,
    Dest,
    Only,
    Jobs,
    DryRun,
}

impl EncField {
    const ALL: [EncField; 7] = [
        EncField::Profile,
        EncField::Engine,
        EncField::Source,
        EncField::Dest,
        EncField::Only,
        EncField::Jobs,
        EncField::DryRun,
    ];

    fn label(self) -> &'static str {
        match self {
            EncField::Profile => "profile",
            EncField::Engine => "engine",
            EncField::Source => "source folder",
            EncField::Dest => "output folder",
            EncField::Only => "only episodes",
            EncField::Jobs => "jobs",
            EncField::DryRun => "dry run",
        }
    }

    fn is_text(self) -> bool {
        matches!(
            self,
            EncField::Source | EncField::Dest | EncField::Only | EncField::Jobs
        )
    }
}

#[derive(Debug, Clone)]
struct EncodeForm {
    profile: usize,
    native: bool,
    source: String,
    dest: String,
    only: String,
    jobs: String,
    dry_run: bool,
    field: usize,
    editing: Option<String>,
}

impl EncodeForm {
    fn selected(&self) -> EncField {
        EncField::ALL[self.field]
    }

    fn value(&self, field: EncField, names: &[String]) -> String {
        match field {
            EncField::Profile => names
                .get(self.profile)
                .cloned()
                .unwrap_or_else(|| "(no profiles yet)".to_string()),
            EncField::Engine => if self.native { "native" } else { "staxrip" }.to_string(),
            EncField::Source => self.source.clone(),
            EncField::Dest => self.dest.clone(),
            EncField::Only => self.only.clone(),
            EncField::Jobs => self.jobs.clone(),
            EncField::DryRun => if self.dry_run { "yes" } else { "no" }.to_string(),
        }
    }

    fn set_text(&mut self, field: EncField, text: String) {
        match field {
            EncField::Source => self.source = text,
            EncField::Dest => self.dest = text,
            EncField::Only => self.only = text,
            EncField::Jobs => self.jobs = text,
            _ => {}
        }
    }

    /// Arguments for `subx encode ...` (without the global `--config`).
    fn args(&self, names: &[String]) -> Option<Vec<String>> {
        let profile = names.get(self.profile)?;
        if self.source.trim().is_empty() || self.dest.trim().is_empty() {
            return None;
        }
        let mut a = vec![
            "encode".to_string(),
            self.source.trim().to_string(),
            self.dest.trim().to_string(),
            "--profile".to_string(),
            profile.clone(),
            "--engine".to_string(),
            if self.native { "native" } else { "staxrip" }.to_string(),
        ];
        if !self.only.trim().is_empty() {
            a.extend(["--only".to_string(), self.only.trim().to_string()]);
        }
        if let Ok(j) = self.jobs.trim().parse::<usize>()
            && j > 1
        {
            a.extend(["-j".to_string(), j.to_string()]);
        }
        if self.dry_run {
            a.push("--dry-run".to_string());
        }
        Some(a)
    }
}

struct ToolRow {
    tool: Tool,
    status: &'static str,
    version: String,
    location: String,
}

struct App {
    cfg: AppConfig,
    local_path: PathBuf,
    config_arg: Option<PathBuf>,
    exe: PathBuf,
    tab: Tab,
    names: Vec<String>,
    selected: usize,
    form: Option<Form>,
    confirm_delete: bool,
    enc: EncodeForm,
    job: Option<Job>,
    follow_log: bool,
    log_scroll: u16,
    tool_rows: Vec<ToolRow>,
    tool_selected: usize,
    status: String,
    quit: bool,
}

impl App {
    fn new(cfg: AppConfig, local_path: PathBuf, config_arg: Option<PathBuf>) -> Self {
        let names: Vec<String> = cfg.encode.profiles.keys().cloned().collect();
        let default_idx = cfg
            .encode
            .profile
            .as_ref()
            .and_then(|p| names.iter().position(|n| n == p))
            .unwrap_or(0);
        let enc = EncodeForm {
            profile: default_idx,
            native: cfg.encode.engine != "staxrip",
            source: String::new(),
            dest: String::new(),
            only: String::new(),
            jobs: cfg.encode.jobs.max(1).to_string(),
            dry_run: true,
            field: 0,
            editing: None,
        };
        Self {
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("subx")),
            cfg,
            local_path,
            config_arg,
            tab: Tab::Profiles,
            names,
            selected: default_idx,
            form: None,
            confirm_delete: false,
            enc,
            job: None,
            follow_log: true,
            log_scroll: 0,
            tool_rows: Vec::new(),
            tool_selected: 0,
            status: "↑/↓ select · Enter edit · n new · d delete · * default · Tab switch · q quit"
                .to_string(),
            quit: false,
        }
    }

    fn reload(&mut self) {
        self.cfg = AppConfig::load(self.config_arg.as_deref());
        self.names = self.cfg.encode.profiles.keys().cloned().collect();
        self.selected = self.selected.min(self.names.len().saturating_sub(1));
        self.enc.profile = self.enc.profile.min(self.names.len().saturating_sub(1));
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            if let Some(job) = &mut self.job
                && job.poll() > 0
                && self.follow_log
            {
                self.log_scroll = u16::MAX;
            }
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.on_key(key);
            }
        }
        Ok(())
    }

    // --- input ------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        // Text entry owns the keyboard while a buffer is open.
        if let Some(form) = &mut self.form
            && form.editing.is_some()
        {
            match key.code {
                KeyCode::Enter => match form.commit_edit() {
                    Ok(()) => self.status = "edited (s to save)".to_string(),
                    Err(e) => self.status = format!("invalid: {e}"),
                },
                KeyCode::Esc => form.cancel_edit(),
                KeyCode::Backspace => form.backspace(),
                KeyCode::Char(c) => form.type_char(c),
                _ => {}
            }
            return;
        }
        if let Some(buf) = &mut self.enc.editing {
            match key.code {
                KeyCode::Enter => {
                    let text = buf.clone();
                    let field = self.enc.selected();
                    self.enc.set_text(field, text);
                    self.enc.editing = None;
                }
                KeyCode::Esc => self.enc.editing = None,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            }
            return;
        }
        if self.confirm_delete {
            self.confirm_delete = false;
            if key.code == KeyCode::Char('y') {
                self.delete_selected();
            } else {
                self.status = "delete cancelled".to_string();
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab => self.switch_tab(1),
            KeyCode::BackTab => self.switch_tab(-1),
            KeyCode::Char('1') => self.set_tab(Tab::Profiles),
            KeyCode::Char('2') => self.set_tab(Tab::Encode),
            KeyCode::Char('3') => self.set_tab(Tab::Tools),
            _ => match self.tab {
                Tab::Profiles => self.on_profiles_key(key),
                Tab::Encode => self.on_encode_key(key),
                Tab::Tools => self.on_tools_key(key),
            },
        }
    }

    fn switch_tab(&mut self, step: i32) {
        let idx = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0) as i32;
        let next = (idx + step).rem_euclid(Tab::ALL.len() as i32) as usize;
        self.set_tab(Tab::ALL[next]);
    }

    fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.status = match tab {
            Tab::Profiles => {
                "↑/↓ select · Enter edit · n new · d delete · * default · Tab switch · q quit"
            }
            Tab::Encode => "↑/↓ field · Enter type · ←/→ toggle · r run · c cancel · PgUp/PgDn log",
            Tab::Tools => "↑/↓ select · i install · R refresh",
        }
        .to_string();
        if tab == Tab::Tools && self.tool_rows.is_empty() {
            self.refresh_tools();
        }
    }

    fn on_profiles_key(&mut self, key: KeyEvent) {
        if let Some(form) = &mut self.form {
            match key.code {
                KeyCode::Up => form.prev_field(),
                KeyCode::Down => form.next_field(),
                KeyCode::Left => form.cycle(-1),
                KeyCode::Right => form.cycle(1),
                KeyCode::Enter => form.begin_edit(),
                KeyCode::Char('s') => self.save_form(false),
                KeyCode::Char('S') => self.save_form(true),
                KeyCode::Esc => {
                    let dirty = form.dirty;
                    self.form = None;
                    self.status = if dirty {
                        "closed without saving".to_string()
                    } else {
                        "closed".to_string()
                    };
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                if self.selected + 1 < self.names.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(name) = self.names.get(self.selected)
                    && let Some(p) = self.cfg.encode.profiles.get(name)
                {
                    self.form = Some(Form::new(name, p.clone()));
                    self.status =
                        "↑/↓ field · Enter type/confirm · ←/→ choice · s save · S save as default · Esc close"
                            .to_string();
                }
            }
            KeyCode::Char('n') => {
                self.form = Some(Form::blank());
                self.status = "new profile: set the name first, then s to save".to_string();
            }
            KeyCode::Char('d') => {
                if !self.names.is_empty() {
                    self.confirm_delete = true;
                    self.status = format!(
                        "delete profile '{}'? y to confirm",
                        self.names[self.selected]
                    );
                }
            }
            KeyCode::Char('*') => {
                if let Some(name) = self.names.get(self.selected).cloned() {
                    match profiles::set_default_profile(&self.local_path, &name) {
                        Ok(()) => {
                            self.status = format!("default profile = {name}");
                            self.reload();
                        }
                        Err(e) => self.status = format!("error: {e:#}"),
                    }
                }
            }
            _ => {}
        }
    }

    fn save_form(&mut self, make_default: bool) {
        let Some(form) = &self.form else {
            return;
        };
        if let Err(e) = form.validate() {
            self.status = format!("invalid: {e}");
            return;
        }
        match profiles::save_profile(&self.local_path, &form.name, &form.profile, make_default) {
            Ok(()) => {
                let name = form.name.clone();
                self.status = format!("saved {name} -> {}", self.local_path.display());
                self.reload();
                self.selected = self.names.iter().position(|n| *n == name).unwrap_or(0);
                if let Some(f) = &mut self.form {
                    f.dirty = false;
                }
            }
            Err(e) => self.status = format!("error: {e:#}"),
        }
    }

    fn delete_selected(&mut self) {
        let Some(name) = self.names.get(self.selected).cloned() else {
            return;
        };
        match profiles::delete_profile(&self.local_path, &name) {
            Ok(true) => {
                self.status = format!("deleted {name}");
                self.reload();
            }
            Ok(false) => {
                self.status = format!(
                    "{name} is not defined in {} (edit the file it comes from)",
                    self.local_path.display()
                );
            }
            Err(e) => self.status = format!("error: {e:#}"),
        }
    }

    fn on_encode_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.enc.field = self.enc.field.saturating_sub(1),
            KeyCode::Down => self.enc.field = (self.enc.field + 1).min(EncField::ALL.len() - 1),
            KeyCode::Enter => {
                let field = self.enc.selected();
                if field.is_text() {
                    self.enc.editing = Some(self.enc.value(field, &self.names));
                } else {
                    self.toggle_enc_field(1);
                }
            }
            KeyCode::Left => self.toggle_enc_field(-1),
            KeyCode::Right => self.toggle_enc_field(1),
            KeyCode::Char('r') => self.start_encode(),
            KeyCode::Char('c') => {
                if let Some(job) = &mut self.job {
                    job.kill();
                    self.status = "cancelled".to_string();
                }
            }
            KeyCode::PageUp => {
                self.follow_log = false;
                self.log_scroll = self.log_scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.log_scroll = self.log_scroll.saturating_add(10);
            }
            KeyCode::End => {
                self.follow_log = true;
                self.log_scroll = u16::MAX;
            }
            _ => {}
        }
    }

    fn toggle_enc_field(&mut self, step: i32) {
        match self.enc.selected() {
            EncField::Profile if !self.names.is_empty() => {
                let n = self.names.len() as i32;
                self.enc.profile = (self.enc.profile as i32 + step).rem_euclid(n) as usize;
            }
            EncField::Engine => self.enc.native = !self.enc.native,
            EncField::DryRun => self.enc.dry_run = !self.enc.dry_run,
            _ => {}
        }
    }

    fn child_args(&self, args: Vec<String>) -> Vec<String> {
        let mut all = Vec::new();
        if let Some(c) = &self.config_arg {
            all.push("--config".to_string());
            all.push(c.display().to_string());
        }
        all.extend(args);
        all
    }

    fn start_encode(&mut self) {
        if self.job.as_ref().is_some_and(Job::running) {
            self.status = "a job is still running (c to cancel)".to_string();
            return;
        }
        let Some(args) = self.enc.args(&self.names) else {
            self.status = "need a profile, a source folder and an output folder".to_string();
            return;
        };
        self.launch(args, "encode started");
    }

    fn launch(&mut self, args: Vec<String>, note: &str) {
        let args = self.child_args(args);
        match Job::spawn(&self.exe, &args) {
            Ok(job) => {
                self.job = Some(job);
                self.follow_log = true;
                self.log_scroll = u16::MAX;
                self.status = note.to_string();
            }
            Err(e) => self.status = format!("cannot start {}: {e}", self.exe.display()),
        }
    }

    fn refresh_tools(&mut self) {
        let resolver = tools::resolver();
        self.tool_rows = Tool::ALL
            .into_iter()
            .map(|tool| match resolver.probe(tool) {
                Some(f) if f.usable => ToolRow {
                    tool,
                    status: "ok",
                    version: f.version.unwrap_or_else(|| "?".to_string()),
                    location: format!("{} ({})", f.path.display(), f.origin),
                },
                Some(f) => ToolRow {
                    tool,
                    status: "unusable",
                    version: f.version.unwrap_or_else(|| "?".to_string()),
                    location: f.detail.unwrap_or_else(|| f.path.display().to_string()),
                },
                None => ToolRow {
                    tool,
                    status: "missing",
                    version: "-".to_string(),
                    location: if tool.source().is_some() {
                        format!("i to install · {}", tool.link())
                    } else {
                        tool.link().to_string()
                    },
                },
            })
            .collect();
        self.status = format!(
            "{} tool(s) ok · ↑/↓ select · i install · R refresh",
            self.tool_rows.iter().filter(|r| r.status == "ok").count()
        );
    }

    fn on_tools_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.tool_selected = self.tool_selected.saturating_sub(1),
            KeyCode::Down => {
                self.tool_selected = (self.tool_selected + 1).min(Tool::ALL.len() - 1);
            }
            KeyCode::Char('R') => self.refresh_tools(),
            KeyCode::Char('i') => {
                if self.job.as_ref().is_some_and(Job::running) {
                    self.status = "a job is still running".to_string();
                    return;
                }
                let tool = Tool::ALL[self.tool_selected];
                if tool.source().is_none() {
                    self.status =
                        format!("{} has no download here; see {}", tool.name(), tool.link());
                    return;
                }
                let args = vec![
                    "tools".to_string(),
                    "install".to_string(),
                    tool.name().to_string(),
                ];
                self.launch(args, &format!("installing {}", tool.name()));
            }
            KeyCode::Char('c') => {
                if let Some(job) = &mut self.job {
                    job.kill();
                }
            }
            _ => {}
        }
    }

    // --- drawing ----------------------------------------------------------

    fn draw(&self, frame: &mut Frame) {
        let [tabs_area, body, status_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let titles: Vec<Line> = Tab::ALL.iter().map(|t| Line::from(t.title())).collect();
        let tabs = Tabs::new(titles)
            .select(Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0))
            .highlight_style(Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            .divider(" │ ");
        frame.render_widget(tabs, tabs_area);

        match self.tab {
            Tab::Profiles => self.draw_profiles(frame, body),
            Tab::Encode => self.draw_encode(frame, body),
            Tab::Tools => self.draw_tools(frame, body),
        }

        let status = Paragraph::new(Line::from(vec![
            Span::styled(" subx ", Style::new().fg(Color::Black).bg(Color::Yellow)),
            Span::raw(" "),
            Span::raw(self.status.as_str()),
        ]));
        frame.render_widget(status, status_area);
    }

    fn draw_profiles(&self, frame: &mut Frame, area: Rect) {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)])
                .areas(area);

        let default = self.cfg.encode.profile.as_deref();
        let items: Vec<ListItem> = self
            .names
            .iter()
            .map(|n| {
                let mark = if Some(n.as_str()) == default {
                    "* "
                } else {
                    "  "
                };
                ListItem::new(format!("{mark}{n}"))
            })
            .collect();
        let list = List::new(items)
            .block(Block::bordered().title(format!(" profiles ({}) ", self.local_path.display())))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");
        let mut state = ListState::default();
        if !self.names.is_empty() {
            state.select(Some(self.selected));
        }
        frame.render_stateful_widget(list, left, &mut state);

        match &self.form {
            Some(form) => self.draw_form(frame, right, form),
            None => {
                let text = match self.names.get(self.selected) {
                    Some(name) => summary_lines(name, &self.cfg),
                    None => vec![
                        Line::from("No profiles yet."),
                        Line::from(""),
                        Line::from(
                            "Press n to create one. Profiles are written to the local config",
                        ),
                        Line::from("and used by `subx encode --profile <name>`."),
                    ],
                };
                frame.render_widget(
                    Paragraph::new(text)
                        .block(Block::bordered().title(" profile "))
                        .wrap(Wrap { trim: false }),
                    right,
                );
            }
        }
    }

    fn draw_form(&self, frame: &mut Frame, area: Rect, form: &Form) {
        let [table_area, hint_area] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(area);
        let rows: Vec<Row> = Field::ALL
            .iter()
            .enumerate()
            .map(|(i, field)| {
                let selected = i == form.field;
                let value = match (&form.editing, selected) {
                    (Some(buf), true) => format!("{buf}▏"),
                    _ => form.value(*field),
                };
                let value = if field.choices().is_some() {
                    format!("◂ {value} ▸")
                } else {
                    value
                };
                let style = if selected {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new()
                };
                Row::new(vec![Cell::from(field.label()), Cell::from(value)]).style(style)
            })
            .collect();
        let title = format!(
            " {}{} ",
            form.name,
            if form.dirty { " (unsaved)" } else { "" }
        );
        let table = Table::new(rows, [Constraint::Length(13), Constraint::Min(10)])
            .block(Block::bordered().title(title));
        frame.render_widget(table, table_area);
        let hint = Paragraph::new(form.selected().hint())
            .style(Style::new().fg(Color::DarkGray))
            .wrap(Wrap { trim: true });
        frame.render_widget(hint, hint_area);
    }

    fn draw_encode(&self, frame: &mut Frame, area: Rect) {
        let [form_area, log_area] = Layout::vertical([
            Constraint::Length(EncField::ALL.len() as u16 + 2),
            Constraint::Min(3),
        ])
        .areas(area);
        let rows: Vec<Row> = EncField::ALL
            .iter()
            .enumerate()
            .map(|(i, field)| {
                let selected = i == self.enc.field;
                let value = match (&self.enc.editing, selected) {
                    (Some(buf), true) => format!("{buf}▏"),
                    _ => self.enc.value(*field, &self.names),
                };
                let value = if field.is_text() {
                    value
                } else {
                    format!("◂ {value} ▸")
                };
                let style = if selected {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new()
                };
                Row::new(vec![Cell::from(field.label()), Cell::from(value)]).style(style)
            })
            .collect();
        let table = Table::new(rows, [Constraint::Length(15), Constraint::Min(10)])
            .block(Block::bordered().title(" encode (r to run) "));
        frame.render_widget(table, form_area);
        self.draw_log(frame, log_area);
    }

    fn draw_tools(&self, frame: &mut Frame, area: Rect) {
        let [table_area, log_area] = Layout::vertical([
            Constraint::Length(Tool::ALL.len() as u16 + 3),
            Constraint::Min(3),
        ])
        .areas(area);
        let rows: Vec<Row> = self
            .tool_rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let color = match r.status {
                    "ok" => Color::Green,
                    "unusable" => Color::Yellow,
                    _ => Color::Red,
                };
                let mut style = Style::new();
                if i == self.tool_selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                Row::new(vec![
                    Cell::from(r.tool.name()),
                    Cell::from(Span::styled(r.status, Style::new().fg(color))),
                    Cell::from(r.version.clone()),
                    Cell::from(r.location.clone()),
                ])
                .style(style)
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(11),
                Constraint::Length(9),
                Constraint::Length(26),
                Constraint::Min(20),
            ],
        )
        .header(Row::new(vec!["TOOL", "STATUS", "VERSION", "LOCATION"]).bold())
        .block(Block::bordered().title(" tools "));
        frame.render_widget(table, table_area);
        self.draw_log(frame, log_area);
    }

    fn draw_log(&self, frame: &mut Frame, area: Rect) {
        let (title, lines): (String, Vec<Line>) = match &self.job {
            Some(job) => {
                let state = match job.exit {
                    None => "running".to_string(),
                    Some(0) => "done".to_string(),
                    Some(code) => format!("exit {code}"),
                };
                (
                    format!(" {} — {} ", state, job.command),
                    job.lines.iter().map(|l| Line::raw(l.as_str())).collect(),
                )
            }
            None => (
                " log ".to_string(),
                vec![Line::from(
                    "Output of the last job appears here (encode: r · install: i).",
                )],
            ),
        };
        let inner_height = area.height.saturating_sub(2) as usize;
        let max_scroll = lines.len().saturating_sub(inner_height) as u16;
        let scroll = self.log_scroll.min(max_scroll);
        let paragraph = Paragraph::new(lines)
            .block(Block::bordered().title(title))
            .scroll((scroll, 0));
        frame.render_widget(paragraph, area);
    }
}

/// Read-only description of a saved profile.
fn summary_lines(name: &str, cfg: &AppConfig) -> Vec<Line<'static>> {
    let Some(p) = cfg.encode.profiles.get(name) else {
        return vec![Line::from("profile not found")];
    };
    let kv = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("{k:<13}"), Style::new().fg(Color::DarkGray)),
            Span::raw(v),
        ])
    };
    vec![
        kv("name", name.to_string()),
        kv("tag", p.tag.clone().unwrap_or_default()),
        kv(
            "size",
            if p.width == 0 {
                "source".to_string()
            } else {
                format!("{}x{}", p.width, p.height)
            },
        ),
        kv("encoder", p.encoder.clone()),
        kv("video args", p.video.join(" ")),
        kv("audio", profiles::audio_summary(&p.audio)),
        kv("color", p.color.clone()),
        kv("crop", p.crop.clone()),
        kv(
            "source",
            match &p.script {
                Some(s) => format!("{} ({})", p.source, s.display()),
                None => p.source.clone(),
            },
        ),
        Line::from(""),
        Line::from(Span::styled(
            "Enter to edit · n new · d delete · * make default",
            Style::new().fg(Color::DarkGray),
        )),
    ]
}

/// Run the TUI until the user quits.
pub fn run(cfg: AppConfig, local_path: PathBuf, config_arg: Option<PathBuf>) -> Result<()> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        bail!("`subx tui` needs an interactive terminal (stdin/stdout are redirected)");
    }
    let mut terminal = ratatui::init();
    let result = App::new(cfg, local_path, config_arg).run(&mut terminal);
    ratatui::restore();
    result
}

/// Where profile edits go: `--config` when given, else the existing local
/// file next to the executable or in the current folder, else a new one in
/// the current folder.
pub fn default_local_path(config_arg: Option<&Path>) -> PathBuf {
    if let Some(c) = config_arg {
        return c.to_path_buf();
    }
    let cwd = PathBuf::from("subx.local.toml");
    if cwd.exists() {
        return cwd;
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        let next_to_exe = dir.join("subx.local.toml");
        if next_to_exe.exists() {
            return next_to_exe;
        }
    }
    cwd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut cfg = AppConfig::default();
        cfg.encode
            .profiles
            .insert("a480".into(), Default::default());
        cfg.encode
            .profiles
            .insert("hevc".into(), Default::default());
        cfg.encode.profile = Some("hevc".into());
        App::new(cfg, PathBuf::from("x.toml"), None)
    }

    #[test]
    fn encode_form_builds_cli_arguments() {
        let mut app = app();
        assert_eq!(app.enc.profile, 1, "default profile preselected");
        assert!(app.enc.args(&app.names).is_none(), "folders required");
        app.enc.source = "S".into();
        app.enc.dest = "D".into();
        app.enc.only = "1-3".into();
        app.enc.jobs = "2".into();
        let args = app.enc.args(&app.names).unwrap();
        assert_eq!(
            args,
            [
                "encode",
                "S",
                "D",
                "--profile",
                "hevc",
                "--engine",
                "native",
                "--only",
                "1-3",
                "-j",
                "2",
                "--dry-run"
            ]
        );
        app.enc.dry_run = false;
        app.enc.native = false;
        app.enc.jobs = "1".into();
        let args = app.enc.args(&app.names).unwrap();
        assert!(!args.contains(&"--dry-run".to_string()) && !args.contains(&"-j".to_string()));
        assert!(args.contains(&"staxrip".to_string()));
        app.config_arg = Some(PathBuf::from("c.toml"));
        let all = app.child_args(args);
        assert_eq!(&all[..3], ["--config", "c.toml", "encode"]);
    }

    #[test]
    fn keys_drive_tabs_forms_and_text_entry() {
        let mut app = app();
        let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);

        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Encode);
        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(app.tab, Tab::Profiles);

        // Open the selected profile, move to the encoder field and cycle it.
        app.on_key(key(KeyCode::Enter));
        assert!(app.form.is_some());
        for _ in 0..4 {
            app.on_key(key(KeyCode::Down));
        }
        assert_eq!(app.form.as_ref().unwrap().selected(), Field::Encoder);
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.form.as_ref().unwrap().profile.encoder, "x265");

        // Type into the tag field, confirm, then close without saving.
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.form.as_ref().unwrap().selected(), Field::Tag);
        app.on_key(key(KeyCode::Enter));
        for c in "BD".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        // While typing, 'q' is text, not quit.
        app.on_key(key(KeyCode::Char('q')));
        assert!(!app.quit);
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.form.as_ref().unwrap().profile.tag.as_deref(),
            Some("BD")
        );
        app.on_key(key(KeyCode::Esc));
        assert!(app.form.is_none());
        assert!(app.status.contains("without saving"));

        // Encode tab text entry and toggles.
        app.on_key(key(KeyCode::Char('2')));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Right));
        assert!(!app.enc.native);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        for c in "C:/src".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.enc.source, "C:/src");
        app.on_key(key(KeyCode::Char('r')));
        assert!(app.status.contains("output folder"), "{}", app.status);

        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    #[test]
    fn delete_asks_for_confirmation() {
        let mut app = app();
        let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        app.on_key(key(KeyCode::Char('d')));
        assert!(app.confirm_delete);
        app.on_key(key(KeyCode::Char('n')));
        assert!(!app.confirm_delete);
        assert!(app.status.contains("cancelled"));
        // The profiles live in the in-memory config only, so confirming reports that.
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('y')));
        assert!(app.status.contains("not defined"), "{}", app.status);
    }

    #[test]
    fn renders_every_tab_without_panicking() {
        let mut app = app();
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        for tab in Tab::ALL {
            app.tab = tab;
            if tab == Tab::Tools {
                app.tool_rows = vec![ToolRow {
                    tool: Tool::Ffmpeg,
                    status: "ok",
                    version: "n9.0.2".into(),
                    location: "tools/ffmpeg (tools dir)".into(),
                }];
            }
            terminal.draw(|f| app.draw(f)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("Profiles") && text.contains("Encode") && text.contains("Tools"));
            match tab {
                Tab::Profiles => assert!(text.contains("hevc") && text.contains("* hevc")),
                Tab::Encode => assert!(text.contains("source folder") && text.contains("dry run")),
                Tab::Tools => assert!(text.contains("n9.0.2")),
            }
        }
        app.form = Some(Form::blank());
        app.tab = Tab::Profiles;
        terminal.draw(|f| app.draw(f)).unwrap();
    }

    #[test]
    fn local_path_prefers_the_config_flag() {
        assert_eq!(
            default_local_path(Some(Path::new("x/y.toml"))),
            PathBuf::from("x/y.toml")
        );
        assert!(default_local_path(None).ends_with("subx.local.toml"));
    }
}
