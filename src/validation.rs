//! Semantic validation supplements serde and JSON Schema structural checks.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::protocol::*;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    #[error("{field}: {message}")]
    Invalid {
        field: &'static str,
        message: String,
    },
    #[error("duplicate task ID: {0}")]
    DuplicateTask(String),
    #[error("task {task} depends on missing task {dependency}")]
    MissingDependency { task: String, dependency: String },
    #[error("task {0} depends on itself")]
    SelfDependency(String),
    #[error("plan dependencies contain a cycle")]
    DependencyCycle,
}

pub trait Validate {
    fn validate(&self) -> Result<(), ValidationError>;
}

pub(crate) fn ensure(
    condition: bool,
    field: &'static str,
    message: impl Into<String>,
) -> Result<(), ValidationError> {
    if condition {
        Ok(())
    } else {
        Err(ValidationError::Invalid {
            field,
            message: message.into(),
        })
    }
}

fn nonempty(value: &str, field: &'static str) -> Result<(), ValidationError> {
    ensure(!value.trim().is_empty(), field, "must not be blank")
}

fn texts(values: &[String], field: &'static str) -> Result<(), ValidationError> {
    for value in values {
        nonempty(value, field)?;
    }
    unique(values, field)
}

fn optional_text(value: &Option<String>, field: &'static str) -> Result<(), ValidationError> {
    if let Some(value) = value {
        nonempty(value, field)?;
    }
    Ok(())
}

fn unique<T: Ord>(values: &[T], field: &'static str) -> Result<(), ValidationError> {
    ensure(
        values.iter().collect::<BTreeSet<_>>().len() == values.len(),
        field,
        "contains duplicates",
    )
}

pub(crate) fn repo_path(path: &str) -> Result<(), ValidationError> {
    ensure(
        !path.trim().is_empty()
            && !path
                .chars()
                .any(|c| c.is_control() || "\\:*?[]{}".contains(c))
            && path
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".."),
        "path",
        "must be a normalized repository-relative path without globs or traversal",
    )
}

impl Validate for VerificationRequirements {
    fn validate(&self) -> Result<(), ValidationError> {
        ensure(
            !self.requirement_refs.is_empty(),
            "verification.requirement_refs",
            "at least one check is mandatory",
        )?;
        texts(&self.requirement_refs, "verification.requirement_refs")
    }
}

impl Validate for TaskPacket {
    fn validate(&self) -> Result<(), ValidationError> {
        nonempty(&self.objective, "task.objective")?;
        ensure(
            !self.definition_of_done.is_empty(),
            "task.definition_of_done",
            "at least one completion criterion is required",
        )?;
        texts(&self.definition_of_done, "task.definition_of_done")?;
        self.verification.validate()?;
        if self.dependencies.contains(&self.task_id) {
            return Err(ValidationError::SelfDependency(
                self.task_id.as_str().into(),
            ));
        }
        unique(&self.dependencies, "task.dependencies")?;
        unique(&self.graph_entities, "task.graph_entities")?;
        texts(&self.invariant_refs, "task.invariant_refs")?;
        for scope in [&self.read_scope, &self.write_scope] {
            for entry in scope {
                repo_path(entry.path())?;
            }
            let paths: Vec<_> = scope.iter().map(ScopePath::path).collect();
            unique(&paths, "task.scope")?;
        }
        Ok(())
    }
}

