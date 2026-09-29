//! Plain-text renderers for `agentctl status` and `agentctl logs`.
//!
//! Pure functions over the observation model, so what a terminal shows is
//! testable. Workflow status comes from the [`Overview`] (canonical state);
//! events are chronology alone. Every string that can originate from a human,
//! a planner, a provider, configuration or the repository goes through
//! [`observe::untrusted`] before it is shown.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use crate::observe::{self, Overview, PlanOverview};
use crate::state::{
    Condition, Event, IntegrationStatus, PlanId, PlanState, TaskStatus, UsageRecord,
};

/// The longest objective shown, in characters.
const OBJECTIVE_WIDTH: usize = 72;
/// How many session prefixes an unresolved line names.
const SESSIONS_SHOWN: usize = 3;

/// `text` made terminal-safe and cut to at most `width` characters, with an
/// ellipsis when it was cut. Cuts on a character boundary, after escaping, so
/// an escape is never split.
fn clip(text: &str, width: usize) -> String {
    let safe = observe::untrusted(text);
    if safe.chars().count() <= width {
        return safe.into_owned();
    }
    let mut out: String = safe.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The unresolved records' sessions, as short safe prefixes.
fn sessions<'a>(sessions: impl Iterator<Item = &'a str>) -> String {
    let distinct: BTreeSet<String> = sessions.map(|s| clip(s, 8)).collect();
    let shown: Vec<&str> = distinct
        .iter()
        .take(SESSIONS_SHOWN)
        .map(String::as_str)
        .collect();
    let more = distinct.len().saturating_sub(SESSIONS_SHOWN);
    let mut text = shown.join(", ");
    if more > 0 {
        let _ = write!(text, " and {more} more");
    }
    text
}

fn unresolved_line(count: usize, sessions: &str) -> String {
    format!(
        "{count} unresolved records (sessions {sessions}): in flight, or left by an agentctl \
         process that ended — `agentctl recover` settles what ended"
    )
}

fn usage_of(records: &[UsageRecord], plan: PlanId) -> String {
    let own: Vec<UsageRecord> = records.iter().filter(|r| r.plan == plan).cloned().collect();
    observe::summary(&observe::total(&own))
}

/// The project's status, from canonical state. `None` means the project has
/// no state store yet.
pub fn status(root: &Path, overview: Option<&Overview>) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "project {}",
        observe::untrusted(&root.display().to_string())
    );
    let Some(overview) = overview else {
        out.push_str("no state yet: nothing has been planned or run here\n");
        return out;
    };
    if let Some(capacity) = &overview.capacity {
        let _ = writeln!(out, "claims held {} of {}", capacity.held, capacity.limit);
    }
    if overview.plans.is_empty() {
        out.push_str("no plans\n");
    }
    for plan in &overview.plans {
        plan_section(&mut out, plan, overview);
    }
    let _ = writeln!(
        out,
        "usage: {}",
        observe::summary(&observe::total(&overview.usage))
    );
    let pending = observe::total(&overview.usage).pending;
    if pending > 0 {
        let _ = writeln!(
            out,
            "{pending} invocations with no end recorded: their usage is not yet known"
        );
    }
    out
}

fn plan_section(out: &mut String, p: &PlanOverview, overview: &Overview) {
    let id = p.plan.id;
    let _ = write!(out, "\nplan {id} {}", p.plan.state);
    if !matches!(p.condition, Condition::NotRunning(_)) {
        let _ = write!(out, " ({:?})", p.condition);
    }
    out.push('\n');
    let _ = writeln!(
        out,
        "  objective: {}",
        clip(&p.plan.intent.objective, OBJECTIVE_WIDTH)
    );

    let mut counts = [0usize; 7];
    for t in &p.tasks {
        let slot = match t.status {
            TaskStatus::Completed => 0,
            TaskStatus::Scheduled(_) => 1,
            TaskStatus::Eligible => 2,
            TaskStatus::WaitingForDependencies(_) | TaskStatus::WaitingForOwnership(_) => 3,
            TaskStatus::Stopped { .. } => 4,
            TaskStatus::Cancelled => 5,
            TaskStatus::Unscheduled { .. } => 6,
        };
        counts[slot] += 1;
    }
    let names = [
        "completed",
        "scheduled",
        "eligible",
        "waiting",
        "stopped",
        "cancelled",
        "unscheduled",
    ];
    let tasks: Vec<String> = names
        .iter()
        .zip(counts)
        .filter(|(_, n)| *n > 0)
        .map(|(name, n)| format!("{n} {name}"))
        .collect();
    if !tasks.is_empty() {
        let _ = writeln!(out, "  tasks: {}", tasks.join(", "));
    }
    for t in &p.tasks {
        let key = clip(&t.key, OBJECTIVE_WIDTH);
        match &t.status {
            TaskStatus::Scheduled(generation) => {
                let _ = writeln!(out, "  scheduled: {key} (generation {generation})");
            }
            TaskStatus::Stopped { generation, .. } => {
                let _ = writeln!(out, "  stopped: {key} (generation {generation})");
            }
            _ => {}
        }
    }

    if p.plan.state == PlanState::NeedsAttention {
        if p.stopped_by_human {
            out.push_str("  attention: stopped by its human; it never continues autonomously\n");
        } else if p.concerns_awaiting > 0 {
            let _ = writeln!(
                out,
                "  attention: {} concerns await `agentctl plan decide {id} <concern> ...`",
                p.concerns_awaiting
            );
        } else {
            let _ = writeln!(
                out,
                "  attention: an instruction awaits its planner: `agentctl plan update {id}`"
            );
        }
    } else if p.stopped_by_human {
        out.push_str("  stopped by its human\n");
    }
    if p.completion_proposed {
        out.push_str("  completion proposed by its planner\n");
    }
    if let Some(integration) = &p.integration {
        let verdict = match &integration.status {
            IntegrationStatus::Intended => "intended, not yet run".to_owned(),
            IntegrationStatus::OutcomeUnknown { .. } => "outcome unknown".to_owned(),
            IntegrationStatus::Finished(result) => result.outcome.to_string(),
        };
        let _ = writeln!(
            out,
            "  integration verification {}: {verdict}",
            integration.id
        );
    }
    if p.unresolved > 0 {
        let s = sessions(
            overview
                .unresolved
                .iter()
                .filter(|u| u.plan == id)
                .map(|u| u.session.as_str()),
        );
        let _ = writeln!(out, "  {}", unresolved_line(p.unresolved, &s));
    }
    let _ = writeln!(out, "  usage: {}", usage_of(&overview.usage, id));
}

