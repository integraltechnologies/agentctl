//! Terminal consumer of the shared observability projection; no SQL or orchestration.
use super::{
    Error, Result,
    observe::{
        self, Snapshot,
        usage::{self, Scope},
    },
    paths::{MachinePaths, PathContext},
};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    prelude::*,
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph, Wrap},
};
use std::{
    io::{self, IsTerminal},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Up,
    Down,
    Panel,
    Session,
    Graph,
    Inspect,
    Help,
}
#[derive(Default)]
pub struct App {
    pub snapshot: Snapshot,
    pub session_index: usize,
    pub selected: usize,
    pub panel: usize,
    pub scope_index: usize,
    pub inspect: bool,
    pub help: bool,
    pub error: Option<String>,
}
impl App {
    pub fn new(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            ..Default::default()
        }
    }
    pub fn session_id(&self) -> Option<&str> {
        self.snapshot
            .sessions
            .get(self.session_index)
            .map(|s| s.id.as_str())
    }
    pub fn agents(&self) -> Vec<(usize, usize)> {
        observe::tree(&self.snapshot, self.session_id())
    }
    pub fn tasks(&self) -> Vec<usize> {
        let plan = self
            .snapshot
            .sessions
            .get(self.session_index)
            .and_then(|s| s.current_plan.as_deref());
        self.snapshot
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.session_id.as_deref() == self.session_id() && plan.is_none_or(|p| t.plan_id == p)
            })
            .map(|(i, _)| i)
            .collect()
    }
    pub fn scopes(&self) -> Vec<Scope> {
        use std::collections::BTreeSet;
        let mut scopes = vec![Scope::Aggregate];
        for values in [0, 1, 2] {
            let names: BTreeSet<_> = self
                .snapshot
                .usage
                .iter()
                .filter_map(|u| match values {
                    0 => u.provider.clone(),
                    1 => u.task_id.clone(),
                    _ => u.role.clone(),
                })
                .collect();
            scopes.extend(names.into_iter().map(|v| match values {
                0 => Scope::Provider(v),
                1 => Scope::Task(v),
                _ => Scope::Role(v),
            }));
        }
        scopes
    }
    pub fn graph(&self) -> usage::Series {
        let scopes = self.scopes();
        // Aggregate is machine-wide across sessions; other scopes are equally
        // explicit machine-wide filters, independent of the selected tree.
        usage::series(
            &self.snapshot,
            scopes[self.scope_index % scopes.len()].clone(),
            None,
        )
    }
    pub fn action(&mut self, action: Action) {
        match action {
            Action::Up => self.selected = self.selected.saturating_sub(1),
            Action::Down => {
                let n = if self.panel == 0 {
                    self.agents().len()
                } else {
                    self.tasks().len()
                };
                self.selected = (self.selected + 1).min(n.saturating_sub(1));
            }
            Action::Panel => {
                self.panel = (self.panel + 1) % 2;
                self.selected = 0;
            }
            Action::Session => {
                self.session_index = (self.session_index + 1) % (self.snapshot.sessions.len() + 1);
                self.selected = 0;
            }
            Action::Graph => self.scope_index = (self.scope_index + 1) % self.scopes().len(),
            Action::Inspect => self.inspect = !self.inspect,
            Action::Help => self.help = !self.help,
        }
    }
    pub fn replace(&mut self, snapshot: Snapshot) {
        let session = self.session_id().map(str::to_owned);
        let selected_id = if self.panel == 0 {
            self.agents()
                .get(self.selected)
                .map(|(i, _)| self.snapshot.agents[*i].id.clone())
        } else {
            self.tasks()
                .get(self.selected)
                .map(|i| self.snapshot.tasks[*i].id.clone())
        };
        let scopes = self.scopes();
        let scope = scopes[self.scope_index % scopes.len()].clone();
        self.snapshot = snapshot;
        self.session_index = session
            .and_then(|id| self.snapshot.sessions.iter().position(|s| s.id == id))
            .unwrap_or_else(|| self.session_index.min(self.snapshot.sessions.len()));
        self.scope_index = self.scopes().iter().position(|s| s == &scope).unwrap_or(0);
        self.selected = if self.panel == 0 {
            self.agents()
                .iter()
                .position(|(i, _)| Some(&self.snapshot.agents[*i].id) == selected_id.as_ref())
        } else {
            self.tasks()
                .iter()
                .position(|i| Some(&self.snapshot.tasks[*i].id) == selected_id.as_ref())
        }
        .unwrap_or(0);
    }
}
fn block(title: impl Into<String>) -> Block<'static> {
    Block::default().title(title.into()).borders(Borders::ALL)
}
fn compact(s: &str) -> String {
    s.chars()
        .rev()
        .take(12)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}