impl Validate for PlanPacket {
    fn validate(&self) -> Result<(), ValidationError> {
        nonempty(&self.objective, "plan.objective")?;
        ensure(
            !self.tasks.is_empty(),
            "plan.tasks",
            "at least one task is required",
        )?;
        self.integration_verification.validate()?;
        let mut indegree = BTreeMap::new();
        for task in &self.tasks {
            task.validate()?;
            if indegree
                .insert(&task.task_id, task.dependencies.len())
                .is_some()
            {
                return Err(ValidationError::DuplicateTask(task.task_id.as_str().into()));
            }
        }
        let mut dependents: BTreeMap<&TaskId, Vec<&TaskId>> = BTreeMap::new();
        for task in &self.tasks {
            for dependency in &task.dependencies {
                if !indegree.contains_key(dependency) {
                    return Err(ValidationError::MissingDependency {
                        task: task.task_id.as_str().into(),
                        dependency: dependency.as_str().into(),
                    });
                }
                dependents
                    .entry(dependency)
                    .or_default()
                    .push(&task.task_id);
            }
        }
        // Iterative Kahn traversal avoids recursion on externally supplied plans.
        let mut ready: BTreeSet<_> = indegree
            .iter()
            .filter_map(|(id, n)| (*n == 0).then_some(*id))
            .collect();
        let mut visited = 0;
        while let Some(id) = ready.pop_first() {
            visited += 1;
            if let Some(children) = dependents.get(id) {
                for child in children {
                    let count = indegree
                        .get_mut(child)
                        .expect("validated dependency endpoint");
                    *count -= 1;
                    if *count == 0 {
                        ready.insert(*child);
                    }
                }
            }
        }
        if visited != self.tasks.len() {
            return Err(ValidationError::DependencyCycle);
        }
        Ok(())
    }
}

impl Validate for ResultPacket {
    fn validate(&self) -> Result<(), ValidationError> {
        ensure(
            (self.status == ResultStatus::Succeeded) == self.failure.is_none(),
            "result.failure",
            "failed/blocked results require failure information; successful results cannot contain it",
        )?;
        if let Some(failure) = &self.failure {
            nonempty(&failure.code, "result.failure.code")?;
            nonempty(&failure.summary, "result.failure.summary")?;
        }
        for path in &self.changed_paths {
            repo_path(path)?;
        }
        unique(&self.changed_paths, "result.changed_paths")?;
        unique(&self.changed_entities, "result.changed_entities")?;
        optional_text(&self.notes, "result.notes")
    }
}

impl Validate for LocationRef {
    fn validate(&self) -> Result<(), ValidationError> {
        ensure(
            self.path.is_some() || self.graph_entity.is_some(),
            "location",
            "requires a path or graph entity",
        )?;
        if let Some(path) = &self.path {
            repo_path(path)?;
        }
        if let Some(line) = self.line {
            ensure(
                line > 0 && self.path.is_some(),
                "location.line",
                "requires a path and a one-based line number",
            )?;
        }
        Ok(())
    }
}

impl Validate for VerificationFinding {
    fn validate(&self) -> Result<(), ValidationError> {
        nonempty(&self.problem, "finding.problem")?;
        ensure(
            !self.requirement_refs.is_empty() || !self.invariant_refs.is_empty(),
            "finding",
            "requires a requirement or invariant reference",
        )?;
        texts(&self.requirement_refs, "finding.requirement_refs")?;
        texts(&self.invariant_refs, "finding.invariant_refs")?;
        if let Some(location) = &self.location {
            location.validate()?;
        }
        Ok(())
    }
}

impl Validate for VerificationTarget {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Self::Integration {
            executor_job_ids, ..
        } = self
        {
            ensure(
                !executor_job_ids.is_empty(),
                "verification.target.executor_job_ids",
                "must identify the contributing executor jobs",
            )?;
            unique(executor_job_ids, "verification.target.executor_job_ids")?;
        }
        Ok(())
    }
}

impl Validate for VerificationPacket {
    fn validate(&self) -> Result<(), ValidationError> {
        self.target.validate()?;
        let independent = match &self.target {
            VerificationTarget::Packet {
                executor_job_id, ..
            } => executor_job_id != &self.verifier_job_id,
            VerificationTarget::Integration {
                executor_job_ids, ..
            } => !executor_job_ids.contains(&self.verifier_job_id),
        };
        ensure(
            independent,
            "verification.verifier_job_id",
            "must be independent of the executor jobs",
        )?;
        for finding in &self.findings {
            finding.validate()?;
        }
        texts(&self.requirement_refs, "verification.requirement_refs")?;
        texts(&self.invariant_refs, "verification.invariant_refs")?;
        optional_text(&self.notes, "verification.notes")?;
        match self.decision {
            VerificationDecision::Pass => {
                ensure(
                    !self.requirement_refs.is_empty(),
                    "verification.requirement_refs",
                    "PASS must identify the checks performed",
                )?;
                ensure(
                    !self.findings.iter().any(|f| {
                        matches!(
                            f.severity,
                            FindingSeverity::Error | FindingSeverity::Critical
                        )
                    }),
                    "verification.findings",
                    "PASS cannot contain error or critical findings",
                )?;
            }
            VerificationDecision::Reject => ensure(
                !self.findings.is_empty(),
                "verification.findings",
                "REJECT requires a structured finding",
            )?,
            VerificationDecision::Blocked => ensure(
                self.notes.is_some() || !self.findings.is_empty(),
                "verification",
                "BLOCKED requires a finding or explanatory note",
            )?,
        }
        Ok(())
    }
}

