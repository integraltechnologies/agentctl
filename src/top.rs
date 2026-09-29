//! `agenttop`: a live, read-only monitor over the shared observation model.
//!
//! It consumes [`crate::observe`] and nothing else: no SQL of its own, no
//! second telemetry source, and no counters that accumulate across
//! refreshes. Every refresh recomputes everything from the canonical records
//! (usage records are re-read wholesale; only the recent-events ring is
//! incremental, by sequence number, so no event is seen twice), so nothing
//! is double counted.
//!
//! It writes nothing and never probes liveness (no session lock, no
//! recovery or barrier check): an invocation with no end recorded is shown
//! as such, with its usage pending, never as running or succeeded, and
//! unresolved records are shown as recorded. Unknown stays unknown:
//! pending and unavailable usage is never drawn as zero tokens, and
//! estimates are marked `~` and drawn as a separate series.
//!
//! Every string that can originate outside agentctl goes through
//! [`observe::untrusted`] before it becomes a span.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Sparkline, Table};
use ratatui::{Frame, Terminal};

use crate::observe::{self, Aggregate, Counts, Dimension, GroupKey, Overview};
use crate::project::Project;
use crate::state::{Event, EventQuery, PlanId, Store, TaskId, UsageRecord};

/// How many recent events are kept.
pub const EVENT_CAP: usize = 200;
/// How many rate windows the graph shows.
const WINDOWS: usize = 60;

/// What the monitor knows: the latest canonical reads, nothing derived that
/// outlives a refresh.
#[derive(Debug)]
pub struct Model {
    /// The last overview read successfully.
    pub overview: Option<Overview>,
    /// The project has no state store yet.
    pub no_state: bool,
    /// The last read failed; it is retried on the next refresh.
    pub error: Option<String>,
    /// The most recent events, oldest first, at most `event_cap`.
    pub events: VecDeque<Event>,
    /// The sequence number of the newest event seen.
    pub last_seq: Option<i64>,
    /// Milliseconds: the time of the last refresh, and the end of the
    /// newest rate window.
    pub now: i64,
    /// The project root, for the header.
    pub root: String,
    event_cap: usize,
}

impl Default for Model {
    fn default() -> Self {
        Self::with_event_cap(EVENT_CAP)
    }
}

impl Model {
    pub fn with_event_cap(event_cap: usize) -> Self {
        Self {
            overview: None,
            no_state: false,
            error: None,
            events: VecDeque::new(),
            last_seq: None,
            now: 0,
            root: String::new(),
            event_cap,
        }
    }

    /// Reads canonical state again. Every refresh drops the previously opened
    /// store and reopens it, so a removed or replaced `state.db` is noticed;
    /// no handle outlives a refresh except the last one, kept in `store`. The
    /// overview is recomputed from scratch and events after the newest one
    /// seen are fetched, after checking the store still holds that event (a
    /// replaced store resets the ring). A failure is recorded in `error` and
    /// affects nothing else.
    /// A failed open or read clears what was shown, so nothing stale is displayed as current.
    pub fn refresh(&mut self, project: &Project, store: &mut Option<Store>, now: i64) {
        self.now = now;
        self.root = project.root.display().to_string();
        *store = None;
        match project.observe() {
            Ok(Some(opened)) => *store = Some(opened),
            Ok(None) => {
                self.no_state = true;
                self.error = None;
                self.overview = None;
                self.events.clear();
                self.last_seq = None;
                return;
            }
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                self.overview = None;
                self.events.clear();
                self.last_seq = None;
                return;
            }
        }
        self.no_state = false;
        let Some(open) = store.as_ref() else { return };
        match self.read(open, project) {
            Ok(()) => self.error = None,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                self.overview = None;
                self.events.clear();
                self.last_seq = None;
                *store = None;
            }
        }
    }

    fn read(&mut self, store: &Store, project: &Project) -> Result<()> {
        if let Some(s) = self.last_seq {
            let probe = store.query_events(&EventQuery {
                after: Some(s.saturating_sub(1)),
                plan: None,
                task: None,
                agent: None,
                kind: None,
                limit: 1,
                newest: false,
            })?;
            if probe.first() != self.events.back() {
                self.events.clear();
                self.last_seq = None;
            }
        }
        let overview = observe::overview(store, project.config.agents.max_concurrency)?;
        let events = store.query_events(&EventQuery {
            after: self.last_seq,
            plan: None,
            task: None,
            agent: None,
            kind: None,
            limit: u32::try_from(self.event_cap).unwrap_or(u32::MAX),
            newest: true,
        })?;
        self.now = self.now.max(overview.taken_at);
        self.overview = Some(overview);
        if let Some(last) = events.last() {
            self.last_seq = Some(last.seq);
        }
        self.events.extend(events);
        while self.events.len() > self.event_cap {
            self.events.pop_front();
        }
        Ok(())
    }
}