fn age(now: u64, timestamp: Option<u64>) -> String {
    timestamp
        .map(|t| format!("{}s", now.saturating_sub(t) / 1000))
        .unwrap_or_else(|| "?".into())
}

pub fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    if area.width < 35 || area.height < 12 {
        frame.render_widget(
            Paragraph::new(
                "agenttop: terminal too small\nResize to at least 35x12\nq quit | ? help",
            ),
            area,
        );
        return;
    }
    if app.help {
        frame.render_widget(Paragraph::new("agenttop — read-only engineering activity\n\nq / Esc / Ctrl-C quit\nUp/Down select; Tab agents/tasks\nEnter toggle expanded probe\ns next session (includes unowned history)\ng next graph scope: aggregate/provider/task/role\nr refresh; ? close help\n\nGraph: observed deltas/min, last 10 minutes.\nGaps mean UNKNOWN, not zero. End-of-job usage arrives in bursts.\nRUNNING is lifecycle; LIVE requires current-process child evidence. Separate observers show UNKNOWN.\nSilence is time since an event, not inferred model idle time.\nNo percentages, model calls, or control actions.\n\nASCII labels and Unicode terminal borders; no special font required.").block(block("Help")).wrap(Wrap{trim:true}),area);
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(if area.height < 24 { 6 } else { 8 }),
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(if area.height < 24 { 1 } else { 3 }),
        Constraint::Length(1),
    ])
    .split(area);
    let series = app.graph();
    let current = series
        .points
        .last()
        .and_then(|p| p.tokens_per_minute)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "UNKNOWN".into());
    let peak = series
        .points
        .iter()
        .filter_map(|p| p.tokens_per_minute)
        .max();
    let title = format!(
        "TOKENS/min observed | {:?} | now {current} | {}{}",
        series.scope,
        series.quality,
        if series.incomplete { " [bounded]" } else { "" }
    );
    if let Some(peak) = peak {
        let mut segments = vec![vec![]];
        for p in &series.points {
            if let Some(n) = p.tokens_per_minute {
                segments.last_mut().expect("segment").push((
                    -(app.snapshot.at_ms.saturating_sub(p.at_ms) as f64) / 60000.0,
                    n as f64,
                ));
            } else if !segments.last().expect("segment").is_empty() {
                segments.push(vec![]);
            }
        }
        let datasets = segments
            .iter()
            .filter(|s| !s.is_empty())
            .map(|points| {
                Dataset::default()
                    .graph_type(GraphType::Line)
                    .marker(symbols::Marker::Braille)
                    .style(Style::default().fg(Color::Cyan))
                    .data(points)
            })
            .collect();
        frame.render_widget(
            Chart::new(datasets)
                .block(block(title))
                .x_axis(Axis::default().bounds([-10.0, 0.0]).labels(["-10m", "now"]))
                .y_axis(
                    Axis::default()
                        .bounds([0.0, peak.max(1) as f64])
                        .labels(["0".to_owned(), peak.to_string()]),
                ),
            rows[0],
        );
    } else {
        frame.render_widget(Paragraph::new("UNKNOWN — no token observations in this window\nNo provider polling or invented zero usage").block(block(title)),rows[0]);
    }
    let header = if let Some(s) = app.snapshot.sessions.get(app.session_index) {
        format!(
            "{} {}/{} {} {} — {}\n{} | {}{} | {}",
            if s.ownership_known {
                "SESSION"
            } else {
                "UNOWNED PLAN"
            },
            app.session_index + 1,
            app.snapshot.sessions.len(),
            compact(&s.id),
            s.state,
            s.title,
            s.activity,
            s.check.as_deref().unwrap_or(""),
            s.blocker
                .as_ref()
                .map(|b| format!(" {}", b.description))
                .unwrap_or_default(),
            s.root
        )
    } else if app.snapshot.sessions.is_empty() && app.snapshot.agents.is_empty() {
        "No engineering sessions or agents. Observation never starts work.".into()
    } else {
        "Unowned/legacy history — no session ownership inferred".into()
    };
    frame.render_widget(Paragraph::new(header), rows[1]);
    let columns = if area.width >= 90 {
        Layout::horizontal([Constraint::Percentage(53), Constraint::Percentage(47)])
            .split(rows[2])
            .to_vec()
    } else {
        vec![rows[2]]
    };
    let agents = app.agents();
    let tasks = app.tasks();
    let selected_style = Style::default().fg(Color::Black).bg(Color::Cyan);
    let agent_lines: Vec<Line> = agents
        .iter()
        .enumerate()
        .skip(if app.panel == 0 {
            app.selected
                .saturating_sub(columns[0].height.saturating_sub(4) as usize)
        } else {
            0
        })
        .map(|(n, (i, depth))| {
            let a = &app.snapshot.agents[*i];
            let line = format!(
                "{}{} {} {} {} {} {}{}",
                "  ".repeat((*depth).min(5)),
                a.role,
                format_args!(
                    "{}/{}",
                    a.verification.as_deref().unwrap_or(&a.state),
                    a.liveness.as_str()
                ),
                a.task_id.clone().unwrap_or_else(|| compact(&a.job_id)),
                age(
                    a.finished_at_ms.unwrap_or(app.snapshot.at_ms),
                    a.started_at_ms
                ),
                a.provider.as_deref().unwrap_or("?"),
                a.model.as_deref().unwrap_or("?"),
                if a.ownership_uncertain {
                    " [parent?]"
                } else {
                    ""
                }
            );
            Line::styled(
                line,
                if app.panel == 0 && n == app.selected {
                    selected_style
                } else {
                    Style::default()
                },
            )
        })
        .collect();
    let progress = app
        .snapshot
        .sessions
        .get(app.session_index)
        .map(|s| {
            if s.progress_complete {
                format!("{}/{} VERIFIED", s.verified, s.task_count)
            } else {
                "partial task view".into()
            }
        })
        .unwrap_or_default();
    let task_lines: Vec<Line> = tasks
        .iter()
        .enumerate()
        .skip(if app.panel == 1 {
            app.selected
                .saturating_sub(columns[0].height.saturating_sub(4) as usize)
        } else {
            0
        })
        .map(|(n, i)| {
            let t = &app.snapshot.tasks[*i];
            Line::styled(
                format!(
                    "{} {} {}",
                    t.id,
                    t.presentation,
                    t.blocker
                        .as_ref()
                        .map(|b| if b.dependencies.is_empty() {
                            b.description.clone()
                        } else {
                            format!("<- {}", b.dependencies.join(","))
                        })
                        .unwrap_or_default()
                ),
                if app.panel == 1 && n == app.selected {
                    selected_style
                } else {
                    Style::default()
                },
            )
        })
        .collect();
    if columns.len() == 2 && !app.inspect {
        frame.render_widget(
            Paragraph::new(agent_lines).block(block("AGENTS [Tab]")),
            columns[0],
        );
        let right = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(columns[1]);
        frame.render_widget(
            Paragraph::new(task_lines).block(block(format!("TASKS {progress}"))),
            right[0],
        );
        frame.render_widget(
            Paragraph::new(detail(app))
                .block(block("PROBE [Enter]"))
                .wrap(Wrap { trim: true }),
            right[1],
        );
    } else if app.inspect {
        frame.render_widget(
            Paragraph::new(detail(app))
                .block(block("PROBE [Enter to return]"))
                .wrap(Wrap { trim: true }),
            rows[2],
        );
    } else if app.panel == 0 {
        frame.render_widget(
            Paragraph::new(agent_lines).block(block("AGENTS [Tab tasks / Enter probe]")),
            columns[0],
        );
    } else {
        frame.render_widget(
            Paragraph::new(task_lines).block(block(format!("TASKS {progress}"))),
            columns[0],
        );
    }
    let recent = app
        .snapshot
        .events
        .iter()
        .rev()
        .filter(|e| {
            app.snapshot
                .sessions
                .get(app.session_index)
                .is_some_and(|s| {
                    e.repository_id == s.repository_id
                        && e.workspace_id.as_deref() == Some(&s.workspace_id)
                        && e.plan_id.as_ref().is_some_and(|p| s.plans.contains(p))
                })
        })
        .take(rows[3].height as usize)
        .map(|e| {
            format!(
                "{}s ago {} {}",
                app.snapshot.at_ms.saturating_sub(e.at_ms) / 1000,
                e.job_id.as_deref().map(compact).unwrap_or_default(),
                e.summary
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    frame.render_widget(Paragraph::new(recent), rows[3]);
    let footer=app.error.clone().or_else(||app.snapshot.warnings.first().cloned()).map(|message|format!("q quit | ? help | {message}")).unwrap_or_else(||"q quit | Tab panel | arrows select | Enter probe | g graph | s session | r refresh | ? help".into());
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::Yellow)),
        rows[4],
    );
}
fn detail(app: &App) -> String {
    if app.panel == 0 {
        if let Some((i, _)) = app.agents().get(app.selected) {
            let a = &app.snapshot.agents[*i];
            let usage: Vec<_> = app
                .snapshot
                .usage
                .iter()
                .filter(|u| u.agent_id.as_deref() == Some(&a.id))
                .collect();
            return format!(
                "{} / {} | Liveness {}\nLast known phase {} | proof {}\nElapsed {} | since event {} (not inferred idle)\nBlocker {}\nLast {}\nToken observations {}\nProvider {} / {}\nRoute {} from {} attempt {}\nAgent {}\nSession {}\nParent {}\nRepository {}\nWorkspace {}\nPlan {} / Task {}\nJob {}",
                a.role,
                a.state,
                a.liveness.as_str(),
                a.activity,
                a.verification.as_deref().unwrap_or("UNKNOWN"),
                age(
                    a.finished_at_ms.unwrap_or(app.snapshot.at_ms),
                    a.started_at_ms
                ),
                age(app.snapshot.at_ms, a.last_event.as_ref().map(|e| e.at_ms)),
                a.blocker
                    .as_ref()
                    .map(|b| b.description.as_str())
                    .unwrap_or("none observed"),
                a.last_event
                    .as_ref()
                    .map(|e| e.summary.as_str())
                    .unwrap_or("UNKNOWN"),
                if usage.is_empty() {
                    "UNKNOWN".into()
                } else {
                    usage
                        .iter()
                        .map(|u| format!("{:?}: {:?}", u.provenance, u.total))
                        .collect::<Vec<_>>()
                        .join(", ")
                },
                a.provider.as_deref().unwrap_or("UNKNOWN"),
                a.model.as_deref().unwrap_or("UNKNOWN"),
                a.requested_role.as_deref().unwrap_or("historical"),
                a.route_origin.as_deref().unwrap_or("UNKNOWN"),
                a.route_attempt
                    .map(|n| format!(
                        "{n} {}",
                        a.fallback_reason
                            .as_deref()
                            .or(a.policy_skip_reason.as_deref())
                            .unwrap_or("")
                    ))
                    .unwrap_or_else(|| "UNKNOWN".into()),
                a.id,
                a.session_id.as_deref().unwrap_or("UNKNOWN"),
                a.parent_id.as_deref().unwrap_or("UNKNOWN"),
                a.repository_id,
                a.workspace_id,
                a.plan_id.as_deref().unwrap_or("-"),
                a.task_id.as_deref().unwrap_or("-"),
                a.job_id
            );
        }
    } else if let Some(i) = app.tasks().get(app.selected) {
        let t = &app.snapshot.tasks[*i];
        return format!(
            "{} — {}\nPlan {}\nSession {}\nCanonical {} / presentation {}\nDependencies {}\nBlocker {}\nExecutor {}\nVerifier {}\nAttempts in bounded view {}\nLast {}",
            t.id,
            t.objective,
            t.plan_id,
            t.session_id.as_deref().unwrap_or("UNKNOWN"),
            t.lifecycle,
            t.presentation,
            t.dependencies.join(","),
            t.blocker
                .as_ref()
                .map(|b| b.description.as_str())
                .unwrap_or("none observed"),
            t.executor_job.as_deref().unwrap_or("not created"),
            t.verifier_job.as_deref().unwrap_or("not created"),
            t.attempts_in_view,
            t.last_event
                .as_ref()
                .map(|e| e.summary.as_str())
                .unwrap_or("UNKNOWN")
        );
    }
    "No selection. No agents are launched by observation.".into()
}