impl Validate for SourceStateRef {
    fn validate(&self) -> Result<(), ValidationError> {
        nonempty(&self.revision, "source_state.revision")?;
        optional_text(&self.worktree_diff_hash, "source_state.worktree_diff_hash")
    }
}

impl Validate for ResumePacket {
    fn validate(&self) -> Result<(), ValidationError> {
        nonempty(&self.next_action, "resume.next_action")?;
        unique(&self.completed_tasks, "resume.completed_tasks")?;
        unique(&self.pending_tasks, "resume.pending_tasks")?;
        ensure(
            !self
                .completed_tasks
                .iter()
                .any(|t| self.pending_tasks.contains(t)),
            "resume",
            "completed and pending tasks must be disjoint",
        )?;
        if let Some(active) = &self.active_task {
            ensure(
                if active.state == TaskState::Verified {
                    self.completed_tasks.contains(&active.task_id)
                } else {
                    self.pending_tasks.contains(&active.task_id)
                },
                "resume.active_task",
                "must agree with completed/pending task membership",
            )?;
        }
        if matches!(
            self.phase,
            ResumePhase::Complete | ResumePhase::IntegrationVerification
        ) {
            ensure(
                self.pending_tasks.is_empty() && !self.completed_tasks.is_empty(),
                "resume.phase",
                "integration verification and completion require all packets verified",
            )?;
        }
        if self.phase == ResumePhase::Complete {
            ensure(
                self.latest_verification.is_some(),
                "resume.latest_verification",
                "completion requires an integration verification reference",
            )?;
        }
        if let Some(source) = &self.source_state {
            source.validate()?;
        }
        Ok(())
    }
}

impl Validate for CommandSpec {
    fn validate(&self) -> Result<(), ValidationError> {
        nonempty(&self.program, "command.program")?;
        nonempty(&self.cwd, "command.cwd")?;
        ensure(
            !self.program.contains('\0')
                && !self.cwd.contains('\0')
                && !self.args.iter().any(|a| a.contains('\0')),
            "command",
            "argv and cwd cannot contain NUL",
        )
    }
}

impl Validate for EvidenceRecord {
    fn validate(&self) -> Result<(), ValidationError> {
        nonempty(&self.summary, "evidence.summary")?;
        if let Some(command) = &self.command {
            command.validate()?;
        }
        if let Some(source) = &self.source_state {
            source.validate()?;
        }
        if let Some(end) = self.finished_at_ms {
            ensure(
                end >= self.started_at_ms,
                "evidence.finished_at_ms",
                "precedes start",
            )?;
        }
        ensure(
            self.exit_status.is_none() || (self.command.is_some() && self.finished_at_ms.is_some()),
            "evidence.exit_status",
            "requires a finished command",
        )?;
        for (value, field) in [
            (&self.stdout_hash, "evidence.stdout_hash"),
            (&self.stderr_hash, "evidence.stderr_hash"),
            (&self.full_log_ref, "evidence.full_log_ref"),
        ] {
            optional_text(value, field)?;
        }
        Ok(())
    }
}

impl Validate for ProviderMetadata {
    fn validate(&self) -> Result<(), ValidationError> {
        nonempty(&self.provider, "provider.provider")?;
        optional_text(&self.model, "provider.model")
    }
}

