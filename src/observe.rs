//! The one provider-neutral observation model: what `agentctl status`,
//! `agentctl logs`, `agenttop` and every later client show.
//!
//! Everything here is pure logic over canonical records, plus one
//! projection ([`overview`]) that reads a [`Store`] and writes nothing.
//! Workflow status comes from canonical state, never from events, which
//! are chronology alone.
//!
//! Unknown stays unknown. Usage keeps its provenance through every
//! aggregate: provider-reported and locally estimated counts are never
//! merged, an invocation that ended without usage is *unavailable*, one
//! with no end recorded is *pending*, and neither is ever zero tokens.
//!
//! Observation never probes liveness. `Store::liveness`,
//! `recovery_required` and `barrier` take session locks, and a poller
//! holding a dead session's lock would make recovery and scheduler barriers
//! misjudge that session as running. Unresolved work is therefore reported
//! as recorded by a session, never classified as running or interrupted.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::num::NonZeroU32;

use anyhow::Result;

use crate::state::{
    AgentId, Capacity, Condition, Integration, Plan, PlanId, PlanState, Role, Store, TaskId,
    TaskStatus, TokenUsage, Unresolved, Usage, UsageRecord,
};

/// Token counts of the invocations that reported one provenance.
///
/// `input` counts every input token, cached ones included; `output` counts
/// every output token, reasoning included. `cached_input`, `cache_write`
/// and `reasoning` are subsets of those, so they are never added to a
/// total. A subset is known only if every contributing invocation reported
/// it: one that did not makes it `None`, as does an overflow, and with no
/// contributor at all it is `None` too (nothing reported it). `input` and
/// `output` saturate instead of overflowing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// The invocations counted.
    pub invocations: u64,
    pub input: u64,
    pub output: u64,
    pub cached_input: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
}

impl Counts {
    /// Input plus output. The subsets are already inside those.
    pub fn total(&self) -> u64 {
        self.input.saturating_add(self.output)
    }

    fn add(&mut self, usage: &TokenUsage) {
        let first = self.invocations == 0;
        let subset = |have: Option<u64>, new: Option<u64>| {
            if first {
                new
            } else {
                have.zip(new).and_then(|(a, b)| a.checked_add(b))
            }
        };
        self.cached_input = subset(self.cached_input, usage.cached_input);
        self.cache_write = subset(self.cache_write, usage.cache_write);
        self.reasoning = subset(self.reasoning, usage.reasoning);
        self.input = self.input.saturating_add(usage.input);
        self.output = self.output.saturating_add(usage.output);
        self.invocations = self.invocations.saturating_add(1);
    }
}

/// The usage of a set of invocations, by provenance.
///
/// `reported` and `estimated` are separate and stay so: there is no
/// accessor that sums them. `unavailable` invocations ended without usage;
/// `pending` ones have no end recorded, so their usage is not yet known.
/// Neither contributes tokens, so the known counts are lower bounds unless
/// [`Aggregate::complete`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Aggregate {
    /// Counts the provider reported.
    pub reported: Counts,
    /// Counts agentctl estimated locally.
    pub estimated: Counts,
    /// Invocations that ended without usage.
    pub unavailable: u64,
    /// Invocations with no end recorded.
    pub pending: u64,
}

impl Aggregate {
    /// Counts `record` once. A record is one invocation's single final
    /// observation, so aggregating a record set holds no other state.
    pub fn add(&mut self, record: &UsageRecord) {
        match &record.usage {
            None => self.pending = self.pending.saturating_add(1),
            Some(Usage::Unavailable) => self.unavailable = self.unavailable.saturating_add(1),
            Some(Usage::ProviderReported(t)) => self.reported.add(t),
            Some(Usage::LocalEstimate(t)) => self.estimated.add(t),
        }
    }

    /// Every invocation counted, whatever became of its usage.
    pub fn invocations(&self) -> u64 {
        self.reported
            .invocations
            .saturating_add(self.estimated.invocations)
            .saturating_add(self.unavailable)
            .saturating_add(self.pending)
    }