pub fn render_text(app: &App, width: u16, height: u16) -> Result<String> {
    super::require(
        (1..=500).contains(&width) && (1..=200).contains(&height),
        "terminal dimensions must be 1..500 by 1..200",
    )?;
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height))?;
    terminal.draw(|f| render(f, app))?;
    let buffer = terminal.backend().buffer();
    Ok((0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n"))
}
struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}
pub fn run(args: &[String]) -> Result<()> {
    if args == ["--help"] {
        println!(
            "agenttop [--once] [--width N --height N]\nq quit; arrows select; Tab panel; Enter probe; g graph; s session; r refresh; ? help"
        );
        return Ok(());
    }
    let mut once = false;
    let mut width = 100;
    let mut height = 30;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--once" => once = true,
            "--width" | "--height" => {
                let value = args
                    .get(i + 1)
                    .and_then(|s| s.parse::<u16>().ok())
                    .ok_or_else(|| Error::Invalid("expected terminal dimension".into()))?;
                if args[i] == "--width" {
                    width = value
                } else {
                    height = value
                }
                i += 1;
            }
            _ => return Err(Error::Invalid("unknown agenttop option; use --help".into())),
        }
        i += 1;
    }
    let paths = MachinePaths::resolve(&PathContext::from_env())?;
    let mut app = App::default();
    let refresh = |app: &mut App| match observe::read(&paths, super::now_ms().unwrap_or(0)) {
        Ok(s) => {
            app.replace(s);
            app.error = None
        }
        Err(_) => {
            app.error =
                Some("Observation unavailable/busy; showing last snapshot. Retrying.".into())
        }
    };
    refresh(&mut app);
    if once {
        println!("{}", render_text(&app, width, height)?);
        return if app.error.is_some() {
            Err(Error::Invalid("observation unavailable".into()))
        } else {
            Ok(())
        };
    }
    super::require(
        io::stdout().is_terminal() && io::stdin().is_terminal(),
        "agenttop needs a terminal; use --once for noninteractive rendering",
    )?;
    enable_raw_mode()?;
    let _restore = Restore;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut last = Instant::now();
    let mut redraw = true;
    loop {
        if last.elapsed() >= Duration::from_secs(1) {
            refresh(&mut app);
            last = Instant::now();
            redraw = true;
        }
        if redraw {
            terminal.draw(|f| render(f, &app))?;
            redraw = false;
        }
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(event::KeyModifiers::CONTROL))
                    {
                        break;
                    }
                    let action = match key.code {
                        KeyCode::Up => Some(Action::Up),
                        KeyCode::Down => Some(Action::Down),
                        KeyCode::Tab => Some(Action::Panel),
                        KeyCode::Enter => Some(Action::Inspect),
                        KeyCode::Char('g') => Some(Action::Graph),
                        KeyCode::Char('s') => Some(Action::Session),
                        KeyCode::Char('?') => Some(Action::Help),
                        KeyCode::Char('r') => {
                            if last.elapsed() >= Duration::from_millis(500) {
                                refresh(&mut app);
                                last = Instant::now();
                            }
                            None
                        }
                        _ => None,
                    };
                    if let Some(action) = action {
                        app.action(action);
                    }
                    redraw = true;
                }
                Event::Resize(..) => redraw = true,
                _ => {}
            }
        }
    }
    Ok(())
}