/// The aggregation the table shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Aggregate,
    Provider,
    Plan,
    Task,
    Role,
    Agent,
}

impl View {
    pub const ALL: [View; 6] = [
        View::Aggregate,
        View::Provider,
        View::Plan,
        View::Task,
        View::Role,
        View::Agent,
    ];

    pub fn name(self) -> &'static str {
        match self {
            View::Aggregate => "aggregate",
            View::Provider => "provider",
            View::Plan => "plan",
            View::Task => "task",
            View::Role => "role",
            View::Agent => "agent",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|v| *v == self).unwrap_or(0)
    }

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// The view a key `1` to `6` selects.
    pub fn from_digit(c: char) -> Option<Self> {
        let n = c.to_digit(10)?;
        Self::ALL
            .get(usize::try_from(n).ok()?.checked_sub(1)?)
            .copied()
    }

    fn dimension(self) -> Option<Dimension> {
        match self {
            View::Aggregate => None,
            View::Provider => Some(Dimension::Provider),
            View::Plan => Some(Dimension::Plan),
            View::Task => Some(Dimension::Task),
            View::Role => Some(Dimension::Role),
            View::Agent => Some(Dimension::Agent),
        }
    }
}

/// What the user chose to look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ui {
    pub view: View,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            view: View::Aggregate,
        }
    }
}

/// What the user did, independent of the terminal library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Quit,
    Next,
    Prev,
    Select(View),
    /// Something that changes nothing but may change the screen (resize).
    Redraw,
}

impl Ui {
    /// Applies `input`; returns whether the user asked to quit.
    pub fn apply(&mut self, input: Input) -> bool {
        match input {
            Input::Quit => return true,
            Input::Next => self.view = self.view.next(),
            Input::Prev => self.view = self.view.prev(),
            Input::Select(view) => self.view = view,
            Input::Redraw => {}
        }
        false
    }
}