    /// Whether every invocation contributed known counts: none ended
    /// without usage and none is still pending.
    pub fn complete(&self) -> bool {
        self.unavailable == 0 && self.pending == 0
    }
}

/// The usage of every record, by provenance.
pub fn total(records: &[UsageRecord]) -> Aggregate {
    let mut aggregate = Aggregate::default();
    for record in records {
        aggregate.add(record);
    }
    aggregate
}

/// What to group usage by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Provider,
    Plan,
    Task,
    Role,
    Agent,
}

/// One group of a [`Dimension`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupKey {
    Provider(String),
    Plan(PlanId),
    /// `task` is `None` for plan-level work (planners, plan-scoped
    /// verifiers): shown as its own group, never assigned to a task.
    Task {
        plan: PlanId,
        task: Option<TaskId>,
    },
    Role(Role),
    Agent {
        agent: AgentId,
        role: Role,
        plan: PlanId,
        task: Option<TaskId>,
    },
}

fn rank(role: Role) -> u8 {
    match role {
        Role::Planner => 0,
        Role::Executor => 1,
        Role::Verifier => 2,
    }
}

type SortKey<'a> = (
    u8,
    Option<PlanId>,
    Option<TaskId>,
    Option<AgentId>,
    u8,
    &'a str,
);

impl GroupKey {
    /// Covers every field, so the order agrees with equality: by kind, then
    /// plan, then task (plan-level first), agent and role.
    fn sort_key(&self) -> SortKey<'_> {
        match self {
            Self::Provider(p) => (0, None, None, None, 0, p),
            Self::Plan(plan) => (1, Some(*plan), None, None, 0, ""),
            Self::Task { plan, task } => (2, Some(*plan), *task, None, 0, ""),
            Self::Role(role) => (3, None, None, None, rank(*role), ""),
            Self::Agent {
                agent,
                role,
                plan,
                task,
            } => (4, Some(*plan), *task, Some(*agent), rank(*role), ""),
        }
    }
}

impl PartialOrd for GroupKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GroupKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

/// The usage of `records` grouped by `dimension`, in `GroupKey` order.
pub fn group(records: &[UsageRecord], dimension: Dimension) -> Vec<(GroupKey, Aggregate)> {
    let mut groups: BTreeMap<GroupKey, Aggregate> = BTreeMap::new();
    for r in records {
        let key = match dimension {
            Dimension::Provider => GroupKey::Provider(r.provider.clone()),
            Dimension::Plan => GroupKey::Plan(r.plan),
            Dimension::Task => GroupKey::Task {
                plan: r.plan,
                task: r.task,
            },
            Dimension::Role => GroupKey::Role(r.role),
            Dimension::Agent => GroupKey::Agent {
                agent: r.agent,
                role: r.role,
                plan: r.plan,
                task: r.task,
            },
        };
        groups.entry(key).or_default().add(r);
    }
    groups.into_iter().collect()
}

/// The width of one rate window, in milliseconds.
pub const RATE_WINDOW_MS: i64 = 60_000;

/// Tokens recorded within one rate window: `start` exclusive, `end`
/// inclusive, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateWindow {
    pub start: i64,
    pub end: i64,
    /// `Counts::total` of provider-reported usage recorded in the window.
    pub reported: u64,
    /// `Counts::total` of locally estimated usage recorded in the window.
    pub estimated: u64,
    /// Invocations that ended in the window without usage: not tokens.
    pub unavailable: u64,
}