impl Validate for AgentJob {
    fn validate(&self) -> Result<(), ValidationError> {
        ensure(
            self.role != AgentRole::Executor || self.task_id.is_some(),
            "job.task_id",
            "executor jobs require a bounded task",
        )?;
        if let Some(provider) = &self.provider {
            provider.validate()?;
        }
        if let Some(start) = self.started_at_ms {
            ensure(
                start >= self.created_at_ms,
                "job.started_at_ms",
                "precedes creation",
            )?;
        }
        if let Some(end) = self.finished_at_ms {
            ensure(
                end >= self.started_at_ms.unwrap_or(self.created_at_ms),
                "job.finished_at_ms",
                "precedes start/creation",
            )?;
        }
        ensure(
            self.state.is_terminal() == self.finished_at_ms.is_some(),
            "job.finished_at_ms",
            "must be present exactly for terminal jobs",
        )?;
        match self.state {
            JobState::Queued => ensure(
                self.started_at_ms.is_none(),
                "job.started_at_ms",
                "queued job has not started",
            )?,
            JobState::Running | JobState::Waiting | JobState::Succeeded | JobState::Failed => {
                ensure(
                    self.started_at_ms.is_some(),
                    "job.started_at_ms",
                    "required after a job starts",
                )?
            }
            JobState::Cancelled => {}
        }
        Ok(())
    }
}

impl Validate for EventContext {
    fn validate(&self) -> Result<(), ValidationError> {
        if let (Some(task), Some(packet)) = (&self.task_id, &self.packet_id) {
            ensure(
                task == packet,
                "context.packet_id",
                "must equal task_id in v1",
            )?;
        }
        if let Some(provider) = &self.provider {
            provider.validate()?;
        }
        Ok(())
    }
}

impl Validate for TokenUsageEvent {
    fn validate(&self) -> Result<(), ValidationError> {
        self.context.validate()?;
        let counts = [
            self.input_tokens,
            self.output_tokens,
            self.cached_tokens,
            self.reasoning_tokens,
            self.total_tokens,
        ];
        ensure(
            self.provenance != TokenUsageProvenance::Unknown || counts.iter().all(Option::is_none),
            "token_usage.provenance",
            "UNKNOWN carries no numeric counts; use ESTIMATED for estimates",
        )
    }
}

impl Validate for AgentEvent {
    fn validate(&self) -> Result<(), ValidationError> {
        nonempty(&self.event_id, "event.event_id")?;
        self.context.validate()?;
        match &self.event {
            AgentEventKind::AgentStarted | AgentEventKind::SymbolRead { .. } => {}
            AgentEventKind::AgentFinished { state } => ensure(
                state.is_terminal(),
                "event.state",
                "finished agents require a terminal job state",
            )?,
            AgentEventKind::TaskPacketLoaded { task_id } => {
                for id in [&self.context.task_id, &self.context.packet_id]
                    .into_iter()
                    .flatten()
                {
                    ensure(id == task_id, "event.task_id", "must agree with context")?;
                }
            }
            AgentEventKind::PlanStepStarted { step } => nonempty(step, "event.step")?,
            AgentEventKind::FileRead { path } | AgentEventKind::FileEdited { path } => {
                repo_path(path)?
            }
            AgentEventKind::ToolStarted {
                invocation_id,
                tool,
            } => {
                nonempty(invocation_id, "event.invocation_id")?;
                nonempty(tool, "event.tool")?;
            }
            AgentEventKind::ToolFinished { invocation_id, .. }
            | AgentEventKind::CommandFinished { invocation_id, .. } => {
                nonempty(invocation_id, "event.invocation_id")?
            }
            AgentEventKind::CommandStarted {
                invocation_id,
                command,
            } => {
                nonempty(invocation_id, "event.invocation_id")?;
                command.validate()?;
            }
            AgentEventKind::VerificationStarted { target } => target.validate()?,
            AgentEventKind::VerificationCheckStarted { requirement_ref } => {
                nonempty(requirement_ref, "event.requirement_ref")?
            }
            AgentEventKind::VerificationFindingCreated { finding } => finding.validate()?,
            AgentEventKind::WaitingOnDependency { task_ids } => {
                ensure(
                    !task_ids.is_empty(),
                    "event.task_ids",
                    "requires a dependency",
                )?;
                unique(task_ids, "event.task_ids")?;
            }
            AgentEventKind::ExperimentBoundary { boundary } => {
                boundary.validate()?;
                ensure(
                    boundary.timestamp_ms == self.timestamp_ms
                        && boundary.job_id == self.context.job_id,
                    "event.boundary",
                    "timestamp and job must agree with envelope",
                )?;
            }
            AgentEventKind::TokenUsageObserved { usage } => {
                usage.validate()?;
                ensure(
                    usage.timestamp_ms == self.timestamp_ms && usage.context == self.context,
                    "event.usage",
                    "timestamp and context must agree with envelope",
                )?;
            }
        }
        Ok(())
    }
}