/// Runs the monitor until the user quits. `next_input(wait)` waits at most
/// `wait` for input and returns `None` on timeout; every wait is bounded by
/// the time left until the next refresh.
pub fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    project: &Project,
    interval: Duration,
    mut next_input: impl FnMut(Duration) -> std::io::Result<Option<Input>>,
) -> Result<()> {
    let mut model = Model::default();
    let mut ui = Ui::default();
    let mut store: Option<Store> = None;
    let mut due = Instant::now();
    loop {
        if Instant::now() >= due {
            model.refresh(project, &mut store, crate::state::now());
            due = Instant::now() + interval;
        }
        terminal
            .draw(|frame| draw(frame, &model, &ui))
            .map_err(|e| anyhow!("drawing the screen: {e}"))?;
        let wait = due.saturating_duration_since(Instant::now());
        if let Some(input) = next_input(wait)?
            && ui.apply(input)
        {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------- drawing

/// `text` made terminal-safe and cut to `width` characters.
fn clip(text: &str, width: usize) -> String {
    let safe = observe::untrusted(text);
    if safe.chars().count() <= width {
        return safe.into_owned();
    }
    let mut out: String = safe.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn age(ms: i64) -> String {
    let s = ms.max(0) / 1_000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3_599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3_600, s / 60 % 60),
    }
}

fn task_key(overview: &Overview, plan: PlanId, task: TaskId) -> Option<&str> {
    overview
        .plans
        .iter()
        .find(|p| p.plan.id == plan)?
        .tasks
        .iter()
        .find(|t| t.id == task)
        .map(|t| t.key.as_str())
}

/// `plan 3 task 7 (key)`, or `plan 3 (plan-level)`.
fn scope(overview: &Overview, plan: PlanId, task: Option<TaskId>) -> String {
    match task {
        None => format!("plan {plan} (plan-level)"),
        Some(task) => match task_key(overview, plan, task) {
            Some(key) => format!("plan {plan} task {task} ({})", clip(key, 24)),
            None => format!("plan {plan} task {task}"),
        },
    }
}

fn label(key: &GroupKey, overview: &Overview) -> String {
    match key {
        GroupKey::Provider(p) => clip(p, 40),
        GroupKey::Plan(plan) => format!("plan {plan}"),
        GroupKey::Task { plan, task } => scope(overview, *plan, *task),
        GroupKey::Role(role) => role.to_string(),
        GroupKey::Agent {
            agent,
            role,
            plan,
            task,
        } => match task {
            Some(task) => format!("agent {agent} {role} plan {plan} task {task}"),
            None => format!("agent {agent} {role} plan {plan}"),
        },
    }
}

/// `in/out`, estimates marked `~`, and `-` when nothing of that provenance
/// was counted (never `0`).
fn pair(counts: &Counts, estimate: bool) -> String {
    if counts.invocations == 0 {
        return "-".into();
    }
    let (i, o) = (
        observe::compact(counts.input),
        observe::compact(counts.output),
    );
    if estimate {
        format!("~{i}/~{o}")
    } else {
        format!("{i}/{o}")
    }
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// Draws the whole screen.
pub fn draw(frame: &mut Frame, model: &Model, ui: &Ui) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_header(frame, header, model);
    match &model.overview {
        Some(overview) => draw_body(frame, body, model, overview, ui),
        None => draw_waiting(frame, body, model),
    }
    draw_footer(frame, footer, ui);
}

fn draw_header(frame: &mut Frame, area: Rect, model: &Model) {
    let mut lines = vec![Line::from(vec![
        Span::styled("agenttop  ", bold()),
        Span::raw(clip(&model.root, 80)),
        Span::styled(format!("  {}", observe::timestamp(model.now)), dim()),
    ])];
    match &model.overview {
        Some(o) => {
            let mut by_state: BTreeMap<String, usize> = BTreeMap::new();
            for p in &o.plans {
                *by_state.entry(p.plan.state.to_string()).or_default() += 1;
            }
            let plans = if by_state.is_empty() {
                "no plans".to_owned()
            } else {
                by_state
                    .iter()
                    .map(|(state, n)| format!("{n} {state}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let pending = observe::total(&o.usage).pending;
            let capacity = match &o.capacity {
                Some(c) => format!("claims held {} of {}", c.held, c.limit),
                None => "no claims capacity yet".into(),
            };
            lines.push(Line::raw(format!("plans: {plans}")));
            lines.push(Line::raw(format!(
                "{pending} invocations with no end recorded | {} unresolved records | {capacity}",
                o.unresolved.len()
            )));
        }
        None if model.no_state => lines.push(Line::raw("no state yet")),
        None => lines.push(Line::raw("waiting for the first successful read")),
    }
    if let Some(error) = &model.error {
        lines.push(Line::styled(
            format!("read failed, retrying: {}", clip(error, 200)),
            Style::new().fg(Color::Red),
        ));
    }
    frame.render_widget(Paragraph::new(lines).block(Block::bordered()), area);
}

fn draw_waiting(frame: &mut Frame, area: Rect, model: &Model) {
    let text = if model.no_state {
        vec![
            Line::styled("no state yet", bold()),
            Line::raw("nothing has been planned or run here; checking again on every refresh"),
        ]
    } else if let Some(error) = &model.error {
        vec![
            Line::styled("cannot read the project's state", bold()),
            Line::raw(clip(error, 300)),
            Line::raw("retrying on every refresh"),
        ]
    } else {
        vec![Line::raw("reading the project's state")]
    };
    frame.render_widget(Paragraph::new(text).block(Block::bordered()), area);
}

fn draw_body(frame: &mut Frame, area: Rect, model: &Model, overview: &Overview, ui: &Ui) {
    let [graph, table, activity] = Layout::vertical([
        Constraint::Length(10),
        Constraint::Min(4),
        Constraint::Length(12),
    ])
    .areas(area);
    draw_graph(frame, graph, model, overview);
    draw_table(frame, table, overview, ui);
    draw_activity(frame, activity, model, overview);
}

fn draw_graph(frame: &mut Frame, area: Rect, model: &Model, overview: &Overview) {
    let block = Block::bordered().title(" tokens per minute ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [caption, headline, reported_row, estimated_row] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Fill(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(Span::styled(
            "tokens per minute, attributed when usage is reported (at invocation end)",
            dim(),
        )),
        caption,
    );

    let totals = observe::total(&overview.usage);
    let ended = overview.usage.iter().any(|r| r.usage.is_some());
    if !ended {
        let pending = if totals.pending > 0 {
            format!("; {} with no end recorded (usage pending)", totals.pending)
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(format!("no usage reported yet{pending}")),
            headline,
        );
        return;
    }
    let windows = observe::token_rate(&overview.usage, model.now, WINDOWS);
    if let Some(newest) = windows.last() {
        frame.render_widget(
            Paragraph::new(format!(
                "last minute: {} reported, ~{} estimated, {} ended without usage; \
                 {} with no end recorded (usage pending)",
                observe::compact(newest.reported),
                observe::compact(newest.estimated),
                newest.unavailable,
                totals.pending
            )),
            headline,
        );
    }
    for (row, name, estimate, style) in [
        (
            reported_row,
            "reported",
            false,
            Style::new().fg(Color::Green),
        ),
        (
            estimated_row,
            "~ estimated",
            true,
            Style::new().fg(Color::Yellow),
        ),
    ] {
        let [side, chart] =
            Layout::horizontal([Constraint::Length(16), Constraint::Fill(1)]).areas(row);
        let series = |w: &observe::RateWindow| if estimate { w.estimated } else { w.reported };
        let width = usize::from(chart.width);
        let skip = windows.len().saturating_sub(width);
        let data: Vec<u64> = windows.iter().skip(skip).map(series).collect();
        let peak = windows.iter().map(series).max().unwrap_or(0);
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(name, style),
                Line::styled(format!("peak {}", observe::compact(peak)), dim()),
            ]),
            side,
        );
        let mut spark = Sparkline::default().data(data).style(style);
        if estimate {
            spark = spark.bar_set(symbols::bar::THREE_LEVELS);
        }
        // Newest window at the right edge: draw into the rightmost columns.
        let used = u16::try_from(windows.len().min(width)).unwrap_or(chart.width);
        let chart = Rect {
            x: chart.x + chart.width - used,
            width: used,
            ..chart
        };
        frame.render_widget(spark, chart);
    }
}

