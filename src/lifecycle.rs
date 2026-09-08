//! Pure lifecycle predicates over caller-supplied state; no scheduler or state store.

use std::collections::BTreeMap;

use crate::protocol::*;
use crate::validation::{Validate, ValidationError, ensure};

impl TaskState {
    pub fn is_complete(self) -> bool {
        self == Self::Verified
    }

    /// Structural edge only. Use PlanPacket::validate_task_transition for DAG/proof guards.
    pub fn can_transition_to(self, next: Self) -> bool {
        use TaskState::*;
        matches!(
            (self, next),
            (Planned, Ready | Blocked)
                | (Ready, Executing | Blocked)
                | (Executing, AwaitingVerification | Blocked)
                | (AwaitingVerification, Verifying | Blocked)
                | (Verifying, Verified | Rejected | Blocked)
                | (Blocked, Planned)
        )
    }
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    pub fn validate_transition(self, next: Self) -> Result<(), ValidationError> {
        use JobState::*;
        ensure(
            matches!(
                (self, next),
                (Queued, Running | Cancelled)
                    | (Running, Waiting | Succeeded | Failed | Cancelled)
                    | (Waiting, Running | Failed | Cancelled)
            ),
            "job.transition",
            format!("{self:?} -> {next:?} is not allowed"),
        )
    }
}

impl PlanPacket {
    fn task(&self, task_id: &TaskId) -> Result<&TaskPacket, ValidationError> {
        self.tasks
            .iter()
            .find(|t| &t.task_id == task_id)
            .ok_or_else(|| ValidationError::Invalid {
                field: "task_id",
                message: format!("unknown task {}", task_id.as_str()),
            })
    }

    fn validate_states(&self, states: &BTreeMap<TaskId, TaskState>) -> Result<(), ValidationError> {
        self.validate()?;
        for (id, state) in states {
            let task = self.task(id)?;
            if !matches!(state, TaskState::Planned | TaskState::Blocked) {
                ensure(
                    Self::dependencies_verified(task, states),
                    "task.dependencies",
                    "active/completed tasks require VERIFIED prerequisites",
                )?;
            }
        }
        Ok(())
    }

    fn dependencies_verified(task: &TaskPacket, states: &BTreeMap<TaskId, TaskState>) -> bool {
        task.dependencies
            .iter()
            .all(|id| states.get(id) == Some(&TaskState::Verified))
    }

    /// Missing dependency state never satisfies a dependency. Only READY is runnable.
    pub fn task_is_runnable(
        &self,
        task_id: &TaskId,
        states: &BTreeMap<TaskId, TaskState>,
    ) -> Result<bool, ValidationError> {
        self.validate_states(states)?;
        let task = self.task(task_id)?;
        Ok(states.get(task_id) == Some(&TaskState::Ready)
            && Self::dependencies_verified(task, states))
    }

    /// Validates a proposed transition without applying it. Rejections require a new
    /// planner-authored correction packet; they never cycle back into execution.
    pub fn validate_task_transition(
        &self,
        task_id: &TaskId,
        states: &BTreeMap<TaskId, TaskState>,
        next: TaskState,
        verification: Option<&VerificationPacket>,
    ) -> Result<(), ValidationError> {
        self.validate_states(states)?;
        let task = self.task(task_id)?;
        let current = states
            .get(task_id)
            .ok_or_else(|| ValidationError::Invalid {
                field: "task.state",
                message: "current task state is missing".into(),
            })?;
        ensure(
            current.can_transition_to(next),
            "task.transition",
            format!("{current:?} -> {next:?} is not allowed"),
        )?;
        if matches!(next, TaskState::Ready | TaskState::Executing) {
            ensure(
                Self::dependencies_verified(task, states),
                "task.dependencies",
                "only VERIFIED prerequisites unlock work",
            )?;
        }
        let expected = match next {
            TaskState::Verified => Some(VerificationDecision::Pass),
            TaskState::Rejected => Some(VerificationDecision::Reject),
            TaskState::Blocked if verification.is_some() => Some(VerificationDecision::Blocked),
            _ => None,
        };
        if let Some(expected) = expected {
            ensure(
                *current == TaskState::Verifying,
                "task.verification",
                "verification decisions require VERIFYING state",
            )?;
            let packet = verification.ok_or_else(|| ValidationError::Invalid {
                field: "task.verification",
                message: "a verifier decision is mandatory".into(),
            })?;
            packet.validate()?;
            ensure(
                matches!(&packet.target, VerificationTarget::Packet { task_id: target, .. } if target == task_id),
                "task.verification.target",
                "requires packet verification for this task",
            )?;
            ensure(
                packet.decision == expected,
                "task.verification.decision",
                "does not match the requested state",
            )?;
            if expected == VerificationDecision::Pass {
                validate_requirements(&task.verification, packet)?;
                validate_invariants(&task.invariant_refs, packet)?;
            }
        } else {
            ensure(
                verification.is_none(),
                "task.verification",
                "unexpected verification for this transition",
            )?;
        }
        Ok(())
    }

    /// Individually verified tasks are necessary but insufficient for plan completion.
    pub fn validate_completion(
        &self,
        states: &BTreeMap<TaskId, TaskState>,
        integration: &VerificationPacket,
    ) -> Result<(), ValidationError> {
        self.validate_states(states)?;
        ensure(
            self.tasks
                .iter()
                .all(|t| states.get(&t.task_id) == Some(&TaskState::Verified)),
            "plan.completion",
            "every task must be VERIFIED",
        )?;
        integration.validate()?;
        ensure(
            matches!(&integration.target, VerificationTarget::Integration { plan_id, .. } if plan_id == &self.plan_id),
            "plan.completion.target",
            "requires integration verification for this plan",
        )?;
        ensure(
            integration.decision == VerificationDecision::Pass,
            "plan.completion.decision",
            "requires integration PASS",
        )?;
        validate_requirements(&self.integration_verification, integration)?;
        for task in &self.tasks {
            validate_invariants(&task.invariant_refs, integration)?;
        }
        Ok(())
    }
}

fn validate_requirements(
    requirements: &VerificationRequirements,
    packet: &VerificationPacket,
) -> Result<(), ValidationError> {
    ensure(
        requirements
            .requirement_refs
            .iter()
            .all(|r| packet.requirement_refs.contains(r)),
        "verification.requirement_refs",
        "PASS must cover every required check",
    )?;
    ensure(
        !requirements.evidence_required || !packet.evidence.is_empty(),
        "verification.evidence",
        "evidence is required for PASS",
    )
}

fn validate_invariants(
    invariants: &[String],
    packet: &VerificationPacket,
) -> Result<(), ValidationError> {
    ensure(
        invariants.iter().all(|r| packet.invariant_refs.contains(r)),
        "verification.invariant_refs",
        "PASS must cover every critical invariant",
    )
}