impl Validate for ProbeSnapshot {
    fn validate(&self) -> Result<(), ValidationError> {
        self.context.validate()?;
        nonempty(&self.phase, "probe.phase")?;
        ensure(
            self.idle_ms <= self.elapsed_ms,
            "probe.idle_ms",
            "cannot exceed elapsed time",
        )?;
        if let Some(target) = &self.current_target {
            target.validate()?;
        }
        if let Some(command) = &self.current_command {
            command.validate()?;
        }
        for (value, field) in [
            (&self.current_step, "probe.current_step"),
            (&self.current_tool, "probe.current_tool"),
            (&self.last_event_id, "probe.last_event_id"),
            (&self.blocker, "probe.blocker"),
            (
                &self.current_verification_check,
                "probe.current_verification_check",
            ),
        ] {
            optional_text(value, field)?;
        }
        unique(&self.waiting_on, "probe.waiting_on")
    }
}

impl Validate for ExperimentBoundary {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::NoProgress { timeout_ms } => {
                ensure(*timeout_ms > 0, "boundary.timeout_ms", "must be positive")?
            }
            Self::NanMetric { metric } => nonempty(metric, "boundary.metric")?,
            Self::MetricThreshold { metric, value, .. } => {
                nonempty(metric, "boundary.metric")?;
                ensure(value.is_finite(), "boundary.value", "must be finite")?;
            }
            _ => {}
        }
        Ok(())
    }
}

impl Validate for ExperimentSpec {
    fn validate(&self) -> Result<(), ValidationError> {
        self.command.validate()?;
        if let Some(source) = &self.source_state {
            source.validate()?;
        }
        for (values, field) in [
            (&self.input_refs, "experiment.input_refs"),
            (&self.metric_refs, "experiment.metric_refs"),
            (&self.output_refs, "experiment.output_refs"),
        ] {
            texts(values, field)?;
        }
        let mut ids = BTreeSet::new();
        for boundary in &self.decision_boundaries {
            nonempty(&boundary.boundary_id, "boundary.boundary_id")?;
            ensure(
                ids.insert(&boundary.boundary_id),
                "boundary.boundary_id",
                "duplicate boundary",
            )?;
            boundary.condition.validate()?;
        }
        Ok(())
    }
}

impl Validate for ExperimentEvent {
    fn validate(&self) -> Result<(), ValidationError> {
        nonempty(&self.boundary_id, "experiment_event.boundary_id")?;
        nonempty(&self.summary, "experiment_event.summary")?;
        self.boundary.validate()?;
        for (metric, value) in &self.metrics {
            nonempty(metric, "experiment_event.metrics")?;
            ensure(
                value.is_finite(),
                "experiment_event.metrics",
                "must be finite; report NaN with a NAN_METRIC boundary",
            )?;
        }
        Ok(())
    }
}

impl Validate for MemoryProvenance {
    fn validate(&self) -> Result<(), ValidationError> {
        texts(&self.source_refs, "memory.source_refs")?;
        ensure(
            !self.source_refs.is_empty() || !self.evidence.is_empty(),
            "memory",
            "requires explicit provenance",
        )?;
        ensure(
            self.trust_class != MemoryTrustClass::AgentNote || self.author_job_id.is_some(),
            "memory.author_job_id",
            "agent notes require an author job",
        )
    }
}