fn draw_table(frame: &mut Frame, area: Rect, overview: &Overview, ui: &Ui) {
    let mut title = vec![Span::raw(" usage by ")];
    title.push(Span::styled(ui.view.name(), bold()));
    title.push(Span::raw(" "));
    let block = Block::bordered().title(Line::from(title));
    if overview.usage.is_empty() {
        frame.render_widget(
            Paragraph::new("no usage reported yet: no invocations recorded").block(block),
            area,
        );
        return;
    }
    let row = |key: String, a: &Aggregate, style: Style| {
        Row::new(vec![
            Cell::from(key),
            Cell::from(a.invocations().to_string()),
            Cell::from(pair(&a.reported, false)),
            Cell::from(pair(&a.estimated, true)),
            Cell::from(a.unavailable.to_string()),
            Cell::from(a.pending.to_string()),
        ])
        .style(style)
    };
    let mut rows = Vec::new();
    match ui.view.dimension() {
        None => rows.push(row(
            "all invocations".into(),
            &observe::total(&overview.usage),
            Style::new(),
        )),
        Some(dimension) => {
            for (key, aggregate) in observe::group(&overview.usage, dimension) {
                rows.push(row(label(&key, overview), &aggregate, Style::new()));
            }
            rows.push(row(
                "total".into(),
                &observe::total(&overview.usage),
                bold(),
            ));
        }
    }
    let header = Row::new([
        "key",
        "inv",
        "reported in/out",
        "~estimated in/out",
        "unavailable",
        "no end (pending)",
    ])
    .style(bold());
    let table = Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(5),
            Constraint::Length(17),
            Constraint::Length(19),
            Constraint::Length(11),
            Constraint::Length(16),
        ],
    )
    .header(header)
    .block(block);
    frame.render_widget(table, area);
}

fn draw_activity(frame: &mut Frame, area: Rect, model: &Model, overview: &Overview) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

    let mut live: Vec<&UsageRecord> = overview
        .usage
        .iter()
        .filter(|r| r.usage.is_none())
        .collect();
    live.sort_by_key(|r| (r.started_at, r.invocation.to_string()));
    let block =
        Block::bordered().title(format!(" no end recorded, usage pending ({}) ", live.len()));
    let height = usize::from(block.inner(left).height);
    let lines: Vec<Line> = if live.is_empty() {
        vec![Line::styled("none", dim())]
    } else {
        live.iter()
            .take(height)
            .map(|r| {
                Line::raw(format!(
                    "{} {} {}/{} {} recorded {}",
                    r.role,
                    scope(overview, r.plan, r.task),
                    clip(&r.provider, 16),
                    clip(&r.model, 24),
                    age(model.now - r.started_at),
                    r.state
                ))
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).block(block), left);

    let block = Block::bordered().title(format!(" recent events ({}) ", model.events.len()));
    let height = usize::from(block.inner(right).height);
    let lines: Vec<Line> = if model.events.is_empty() {
        vec![Line::styled("none", dim())]
    } else {
        model
            .events
            .iter()
            .rev()
            .take(height)
            .map(|e| Line::raw(event_line(e)))
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).block(block), right);
}