/// Tokens per minute, as windows of [`RATE_WINDOW_MS`] ending at `now`.
///
/// Usage is reported only as an invocation ends, so its tokens belong to
/// the instant it ended (`ended_at`), whatever its duration. Window `i`
/// covers `ended_at` in `(now - (i + 1) * 60_000, now - i * 60_000]`, and
/// the result is oldest first, so the last element is the most recent
/// minute and a graph reads left to right (reported and estimated kept
/// apart). A record ended after
/// `now` is in no window. Pending invocations contribute nothing: their
/// usage is unknown, not zero, and they are counted by [`Aggregate`].
pub fn token_rate(records: &[UsageRecord], now: i64, windows: usize) -> Vec<RateWindow> {
    // Newest first while filling; index is the window number.
    let mut out: Vec<RateWindow> = (0..windows)
        .map(|i| {
            let i = i64::try_from(i).unwrap_or(i64::MAX);
            RateWindow {
                start: now.saturating_sub(i.saturating_add(1).saturating_mul(RATE_WINDOW_MS)),
                end: now.saturating_sub(i.saturating_mul(RATE_WINDOW_MS)),
                reported: 0,
                estimated: 0,
                unavailable: 0,
            }
        })
        .collect();
    for r in records {
        let (Some(ended), Some(usage)) = (r.ended_at, &r.usage) else {
            continue;
        };
        if ended > now {
            continue;
        }
        let Ok(index) = usize::try_from(now.saturating_sub(ended) / RATE_WINDOW_MS) else {
            continue;
        };
        let Some(window) = out.get_mut(index) else {
            continue;
        };
        match usage {
            Usage::ProviderReported(t) => {
                window.reported = window
                    .reported
                    .saturating_add(t.input.saturating_add(t.output))
            }
            Usage::LocalEstimate(t) => {
                window.estimated = window
                    .estimated
                    .saturating_add(t.input.saturating_add(t.output))
            }
            Usage::Unavailable => window.unavailable = window.unavailable.saturating_add(1),
        }
    }
    out.reverse();
    out
}

/// Text safe to write to a terminal as one line.
///
/// Every C0 and C1 control, DEL, the Unicode line and paragraph separators
/// and the bidi controls become visible escapes (`\n`, `\r`, `\t`, else
/// `\u{1b}`), so provider or user text can move no cursor, set no title,
/// open no link, write no clipboard and reorder no display. Text that needs
/// nothing is borrowed. Display only: stored data is never changed.
pub fn untrusted(text: &str) -> Cow<'_, str> {
    fn unsafe_char(c: char) -> bool {
        matches!(c,
            '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{61c}' | '\u{200e}' | '\u{200f}'
            | '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }
    if !text.chars().any(unsafe_char) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if unsafe_char(c) => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Milliseconds since the Unix epoch as UTC `YYYY-MM-DDTHH:MM:SS.mmmZ`.
/// Civil-from-days arithmetic, exact before 1970 too.
pub fn timestamp(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let of_day = ms.rem_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        of_day / 3_600_000,
        of_day / 60_000 % 60,
        of_day / 1_000 % 60,
        of_day % 1_000
    )
}

/// `1234567` as `1,234,567`.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A token count for a narrow column: `999`, `1.2k`, `3.4M`. Truncates
/// rather than rounds, so a figure never reads larger than it is.
pub fn compact(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    // Tenths of a thousand, then of a million, and so on.
    let mut tenths = n / 100;
    for unit in ["k", "M", "G"] {
        if tenths < 10_000 {
            return format!("{}.{}{unit}", tenths / 10, tenths % 10);
        }
        tenths /= 1_000;
    }
    format!("{}.{}T", tenths / 10, tenths % 10)
}

/// One line saying what `a` holds, the same on every surface:
/// `1,234 in / 567 out reported; ~89 in / ~12 out estimated; 2 unavailable;
/// 1 pending`. Empty parts are omitted, estimates are marked `~`, and
/// reported and estimated are never summed into one number.
pub fn summary(a: &Aggregate) -> String {
    let mut parts = Vec::new();
    if a.reported.invocations > 0 {
        parts.push(format!(
            "{} in / {} out reported",
            grouped(a.reported.input),
            grouped(a.reported.output)
        ));
    }
    if a.estimated.invocations > 0 {
        parts.push(format!(
            "~{} in / ~{} out estimated",
            grouped(a.estimated.input),
            grouped(a.estimated.output)
        ));
    }
    if a.unavailable > 0 {
        parts.push(format!("{} unavailable", grouped(a.unavailable)));
    }
    if a.pending > 0 {
        parts.push(format!("{} pending", grouped(a.pending)));
    }
    if parts.is_empty() {
        "no invocations".into()
    } else {
        parts.join("; ")
    }
}

/// A task as scheduling sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOverview {
    pub id: TaskId,
    pub key: String,
    pub status: TaskStatus,
}