/// One event as one terminal-safe line:
/// `{seq:>6} {timestamp} {kind} {scope} {detail}`.
pub fn event_line(event: &Event) -> String {
    let mut scope = Vec::new();
    if let Some(plan) = event.plan {
        scope.push(format!("plan {plan}"));
    }
    if let Some(task) = event.task {
        scope.push(format!("task {task}"));
    }
    if let Some(agent) = event.agent {
        scope.push(format!("agent {agent}"));
    }
    let mut line = format!(
        "{:>6} {} {}",
        event.seq,
        observe::timestamp(event.at),
        observe::untrusted(&event.kind)
    );
    if !scope.is_empty() {
        line.push(' ');
        line.push_str(&scope.join(" "));
    }
    let _ = write!(line, " {}", observe::untrusted(&event.detail));
    line
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::observe::overview;
    use crate::state::tests::{ended, objective, ready_plan, store, succeeded};
    use crate::state::{
        AgentScope, Claim, InvocationEnd, InvocationState, Role, TokenUsage, Usage,
    };

    const LIMIT: NonZeroU32 = NonZeroU32::new(4).unwrap();

    fn tokens(input: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input,
            output,
            cached_input: None,
            cache_write: None,
            reasoning: None,
        }
    }

    fn is_unsafe(c: char) -> bool {
        c.is_control()
            || matches!(c,
                '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}'
                | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }

    fn safe_lines(text: &str) {
        assert!(!text.chars().any(|c| c != '\n' && is_unsafe(c)), "{text:?}");
    }

    const EVIL: &str = "\x1b[31mred\x1b]52;c;SGVsbG8=\x07\u{9b}2J\r\nx\u{202e}y";

    #[test]
    fn no_state_and_no_plans() {
        let none = status(Path::new("/p"), None);
        assert!(none.contains("no state yet"));
        let (_dir, store) = store();
        let o = overview(&store, LIMIT).unwrap();
        let text = status(Path::new("/p"), Some(&o));
        assert!(text.contains("no plans"));
        assert!(text.contains("usage: no invocations"));
    }

    #[test]
    fn several_plans_are_reported_from_canonical_state() {
        let (_dir, mut store) = store();
        let planning = store.create_plan(&objective("still planning")).unwrap();
        let (running, tasks) =
            ready_plan(&mut store, &[("a", &["a.txt"], &[]), ("b", &[], &["a"])]);
        store.start_plan(running).unwrap();
        let Claim::Claimed(_) = store.claim(tasks[0], LIMIT).unwrap() else {
            panic!("not claimed");
        };
        let (attention, _) = ready_plan(&mut store, &[("k", &[], &[])]);
        store.start_plan(attention).unwrap();
        let basis = store.replan_basis(attention).unwrap();
        let _ = store
            .replan(
                attention,
                &basis,
                None,
                &[crate::planner::Command::RaiseAttention {
                    concern: "k".into(),
                    reason: "r".into(),
                    evidence: vec!["e".into()],
                    tasks: vec!["k".into()],
                }],
                &|_| Ok(()),
                &|_| Ok(Vec::new()),
            )
            .unwrap();

        let o = overview(&store, LIMIT).unwrap();
        let text = status(Path::new("/p"), Some(&o));
        assert!(text.contains("claims held 1 of 4"), "{text}");
        assert!(
            text.contains(&format!("plan {planning} planning")),
            "{text}"
        );
        assert!(text.contains(&format!("plan {running} running")), "{text}");
        assert!(text.contains("1 scheduled, 1 waiting"), "{text}");
        assert!(text.contains("scheduled: a (generation"), "{text}");
        assert!(
            text.contains(&format!("plan {attention} needs_attention")),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "1 concerns await `agentctl plan decide {attention} <concern> ...`"
            )),
            "{text}"
        );
        assert!(text.contains("objective: still planning"), "{text}");
    }

    #[test]
    fn a_live_invocation_is_unresolved_never_succeeded() {
        let (_dir, mut store) = store();
        let plan = store.create_plan(&objective("p")).unwrap();
        let agent = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        let done = store.start_invocation(agent, "claude", "m", None).unwrap();
        store.invocation_running(done).unwrap();
        store
            .finish_invocation(
                done,
                &InvocationEnd {
                    usage: Usage::ProviderReported(tokens(10, 5)),
                    ..succeeded()
                },
            )
            .unwrap();
        let live = store.start_invocation(agent, "claude", "m", None).unwrap();
        store.invocation_running(live).unwrap();

        let o = overview(&store, LIMIT).unwrap();
        let text = status(Path::new("/p"), Some(&o));
        assert!(text.contains("unresolved records"), "{text}");
        assert!(text.contains("`agentctl recover`"), "{text}");
        assert!(
            text.contains("1 invocations with no end recorded"),
            "{text}"
        );
        assert!(text.contains("10 in / 5 out reported; 1 pending"), "{text}");
        assert!(!text.contains("succeeded"), "{text}");
    }

    #[test]
    fn mixed_provenance_keeps_its_marks() {
        let (_dir, mut store) = store();
        let plan = store.create_plan(&objective("p")).unwrap();
        let agent = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        for usage in [
            Usage::ProviderReported(tokens(1_234, 567)),
            Usage::LocalEstimate(tokens(89, 12)),
            Usage::Unavailable,
        ] {
            let id = store.start_invocation(agent, "claude", "m", None).unwrap();
            store.invocation_running(id).unwrap();
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
        let cancelled = store.start_invocation(agent, "claude", "m", None).unwrap();
        store.invocation_running(cancelled).unwrap();
        store
            .finish_invocation(cancelled, &ended(InvocationState::Cancelled))
            .unwrap();
        let o = overview(&store, LIMIT).unwrap();
        let text = status(Path::new("/p"), Some(&o));
        assert!(
            text.contains("1,234 in / 567 out reported; ~89 in / ~12 out estimated"),
            "{text}"
        );
    }

    #[test]
    fn untrusted_text_never_reaches_the_terminal_raw() {
        let (_dir, mut store) = store();
        let plan = store
            .create_plan(&objective("a\u{202e}b\u{2066}c"))
            .unwrap();
        let agent = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        let id = store.start_invocation(agent, "claude", EVIL, None).unwrap();
        store.invocation_running(id).unwrap();
        let o = overview(&store, LIMIT).unwrap();
        safe_lines(&status(Path::new(EVIL), Some(&o)));

        let event = Event {
            seq: 7,
            at: 0,
            kind: EVIL.into(),
            plan: Some(plan),
            task: None,
            agent: Some(agent),
            detail: EVIL.into(),
        };
        let line = event_line(&event);
        assert!(!line.chars().any(is_unsafe), "{line:?}");
        assert_eq!(line.lines().count(), 1);
    }

    #[test]
    fn objectives_are_clipped_on_character_boundaries() {
        let long = "é".repeat(200);
        let clipped = clip(&long, OBJECTIVE_WIDTH);
        assert_eq!(clipped.chars().count(), OBJECTIVE_WIDTH);
        assert!(clipped.ends_with('…'));
        assert_eq!(clip("short", OBJECTIVE_WIDTH), "short");
    }

    #[test]
    fn event_lines_omit_absent_scope() {
        let event = |plan, task, agent| Event {
            seq: 42,
            at: 0,
            kind: "plan.created".into(),
            plan,
            task,
            agent,
            detail: "d".into(),
        };
        assert_eq!(
            event_line(&event(None, None, None)),
            "    42 1970-01-01T00:00:00.000Z plan.created d"
        );
        let p = "1".parse().unwrap();
        let t = "2".parse().unwrap();
        let a = "3".parse().unwrap();
        assert_eq!(
            event_line(&event(Some(p), Some(t), Some(a))),
            "    42 1970-01-01T00:00:00.000Z plan.created plan 1 task 2 agent 3 d"
        );
        assert_eq!(
            event_line(&event(Some(p), None, Some(a))),
            "    42 1970-01-01T00:00:00.000Z plan.created plan 1 agent 3 d"
        );
    }
}