/// `HH:MM:SS kind scope detail`, terminal-safe.
fn event_line(e: &Event) -> String {
    let time = observe::timestamp(e.at);
    let mut line = format!(
        "{} {}",
        time.get(11..19).unwrap_or(""),
        observe::untrusted(&e.kind)
    );
    if let Some(plan) = e.plan {
        line.push_str(&format!(" plan {plan}"));
    }
    if let Some(task) = e.task {
        line.push_str(&format!(" task {task}"));
    }
    if let Some(agent) = e.agent {
        line.push_str(&format!(" agent {agent}"));
    }
    line.push(' ');
    line.push_str(&observe::untrusted(&e.detail));
    line
}

fn draw_footer(frame: &mut Frame, area: Rect, ui: &Ui) {
    let mut spans = vec![Span::styled(
        "q/Esc/Ctrl-C quit  Tab/Shift-Tab switch view  ",
        dim(),
    )];
    for (i, view) in View::ALL.iter().enumerate() {
        let style = if *view == ui.view {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        };
        spans.push(Span::styled(format!(" {} {} ", i + 1, view.name()), style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ratatui::backend::TestBackend;

    use super::*;
    use crate::state::tests::{objective, ready_plan, succeeded};
    use crate::state::{AgentScope, InvocationEnd, Role, TokenUsage, Usage};

    fn tokens(input: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input,
            output,
            cached_input: None,
            cache_write: None,
            reasoning: None,
        }
    }

    fn project() -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::create(dir.path(), crate::config::tests::sample()).unwrap();
        (dir, project)
    }

    fn with_store() -> (tempfile::TempDir, Project, Store) {
        let (dir, project) = project();
        std::fs::create_dir_all(dir.path().join(".agentctl")).unwrap();
        let store = Store::open(&project.state_path()).unwrap();
        (dir, project, store)
    }

    /// One invocation of `agent`; ended with `usage`, or left with no end.
    fn invoke(
        store: &mut Store,
        agent: crate::state::AgentId,
        provider: &str,
        model: &str,
        usage: Option<Usage>,
    ) {
        let id = store
            .start_invocation(agent, provider, model, None)
            .unwrap();
        store.invocation_running(id).unwrap();
        if let Some(usage) = usage {
            store
                .finish_invocation(
                    id,
                    &InvocationEnd {
                        usage,
                        ..succeeded()
                    },
                )
                .unwrap();
        }
    }

    fn text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn render(model: &Model, ui: &Ui) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| draw(f, model, ui)).unwrap();
        text(&terminal)
    }

    fn refreshed(project: &Project, store: &mut Option<Store>) -> Model {
        let mut model = Model::default();
        model.refresh(project, store, crate::state::now());
        model
    }

    /// claude reported, codex estimated, one unavailable, one live.
    fn mixed(store: &mut Store) -> PlanId {
        let (plan, tasks) = ready_plan(store, &[("build", &[], &[])]);
        let planner = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        invoke(
            store,
            planner,
            "claude",
            "claude-opus",
            Some(Usage::ProviderReported(tokens(1_200, 300))),
        );
        let generation = store.start_generation(tasks[0]).unwrap();
        let executor = store
            .create_agent(Role::Executor, AgentScope::Generation(generation))
            .unwrap();
        invoke(
            store,
            executor,
            "codex",
            "gpt",
            Some(Usage::LocalEstimate(tokens(500, 100))),
        );
        let verifier = store
            .create_agent(Role::Verifier, AgentScope::Generation(generation))
            .unwrap();
        invoke(
            store,
            verifier,
            "claude",
            "claude-opus",
            Some(Usage::Unavailable),
        );
        let live = store
            .create_agent(Role::Verifier, AgentScope::Plan(plan))
            .unwrap();
        invoke(store, live, "codex", "gpt", None);
        plan
    }

    fn is_unsafe(c: char) -> bool {
        c.is_control()
            || matches!(c,
                '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}'
                | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }

    #[test]
    fn no_state_yet_keeps_retrying_and_creates_nothing() {
        let (dir, project) = project();
        let mut store = None;
        let mut model = refreshed(&project, &mut store);
        assert!(model.no_state && store.is_none());
        let screen = render(&model, &Ui::default());
        assert!(screen.contains("no state yet"), "{screen}");
        assert!(!dir.path().join(".agentctl").exists());

        // The state appears later: the next refresh picks it up.
        std::fs::create_dir_all(dir.path().join(".agentctl")).unwrap();
        drop(Store::open(&project.state_path()).unwrap());
        model.refresh(&project, &mut store, crate::state::now());
        assert!(!model.no_state && store.is_some() && model.error.is_none());
        assert!(render(&model, &Ui::default()).contains("no plans"));
    }

    #[test]
    fn an_empty_store_claims_no_tokens() {
        let (_dir, project, _writer) = with_store();
        let model = refreshed(&project, &mut None);
        for view in View::ALL {
            let screen = render(&model, &Ui { view });
            assert!(screen.contains("no usage reported yet"), "{screen}");
            assert!(!screen.contains("0 tokens"), "{screen}");
            assert!(screen.contains("no plans"), "{screen}");
        }
    }

    #[test]
    fn mixed_provenance_stays_distinct_and_unknown_stays_unknown() {
        let (_dir, project, mut writer) = with_store();
        let plan = mixed(&mut writer);
        let model = refreshed(&project, &mut None);
        assert!(model.error.is_none());

        let aggregate = render(&model, &Ui::default());
        assert!(
            aggregate.contains(
                "tokens per minute, attributed when usage is reported (at invocation end)"
            )
        );
        assert!(
            aggregate.contains(
                "last minute: 1.5k reported, ~600 estimated, 1 ended without usage; \
                 1 with no end recorded (usage pending)"
            ),
            "{aggregate}"
        );
        // 1 live, 1 unavailable, 1 reported, 1 estimated.
        assert!(aggregate.contains("1.2k/300"), "{aggregate}");
        assert!(aggregate.contains("~500/~100"), "{aggregate}");
        assert!(aggregate.contains("unavailable"), "{aggregate}");
        assert!(
            aggregate.contains("1 invocations with no end recorded"),
            "{aggregate}"
        );
        assert!(
            aggregate.contains("no end recorded, usage pending (1)"),
            "{aggregate}"
        );
        assert!(aggregate.contains("codex/gpt"), "{aggregate}");
        assert!(!aggregate.contains("succeeded"), "{aggregate}");
        assert!(aggregate.contains("recent events"), "{aggregate}");

        let by = |view| render(&model, &Ui { view });
        let provider = by(View::Provider);
        assert!(
            provider.contains("claude") && provider.contains("codex"),
            "{provider}"
        );
        assert!(provider.contains("total"), "{provider}");
        let plans = by(View::Plan);
        assert!(plans.contains(&format!("plan {plan} ")), "{plans}");
        let task = by(View::Task);
        assert!(
            task.contains("(plan-level)") && task.contains("(build)"),
            "{task}"
        );
        let role = by(View::Role);
        assert!(
            role.contains("planner") && role.contains("executor") && role.contains("verifier"),
            "{role}"
        );
        let agent = by(View::Agent);
        assert!(
            agent.contains("executor plan") && agent.contains("task"),
            "{agent}"
        );
    }

    #[test]
    fn hostile_strings_reach_no_cell_raw() {
        const EVIL: &str = "\x1b[31mred\x1b]52;c;SGVsbG8=\x07\u{9b}2J\rover\u{202e}write";
        let (_dir, project, mut writer) = with_store();
        let plan = writer.create_plan(&objective("hostile")).unwrap();
        let planner = writer
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        invoke(
            &mut writer,
            planner,
            EVIL,
            EVIL,
            Some(Usage::ProviderReported(tokens(1, 2))),
        );
        invoke(&mut writer, planner, EVIL, EVIL, None);
        let model = refreshed(&project, &mut None);
        for view in View::ALL {
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal.draw(|f| draw(f, &model, &Ui { view })).unwrap();
            let buffer = terminal.backend().buffer();
            for cell in &buffer.content {
                assert!(!cell.symbol().chars().any(is_unsafe), "{:?}", cell.symbol());
            }
        }
        let screen = render(&model, &Ui::default());
        assert!(screen.contains("\\u{1b}[31mred"), "{screen}");
    }

    #[test]
    fn the_events_ring_is_bounded_and_incremental() {
        let (_dir, project, mut writer) = with_store();
        let plan = writer.create_plan(&objective("p")).unwrap();
        let agent = writer
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        let mut store = None;
        let mut model = Model::with_event_cap(10);
        for round in 0..4 {
            for _ in 0..5 {
                invoke(&mut writer, agent, "claude", "m", Some(Usage::Unavailable));
            }
            model.refresh(&project, &mut store, crate::state::now());
            assert!(model.events.len() <= 10, "round {round}");
            let seqs: Vec<i64> = model.events.iter().map(|e| e.seq).collect();
            assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
            assert_eq!(model.last_seq, seqs.last().copied());
        }
        assert_eq!(model.events.len(), 10);
        let last = model.last_seq;
        model.refresh(&project, &mut store, crate::state::now());
        assert_eq!(model.last_seq, last);
        assert_eq!(model.events.len(), 10);
    }

    #[test]
    fn refreshing_never_double_counts_and_never_creates_a_session() {
        fn listing(root: &Path) -> Vec<String> {
            fn walk(dir: &Path, out: &mut Vec<String>) {
                for entry in std::fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    let name = path.display().to_string();
                    if name.ends_with("-wal") || name.ends_with("-shm") {
                        continue;
                    }
                    if path.is_dir() {
                        walk(&path, out);
                    }
                    out.push(name);
                }
            }
            let mut out = Vec::new();
            walk(&root.join(".agentctl"), &mut out);
            out.sort();
            out
        }
        let (dir, project, mut writer) = with_store();
        mixed(&mut writer);
        let before = listing(dir.path());
        let mut store = None;
        let mut model = Model::default();
        let mut first = None;
        for _ in 0..5 {
            model.refresh(&project, &mut store, crate::state::now());
            let total = observe::total(&model.overview.as_ref().unwrap().usage);
            assert_eq!(*first.get_or_insert(total), total);
        }
        let total = first.unwrap();
        assert_eq!(
            (total.reported.total(), total.estimated.total()),
            (1_500, 600)
        );
        assert_eq!((total.unavailable, total.pending), (1, 1));
        assert_eq!(listing(dir.path()), before);
    }

    #[test]
    fn a_failing_read_is_shown_and_retried() {
        let (dir, project) = project();
        std::fs::create_dir_all(dir.path().join(".agentctl")).unwrap();
        std::fs::write(project.state_path(), "not a database").unwrap();
        let mut store = None;
        let mut model = refreshed(&project, &mut store);
        assert!(model.error.is_some() && store.is_none() && model.overview.is_none());
        let screen = render(&model, &Ui::default());
        assert!(
            screen.contains("cannot read the project's state"),
            "{screen}"
        );
        assert!(screen.contains("read failed, retrying"), "{screen}");

        std::fs::remove_file(project.state_path()).unwrap();
        drop(Store::open(&project.state_path()).unwrap());
        model.refresh(&project, &mut store, crate::state::now());
        assert!(model.error.is_none() && model.overview.is_some());
    }

    #[test]
    fn a_failing_read_after_a_good_one_hides_stale_data() {
        let (_dir, project, mut writer) = with_store();
        let plan = writer.create_plan(&objective("intent")).unwrap();
        let agent = writer
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        invoke(
            &mut writer,
            agent,
            "claude",
            "m",
            Some(Usage::ProviderReported(tokens(10, 5))),
        );
        drop(writer);
        let mut store = None;
        let mut model = refreshed(&project, &mut store);
        assert!(model.overview.is_some() && model.error.is_none());
        store = None;
        remove_store_files(&project);
        std::fs::write(project.state_path(), "not a database").unwrap();
        model.refresh(&project, &mut store, crate::state::now());
        assert!(model.error.is_some() && model.overview.is_none());
        assert!(model.events.is_empty() && model.last_seq.is_none());
        let screen = render(&model, &Ui::default());
        assert!(screen.contains("read failed, retrying"), "{screen}");
    }

    fn remove_store_files(project: &Project) {
        let db = project.state_path();
        for suffix in ["", "-wal", "-shm"] {
            let mut name = db.clone().into_os_string();
            name.push(suffix);
            match std::fs::remove_file(std::path::PathBuf::from(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => panic!("remove store file: {e}"),
            }
        }
    }

    #[test]
    fn a_removed_store_is_noticed() {
        let (_dir, project, mut writer) = with_store();
        let plan = writer.create_plan(&objective("intent")).unwrap();
        let agent = writer
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        invoke(
            &mut writer,
            agent,
            "claude",
            "m",
            Some(Usage::ProviderReported(tokens(10, 5))),
        );
        let mut store = None;
        let mut model = Model::default();
        model.refresh(&project, &mut store, crate::state::now());
        assert!(model.overview.is_some());
        drop(writer);
        store = None;
        remove_store_files(&project);
        model.refresh(&project, &mut store, crate::state::now());
        assert!(model.no_state);
        assert!(model.overview.is_none());
        assert!(model.events.is_empty());
        assert!(model.last_seq.is_none());
        assert!(render(&model, &Ui::default()).contains("no state yet"));
    }

    #[test]
    fn a_replaced_store_is_read_afresh() {
        let (_dir, project, mut writer) = with_store();
        let plan = writer.create_plan(&objective("intent")).unwrap();
        for _ in 0..3 {
            let agent = writer
                .create_agent(Role::Planner, AgentScope::Plan(plan))
                .unwrap();
            invoke(
                &mut writer,
                agent,
                "claude",
                "a-model",
                Some(Usage::ProviderReported(tokens(10, 5))),
            );
        }
        let mut store = None;
        let mut model = Model::default();
        model.refresh(&project, &mut store, crate::state::now());
        assert!(!model.events.is_empty());
        assert!(model.last_seq.is_some());
        drop(writer);
        store = None;
        remove_store_files(&project);
        let (other_dir, other, mut replacement) = with_store();
        let plan = replacement.create_plan(&objective("intent")).unwrap();
        let agent = replacement
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        invoke(
            &mut replacement,
            agent,
            "claude",
            "replacement-model",
            Some(Usage::ProviderReported(tokens(1, 1))),
        );
        let all = replacement
            .query_events(&EventQuery {
                after: None,
                plan: None,
                task: None,
                agent: None,
                kind: None,
                limit: u32::MAX,
                newest: false,
            })
            .unwrap();
        drop(replacement);
        for suffix in ["", "-wal", "-shm"] {
            let mut from = other.state_path().into_os_string();
            from.push(suffix);
            let mut to = project.state_path().into_os_string();
            to.push(suffix);
            if std::path::Path::new(&from).exists() {
                std::fs::copy(&from, &to).unwrap();
            }
        }
        drop(other_dir);
        model.refresh(&project, &mut store, crate::state::now());
        let overview = model.overview.as_ref().unwrap();
        assert_eq!(overview.usage.len(), 1);
        assert_eq!(overview.usage[0].model, "replacement-model");
        let skip = all.len().saturating_sub(model.event_cap);
        let expected: Vec<Event> = all[skip..].to_vec();
        assert_eq!(model.events.iter().cloned().collect::<Vec<_>>(), expected);
        let max = all.iter().map(|e| e.seq).max().unwrap();
        assert!(model.events.iter().all(|e| e.seq <= max));
        assert!(
            model
                .events
                .iter()
                .all(|e| !format!("{e:?}").contains("a-model"))
        );
    }

    #[test]
    fn views_and_keys() {
        assert_eq!(View::Agent.next(), View::Aggregate);
        assert_eq!(View::Aggregate.prev(), View::Agent);
        assert_eq!(View::from_digit('1'), Some(View::Aggregate));
        assert_eq!(View::from_digit('6'), Some(View::Agent));
        assert_eq!(View::from_digit('0'), None);
        assert_eq!(View::from_digit('7'), None);
        let mut ui = Ui::default();
        assert!(!ui.apply(Input::Next) && ui.view == View::Provider);
        assert!(!ui.apply(Input::Select(View::Role)) && ui.view == View::Role);
        assert!(!ui.apply(Input::Redraw) && ui.view == View::Role);
        assert!(ui.apply(Input::Quit));
    }

    #[test]
    fn a_live_update_appears_and_counts_once() {
        let (_dir, project, mut writer) = with_store();
        let plan = writer.create_plan(&objective("p")).unwrap();
        let agent = writer
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let mut calls = 0;
        run(&mut terminal, &project, Duration::from_millis(50), |wait| {
            calls += 1;
            match calls {
                1 => {
                    invoke(
                        &mut writer,
                        agent,
                        "claude",
                        "m",
                        Some(Usage::ProviderReported(tokens(1_000, 234))),
                    );
                    std::thread::sleep(wait);
                    Ok(None)
                }
                2 => Ok(Some(Input::Redraw)),
                _ => Ok(Some(Input::Quit)),
            }
        })
        .unwrap();
        let screen = text(&terminal);
        assert!(screen.contains("1.0k/234"), "{screen}");
        assert!(screen.contains("last minute: 1.2k reported"), "{screen}");
        assert!(screen.contains("invocation.ended"), "{screen}");
    }
}