/// A plan as canonical state has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanOverview {
    pub plan: Plan,
    pub condition: Condition,
    pub tasks: Vec<TaskOverview>,
    /// Concerns raised and not yet decided by a human.
    pub concerns_awaiting: usize,
    /// The plan is paused, or a concern was decided `stop`.
    pub stopped_by_human: bool,
    /// Its planner's latest replan proposes completion.
    pub completion_proposed: bool,
    /// Its latest integration verification.
    pub integration: Option<Integration>,
    /// Unresolved records of this plan (see `Overview::unresolved`).
    pub unresolved: usize,
}

/// A snapshot of the whole project, for every surface.
///
/// Each plan's scheduling snapshot is one read transaction, so a plan's
/// condition and task statuses agree. The overview as a whole is read plan
/// by plan and the usage after, so under concurrent writers the parts may
/// come from slightly different moments; each is itself true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overview {
    /// Milliseconds, as event and invocation times are.
    pub taken_at: i64,
    pub plans: Vec<PlanOverview>,
    /// The project-wide scheduling capacity; `None` while there is no plan.
    pub capacity: Option<Capacity>,
    /// Every invocation's usage record.
    pub usage: Vec<UsageRecord>,
    /// Work a session recorded and nothing settled. Never classified as
    /// running or interrupted: if the recording agentctl process ended,
    /// `agentctl recover` settles it.
    pub unresolved: Vec<Unresolved>,
}

/// Reads the overview of `store` under the concurrency ceiling `limit`.
/// Uses read APIs only and never probes session liveness.
pub fn overview(store: &Store, limit: NonZeroU32) -> Result<Overview> {
    let taken_at = crate::state::now();
    let unresolved = store.unresolved(None)?;
    let mut capacity = None;
    let mut plans = Vec::new();
    for plan in store.plans()? {
        let snapshot = store.snapshot(plan.id, limit)?;
        capacity = Some(snapshot.capacity);
        let keys: BTreeMap<TaskId, String> = store
            .tasks(plan.id)?
            .into_iter()
            .map(|t| (t.id, t.key))
            .collect();
        let concerns = store.attention(plan.id)?;
        plans.push(PlanOverview {
            condition: snapshot.condition(),
            tasks: snapshot
                .tasks
                .iter()
                .map(|(id, status)| TaskOverview {
                    id: *id,
                    // Absent only if the task was added after the keys read.
                    key: keys
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| format!("task {id}")),
                    status: status.clone(),
                })
                .collect(),
            concerns_awaiting: concerns.iter().filter(|c| c.decision.is_none()).count(),
            stopped_by_human: plan.state == PlanState::Paused
                || concerns
                    .iter()
                    .any(|c| matches!(c.decision, Some((crate::state::HumanDecision::Stop, _)))),
            completion_proposed: store.completion_proposal(plan.id)?.is_some(),
            integration: store.integrations(plan.id)?.pop(),
            unresolved: unresolved.iter().filter(|u| u.plan == plan.id).count(),
            plan,
        });
    }
    Ok(Overview {
        taken_at,
        plans,
        capacity,
        usage: store.usage_records()?,
        unresolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::UNCHECKED;
    use crate::state::tests::{ended, objective, ready_plan, store, succeeded};
    use crate::state::{AgentScope, InvocationEnd, InvocationState, UnresolvedKind};

    fn id<T: std::str::FromStr>(n: u32) -> T
    where
        T::Err: std::fmt::Debug,
    {
        n.to_string().parse().unwrap()
    }

    fn tokens(input: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input,
            output,
            cached_input: None,
            cache_write: None,
            reasoning: None,
        }
    }

    fn record(n: u32, ended_at: Option<i64>, usage: Option<Usage>) -> UsageRecord {
        UsageRecord {
            invocation: id(n),
            agent: id(n),
            role: Role::Executor,
            plan: id(1),
            task: Some(id(1)),
            generation: None,
            provider: "claude".into(),
            model: "m".into(),
            state: InvocationState::Succeeded,
            started_at: 0,
            ended_at,
            usage,
        }
    }

    fn reported(n: u32, ended_at: i64, input: u64, output: u64) -> UsageRecord {
        record(
            n,
            Some(ended_at),
            Some(Usage::ProviderReported(tokens(input, output))),
        )
    }

    #[test]
    fn subsets_are_never_added_to_totals() {
        let usage = TokenUsage {
            input: 100,
            output: 50,
            cached_input: Some(80),
            cache_write: Some(10),
            reasoning: Some(30),
        };
        let a = total(&[record(1, Some(5), Some(Usage::ProviderReported(usage)))]);
        assert_eq!(a.reported.total(), 150);
        assert_eq!(a.reported.cached_input, Some(80));
        assert_eq!(a.reported.cache_write, Some(10));
        assert_eq!(a.reported.reasoning, Some(30));
        assert_eq!(a.estimated.total(), 0);
    }

    #[test]
    fn a_subset_is_known_only_if_every_invocation_reported_it() {
        let with = |cached| {
            Some(Usage::ProviderReported(TokenUsage {
                cached_input: cached,
                ..tokens(10, 1)
            }))
        };
        let all = total(&[
            record(1, Some(1), with(Some(4))),
            record(2, Some(2), with(Some(6))),
        ]);
        assert_eq!(all.reported.cached_input, Some(10));
        let some = total(&[
            record(1, Some(1), with(Some(4))),
            record(2, Some(2), with(None)),
            record(3, Some(3), with(Some(1))),
        ]);
        assert_eq!(some.reported.cached_input, None);
        assert_eq!(some.reported.input, 30);
        // Nothing contributed: nothing reported it.
        assert_eq!(Aggregate::default().reported.cached_input, None);
        // Overflow is unknown, and the totals saturate.
        let big = total(&[
            record(1, Some(1), with(Some(u64::MAX))),
            record(2, Some(2), with(Some(1))),
        ]);
        assert_eq!(big.reported.cached_input, None);
        let huge = Some(Usage::ProviderReported(tokens(u64::MAX, u64::MAX)));
        let s = total(&[record(1, Some(1), huge), record(2, Some(2), huge)]);
        assert_eq!((s.reported.input, s.reported.total()), (u64::MAX, u64::MAX));
    }

    #[test]
    fn provenance_is_never_merged() {
        let records = [
            reported(1, 10, 1_234, 567),
            record(2, Some(11), Some(Usage::LocalEstimate(tokens(89, 12)))),
            record(3, Some(12), Some(Usage::Unavailable)),
            record(4, Some(13), Some(Usage::Unavailable)),
            record(5, None, None),
        ];
        let a = total(&records);
        assert_eq!((a.reported.input, a.reported.output), (1_234, 567));
        assert_eq!((a.estimated.input, a.estimated.output), (89, 12));
        assert_eq!((a.unavailable, a.pending), (2, 1));
        assert_eq!(a.invocations(), 5);
        assert!(!a.complete());
        assert!(total(&records[..2]).complete());
        assert_eq!(
            summary(&a),
            "1,234 in / 567 out reported; ~89 in / ~12 out estimated; 2 unavailable; 1 pending"
        );
        assert_eq!(total(&records), a);
    }

    #[test]
    fn summaries_omit_empty_parts() {
        assert_eq!(summary(&Aggregate::default()), "no invocations");
        let only_estimate = total(&[record(1, Some(1), Some(Usage::LocalEstimate(tokens(3, 4))))]);
        assert_eq!(summary(&only_estimate), "~3 in / ~4 out estimated");
        let pending = total(&[record(1, None, None)]);
        assert_eq!(summary(&pending), "1 pending");
        let zero = total(&[reported(1, 1, 0, 0)]);
        assert_eq!(summary(&zero), "0 in / 0 out reported");
    }

    #[test]
    fn timestamps_are_utc_civil_time() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(timestamp(1_709_210_096_007), "2024-02-29T12:34:56.007Z");
        assert_eq!(timestamp(1_790_685_296_789), "2026-09-29T12:34:56.789Z");
        assert_eq!(timestamp(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(timestamp(-86_400_000), "1969-12-31T00:00:00.000Z");
    }

    #[test]
    fn compact_counts() {
        assert_eq!(compact(0), "0");
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1_000), "1.0k");
        assert_eq!(compact(1_299), "1.2k");
        assert_eq!(compact(999_999), "999.9k");
        assert_eq!(compact(1_000_000), "1.0M");
        assert_eq!(compact(3_450_000), "3.4M");
        assert_eq!(compact(2_000_000_000), "2.0G");
        assert_eq!(compact(u64::MAX), "18446744.0T");
    }

    #[test]
    fn groups_are_deterministic_and_keep_plan_level_work_apart() {
        let mut planner = record(1, Some(1), Some(Usage::ProviderReported(tokens(1, 1))));
        planner.role = Role::Planner;
        planner.task = None;
        let mut exec = reported(2, 2, 10, 10);
        exec.provider = "codex".into();
        let mut other = reported(3, 3, 5, 5);
        other.plan = id(2);
        other.task = Some(id(7));
        other.agent = id(9);
        let records = [other.clone(), exec.clone(), planner.clone()];

        let by_task = group(&records, Dimension::Task);
        assert_eq!(
            by_task.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            [
                GroupKey::Task {
                    plan: id(1),
                    task: None
                },
                GroupKey::Task {
                    plan: id(1),
                    task: Some(id(1))
                },
                GroupKey::Task {
                    plan: id(2),
                    task: Some(id(7))
                },
            ]
        );
        assert_eq!(by_task[0].1.reported.total(), 2);
        assert_eq!(by_task[1].1.reported.total(), 20);

        let by_provider = group(&records, Dimension::Provider);
        assert_eq!(by_provider.len(), 2);
        assert_eq!(by_provider[0].0, GroupKey::Provider("claude".into()));
        assert_eq!(by_provider[0].1.reported.total(), 12);
        assert_eq!(group(&records, Dimension::Plan).len(), 2);
        let by_role = group(&records, Dimension::Role);
        assert_eq!(by_role[0].0, GroupKey::Role(Role::Planner));
        assert_eq!(by_role[1].0, GroupKey::Role(Role::Executor));
        assert_eq!(group(&records, Dimension::Agent).len(), 3);

        // Every record counts once, in every dimension, in any input order.
        let reversed: Vec<_> = records.iter().rev().cloned().collect();
        for d in [
            Dimension::Provider,
            Dimension::Plan,
            Dimension::Task,
            Dimension::Role,
            Dimension::Agent,
        ] {
            let g = group(&records, d);
            assert_eq!(g.iter().map(|(_, a)| a.invocations()).sum::<u64>(), 3);
            assert_eq!(g, group(&reversed, d));
            assert_eq!(g, group(&records, d));
        }
    }

    const NOW: i64 = 1_000_000;

    fn windows(records: &[UsageRecord], n: usize) -> Vec<(u64, u64, u64)> {
        token_rate(records, NOW, n)
            .iter()
            .map(|w| (w.reported, w.estimated, w.unavailable))
            .collect()
    }

    #[test]
    fn no_records_is_all_zero_windows() {
        let w = token_rate(&[], NOW, 3);
        assert_eq!(w.len(), 3);
        assert!(
            w.iter()
                .all(|w| (w.reported, w.estimated, w.unavailable) == (0, 0, 0))
        );
        assert_eq!(w[2].end, NOW);
        assert_eq!(w[2].start, NOW - RATE_WINDOW_MS);
        assert_eq!(w[0].end, NOW - 2 * RATE_WINDOW_MS);
        assert!(token_rate(&[], NOW, 0).is_empty());
    }

    #[test]
    fn windows_are_oldest_first_and_sparse() {
        let records = [
            reported(1, NOW - 10, 3, 4),
            reported(2, NOW - 130_000, 1, 1),
        ];
        assert_eq!(
            windows(&records, 4),
            [(0, 0, 0), (2, 0, 0), (0, 0, 0), (7, 0, 0)]
        );
        // Older than every window: in none.
        assert_eq!(windows(&records, 1), [(7, 0, 0)]);
    }

    #[test]
    fn window_boundaries() {
        let at = |t| windows(&[reported(1, t, 1, 0)], 2);
        assert_eq!(at(NOW), [(0, 0, 0), (1, 0, 0)]);
        assert_eq!(at(NOW - RATE_WINDOW_MS + 1), [(0, 0, 0), (1, 0, 0)]);
        assert_eq!(at(NOW - RATE_WINDOW_MS), [(1, 0, 0), (0, 0, 0)]);
        assert_eq!(at(NOW - 2 * RATE_WINDOW_MS + 1), [(1, 0, 0), (0, 0, 0)]);
        assert_eq!(at(NOW - 2 * RATE_WINDOW_MS), [(0, 0, 0), (0, 0, 0)]);
        assert_eq!(at(NOW + 1), [(0, 0, 0), (0, 0, 0)]);
    }

    #[test]
    fn rate_keeps_provenance_and_unknowns_out_of_tokens() {
        let records = [
            reported(1, NOW - 5, 10, 5),
            record(2, Some(NOW - 6), Some(Usage::LocalEstimate(tokens(7, 3)))),
            record(3, Some(NOW - 7), Some(Usage::Unavailable)),
            record(4, None, None),
        ];
        assert_eq!(windows(&records, 1), [(15, 10, 1)]);
    }

    #[test]
    fn concurrent_agents_are_summed_exactly_and_recomputation_agrees() {
        let records: Vec<_> = (1..=200)
            .map(|n| reported(n, NOW - i64::from(n % 50), u64::from(n), 2))
            .collect();
        let expected: u64 = (1..=200u64).map(|n| n + 2).sum();
        let first = token_rate(&records, NOW, 3);
        assert_eq!(first[2].reported, expected);
        assert_eq!(first, token_rate(&records, NOW, 3));
    }

    fn adversarial() -> Vec<&'static str> {
        vec![
            "\x1b[31mred\x1b[0m \x1b[2J\x1b[H",
            "\x1b]8;;http://evil\x07click\x1b]8;;\x07",
            "\x1b]52;c;ZXZpbA==\x07",
            "\x1b]0;title\x07",
            "\u{9b}31m",
            "line\rOVERWRITE",
            "back\x08\x08space",
            "a\u{202e}txet\u{2066}x\u{2069}\u{200e}\u{200f}\u{61c}\u{202a}\u{202b}\u{202c}\u{202d}",
            "x\u{2028}y\u{2029}z\u{7f}\u{0}\x07\n\t",
        ]
    }

    fn is_unsafe(c: char) -> bool {
        c.is_control()
            || matches!(c,
                '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}'
                | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }

    #[test]
    fn untrusted_text_carries_no_terminal_control() {
        for text in adversarial() {
            let shown = untrusted(text);
            assert!(!shown.chars().any(is_unsafe), "{text:?} -> {shown:?}");
            assert!(matches!(shown, Cow::Owned(_)));
        }
        for c in (0u32..=0x9f).chain([0x61c, 0x200e, 0x200f, 0x2028, 0x2029]) {
            let c = char::from_u32(c).unwrap();
            if c.is_control() || is_unsafe(c) {
                let s = c.to_string();
                assert!(!untrusted(&s).chars().any(is_unsafe), "{c:?}");
            }
        }
        assert_eq!(untrusted("a\nb\rc\td"), "a\\nb\\rc\\td");
        assert_eq!(untrusted("\x1b[0m"), "\\u{1b}[0m");
        assert_eq!(untrusted("\u{9b}"), "\\u{9b}");
        assert_eq!(untrusted("\u{202e}"), "\\u{202e}");
    }

    #[test]
    fn printable_text_is_borrowed_unchanged() {
        for text in [
            "plain",
            "",
            "héllo wörld — 日本語 🎉",
            "a b  c",
            "\u{a0}\u{ff}",
        ] {
            assert!(matches!(untrusted(text), Cow::Borrowed(t) if t == text));
        }
    }

    const LIMIT: NonZeroU32 = NonZeroU32::new(4).unwrap();

    #[test]
    fn an_empty_store_has_an_empty_overview() {
        let (_dir, store) = store();
        let o = overview(&store, LIMIT).unwrap();
        assert!(o.plans.is_empty() && o.usage.is_empty() && o.unresolved.is_empty());
        assert_eq!(o.capacity, None);
        assert!(o.taken_at > 0);
    }

    #[test]
    fn plans_are_shown_as_canonical_state_has_them() {
        let (_dir, mut store) = store();
        let planning = store.create_plan(&objective("planning")).unwrap();
        let (ready, _) = ready_plan(&mut store, &[("r", &[], &[])]);
        let (running, tasks) =
            ready_plan(&mut store, &[("a", &["a.txt"], &[]), ("b", &[], &["a"])]);
        store.start_plan(running).unwrap();
        let crate::state::Claim::Claimed(generation) =
            store.claim(tasks[0], LIMIT, UNCHECKED).unwrap()
        else {
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
        let of = |p: PlanId| o.plans.iter().find(|x| x.plan.id == p).unwrap();
        assert_eq!(o.plans.len(), 4);
        assert_eq!(of(planning).plan.state, PlanState::Planning);
        assert_eq!(
            of(planning).condition,
            Condition::NotRunning(PlanState::Planning)
        );
        assert!(of(planning).tasks.is_empty());
        assert_eq!(of(ready).condition, Condition::NotRunning(PlanState::Ready));
        assert_eq!(of(ready).tasks[0].key, "r");

        let r = of(running);
        assert_eq!(r.condition, Condition::Waiting);
        assert_eq!(r.tasks[0].key, "a");
        assert_eq!(r.tasks[0].status, TaskStatus::Scheduled(generation));
        assert_eq!(
            r.tasks[1].status,
            TaskStatus::WaitingForDependencies(vec![tasks[0]])
        );
        assert_eq!(o.capacity, Some(Capacity { held: 1, limit: 4 }));

        let n = of(attention);
        assert_eq!(n.plan.state, PlanState::NeedsAttention);
        assert_eq!(n.concerns_awaiting, 1);
        assert!(!n.stopped_by_human && !n.completion_proposed);
        assert!(n.integration.is_none());
        assert_eq!(of(planning).concerns_awaiting, 0);
    }

    #[test]
    fn live_and_unavailable_invocations_stay_unknown() {
        let (_dir, mut store) = store();
        let plan = store.create_plan(&objective("p")).unwrap();
        let agent = store
            .create_agent(Role::Planner, AgentScope::Plan(plan))
            .unwrap();
        let unavailable = store.start_invocation(agent, "claude", "m", None).unwrap();
        store.invocation_running(unavailable).unwrap();
        store
            .finish_invocation(
                unavailable,
                &InvocationEnd {
                    usage: Usage::Unavailable,
                    ..succeeded()
                },
            )
            .unwrap();
        let cancelled = store.start_invocation(agent, "claude", "m", None).unwrap();
        store.invocation_running(cancelled).unwrap();
        store
            .finish_invocation(cancelled, &ended(InvocationState::Cancelled))
            .unwrap();
        let live = store.start_invocation(agent, "claude", "m", None).unwrap();
        store.invocation_running(live).unwrap();

        let o = overview(&store, LIMIT).unwrap();
        let a = total(&o.usage);
        assert_eq!(
            a.unavailable + a.reported.invocations + a.estimated.invocations,
            2
        );
        assert_eq!(a.pending, 1);
        assert!(!a.complete());
        let live_record = o.usage.iter().find(|r| r.invocation == live).unwrap();
        assert_eq!(live_record.usage, None);
        assert_ne!(live_record.state, InvocationState::Succeeded);
        assert!(
            o.unresolved
                .iter()
                .any(|u| u.kind == UnresolvedKind::Invocation(live) && u.plan == plan)
        );
        assert_eq!(o.plans[0].unresolved, o.unresolved.len());
        assert!(o.usage.iter().any(|r| r.usage == Some(Usage::Unavailable)));
        assert_eq!(overview(&store, LIMIT).unwrap().usage, o.usage);
    }
}
