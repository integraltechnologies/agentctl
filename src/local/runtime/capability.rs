//! Derived Stage 6 control-plane capabilities.
//!
//! This is an inspection projection, not another lifecycle or a capability
//! ledger. Findings are reconstructed from canonical plan/task state, the
//! journal, runtime artifacts, and ontology-generation history.
use super::*;
use crate::local::{
    graph::{DecisionReason, FootprintOutlook, GenerationOrigin, GenerationState},
    planning::PlanState,
};
use rusqlite::params;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ControlPlaneCapability {
    ExecuteAndIndependentlyVerifyBoundedTask,
    PreserveDependencyLockUntilVerification,
    KeepCandidateSeparateFromAcceptedTruth,
    RejectFailureWithoutAdvancingAcceptedTruth,
    RefuseStaleSource,
    MediateContextEscalation,
    ExposeStructuralEvidenceToIntegration,
    ResumeDurableWorkToCompletion,
    AtomicallyIntegrateAndAcceptFinalTruth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CapabilityStatus {
    Supported,
    NotDemonstrated,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CapabilityEvidenceKind {
    TaskVerification,
    DependencyTransition,
    OntologyGeneration,
    FailureBoundary,
    SourceDrift,
    ContextDecision,
    VerificationInput,
    ResumeCheckpoint,
    IntegrationAcceptance,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapabilityEvidence {
    pub kind: CapabilityEvidenceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependency_task_id: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor_job_id: Option<JobId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verifier_job_id: Option<JobId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_hash: Option<String>,
    pub detail: String,
}

impl CapabilityEvidence {
    fn new(kind: CapabilityEvidenceKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            sequence: None,
            task_id: None,
            dependency_task_id: None,
            executor_job_id: None,
            verifier_job_id: None,
            generation_id: None,
            artifact_hash: None,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CapabilityFinding {
    pub capability: ControlPlaneCapability,
    pub status: CapabilityStatus,
    pub evidence: Vec<CapabilityEvidence>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ControlPlaneCapabilityReport {
    pub version: &'static str,
    pub repository_id: RepositoryId,
    pub workspace_id: WorkspaceId,
    pub plan_id: PlanId,
    pub plan_state: PlanState,
    pub accepted_generation_id: Option<String>,
    pub final_source: Option<SourceStateRef>,
    pub capabilities: Vec<CapabilityFinding>,
}

#[derive(Clone)]
struct Transition {
    sequence: i64,
    task: TaskId,
    to: TaskState,
    proof: Option<VerificationPacket>,
}

#[derive(Clone)]
struct Phase {
    sequence: i64,
    phase: String,
    job: Option<JobId>,
}

fn bound_verifier_job(
    artifacts: &Artifacts,
    info: &RepositoryInfo,
    plan_id: &PlanId,
    task_id: Option<&TaskId>,
    job: &RuntimeJob,
    proof: &VerificationPacket,
) -> bool {
    job.role == AgentRole::Verifier
        && job.state == RuntimeJobState::Succeeded
        && job.plan_id.as_ref() == Some(plan_id)
        && job.task_id.as_ref() == task_id
        && job.workspace_id == info.workspace_id
        && job.reported_verification == Some(proof.decision)
        && job.context_request.is_none()
        && proof.verifier_job_id == job.job_id
        && artifacts
            .decode::<provider::JobInput>(&job.input)
            .is_ok_and(|input| {
                input.repository_id == info.repository_id
                    && input.workspace_id == info.workspace_id
                    && input.plan_id.as_ref() == Some(plan_id)
                    && input.task_id.as_ref() == task_id
                    && input.job_id == job.job_id
                    && input.role == AgentRole::Verifier
                    && serde_json::from_value::<VerificationTarget>(
                        input.artifact["target"].clone(),
                    )
                    .is_ok_and(|target| target == proof.target)
            })
        && job.output.as_ref().is_some_and(|output| {
            artifacts
                .decode::<VerificationPacket>(output)
                .is_ok_and(|stored| stored == *proof)
        })
}

fn finding(
    capability: ControlPlaneCapability,
    status: CapabilityStatus,
    evidence: Vec<CapabilityEvidence>,
    limitation: impl Into<String>,
) -> CapabilityFinding {
    let limitation = limitation.into();
    CapabilityFinding {
        capability,
        status,
        evidence,
        limitations: (status != CapabilityStatus::Supported && !limitation.is_empty())
            .then_some(limitation)
            .into_iter()
            .collect(),
    }
}

impl Store {
    /// Inspect what this plan's durable history actually demonstrates. This
    /// never persists a score or infers support from the mere presence of code.
    pub fn control_plane_capabilities(
        &self,
        artifacts: &Artifacts,
        start: &Path,
        id: &PlanId,
    ) -> Result<ControlPlaneCapabilityReport> {
        let info = graph::checked_workspace(self, start)?;
        let view = self.execution_plan(start, id)?;
        let tasks = self.execution_tasks(start, id)?;
        let run = self.runtime_status(start, id)?;
        let jobs = self.runtime_jobs(start, Some(id))?;
        let jobs_by_id: BTreeMap<_, _> = jobs.iter().map(|job| (&job.job_id, job)).collect();
        let transitions = self.capability_transitions(&info, id)?;
        let phases = self.capability_phases(&info, id)?;
        let generations: Vec<_> = self
            .ontology_generations(start, 1000)?
            .into_iter()
            .filter(|generation| {
                generation.plan() == Some(id)
                    || generation
                        .acceptance
                        .as_ref()
                        .and_then(|decision| decision.plan_id.as_ref())
                        == Some(id)
                    || generation
                        .closure
                        .as_ref()
                        .and_then(|decision| decision.plan_id.as_ref())
                        == Some(id)
            })
            .collect();
        let ontology = self.ontology_status(start)?;
        let mut capabilities = vec![];

        let mut verified = vec![];
        for transition in transitions
            .iter()
            .filter(|event| event.to == TaskState::Verified)
        {
            let Some(proof) = &transition.proof else {
                continue;
            };
            let VerificationTarget::Packet {
                task_id,
                executor_job_id,
            } = &proof.target
            else {
                continue;
            };
            let Some(executor) = jobs_by_id.get(executor_job_id) else {
                continue;
            };
            let Some(verifier) = jobs_by_id.get(&proof.verifier_job_id) else {
                continue;
            };
            let bounded = tasks
                .iter()
                .find(|task| &task.packet.task_id == task_id)
                .is_some_and(|task| {
                    !task.packet.read_scope.is_empty() || !task.packet.write_scope.is_empty()
                });
            if bounded
                && transition.task == *task_id
                && proof.decision == VerificationDecision::Pass
                && executor.role == AgentRole::Executor
                && executor.state == RuntimeJobState::Succeeded
                && executor.plan_id.as_ref() == Some(id)
                && executor.task_id.as_ref() == Some(task_id)
                && verifier.role == AgentRole::Verifier
                && verifier.state == RuntimeJobState::Succeeded
                && verifier.plan_id.as_ref() == Some(id)
                && verifier.task_id.as_ref() == Some(task_id)
                && executor.job_id != verifier.job_id
                && executor.workspace_id == info.workspace_id
                && verifier.workspace_id == info.workspace_id
                && bound_verifier_job(artifacts, &info, id, Some(task_id), verifier, proof)
                && run
                    .as_ref()
                    .and_then(|run| run.accepted.get(task_id))
                    .is_some_and(|accepted| {
                        accepted.executor == *executor_job_id
                            && accepted.verifier == proof.verifier_job_id
                            && artifacts
                                .decode::<CapturedDiff>(&accepted.diff)
                                .is_ok_and(|diff| {
                                    diff.plan_id == *id
                                        && diff.task_id.as_ref() == Some(task_id)
                                        && diff.executor_job_id.as_ref() == Some(executor_job_id)
                                        && artifacts
                                            .decode::<SourceSnapshot>(&diff.after)
                                            .and_then(|snapshot| snapshot.source_ref())
                                            .is_ok_and(|source| {
                                                artifacts
                                                    .decode::<provider::JobInput>(&verifier.input)
                                                    .is_ok_and(|input| input.source == source)
                                            })
                                })
                    })
            {
                let mut evidence = CapabilityEvidence::new(
                    CapabilityEvidenceKind::TaskVerification,
                    "bounded task reached VERIFIED through distinct successful executor and verifier jobs",
                );
                evidence.sequence = Some(transition.sequence);
                evidence.task_id = Some(task_id.clone());
                evidence.executor_job_id = Some(executor_job_id.clone());
                evidence.verifier_job_id = Some(proof.verifier_job_id.clone());
                verified.push(evidence);
            }
        }
        let runtime_missing = run.is_none();
        capabilities.push(finding(
            ControlPlaneCapability::ExecuteAndIndependentlyVerifyBoundedTask,
            if !verified.is_empty() {
                CapabilityStatus::Supported
            } else if runtime_missing {
                CapabilityStatus::Unavailable
            } else {
                CapabilityStatus::NotDemonstrated
            },
            verified.clone(),
            if runtime_missing {
                "this plan has no controller-owned runtime history"
            } else {
                "no task has a complete canonical executor/verifier proof"
            },
        ));

        let dependencies: Vec<_> = view
            .plan
            .packet
            .tasks
            .iter()
            .flat_map(|task| {
                task.dependencies
                    .iter()
                    .map(move |dependency| (task, dependency))
            })
            .collect();
        let has_dependencies = !dependencies.is_empty();
        let mut dependency_evidence = vec![];
        let mut all_unlocked_after_verification = has_dependencies;
        for (task, dependency) in dependencies {
            let verified_at = verified
                .iter()
                .find(|evidence| evidence.task_id.as_ref() == Some(dependency))
                .and_then(|evidence| evidence.sequence);
            let ready_at = transitions
                .iter()
                .find(|event| event.task == task.task_id && event.to == TaskState::Ready)
                .map(|event| event.sequence);
            match (verified_at, ready_at) {
                (Some(verified_at), Some(ready_at)) if verified_at < ready_at => {
                    let mut evidence = CapabilityEvidence::new(
                        CapabilityEvidenceKind::DependencyTransition,
                        "dependent task first became READY only after its dependency was VERIFIED",
                    );
                    evidence.sequence = Some(ready_at);
                    evidence.task_id = Some(task.task_id.clone());
                    evidence.dependency_task_id = Some(dependency.clone());
                    dependency_evidence.push(evidence);
                }
                _ => all_unlocked_after_verification = false,
            }
        }
        capabilities.push(finding(
            ControlPlaneCapability::PreserveDependencyLockUntilVerification,
            if all_unlocked_after_verification {
                CapabilityStatus::Supported
            } else if !has_dependencies {
                CapabilityStatus::Unavailable
            } else {
                CapabilityStatus::NotDemonstrated
            },
            dependency_evidence,
            if !has_dependencies {
                "the plan contains no dependency edge"
            } else {
                "not every dependency edge has a completed VERIFIED-before-READY transition"
            },
        ));

        let mut candidate_evidence = vec![];
        for generation in &generations {
            let safely_decided = generation.acceptance.as_ref().is_none_or(|decision| {
                decision.reason == DecisionReason::IntegrationVerified
                    && decision.plan_id.as_ref() == Some(id)
                    && matches!(
                        &generation.origin,
                        GenerationOrigin::Runtime { plan_id, .. } if plan_id == id
                    )
            });
            if safely_decided {
                let mut evidence = CapabilityEvidence::new(
                    CapabilityEvidenceKind::OntologyGeneration,
                    format!(
                        "runtime observation ended {:?}; acceptance, if any, was integration-authorized",
                        generation.state
                    ),
                );
                evidence.task_id = match &generation.origin {
                    graph::GenerationOrigin::Runtime { task_id, .. } => task_id.clone(),
                    graph::GenerationOrigin::External => None,
                };
                evidence.generation_id = Some(generation.generation_id.clone());
                candidate_evidence.push(evidence);
            }
        }
        capabilities.push(finding(
            ControlPlaneCapability::KeepCandidateSeparateFromAcceptedTruth,
            if !candidate_evidence.is_empty() && candidate_evidence.len() == generations.len() {
                CapabilityStatus::Supported
            } else if runtime_missing {
                CapabilityStatus::Unavailable
            } else {
                CapabilityStatus::NotDemonstrated
            },
            candidate_evidence,
            "no plan-owned ontology candidate lifecycle is evidenced",
        ));

        let task_rejected = transitions
            .iter()
            .find(|event| event.to == TaskState::Rejected);
        let integration_rejected = generations.iter().find(|generation| {
            generation
                .closure
                .as_ref()
                .is_some_and(|decision| decision.reason == DecisionReason::IntegrationRejected)
        });
        let executor_failed = jobs.iter().find(|job| {
            job.role == AgentRole::Executor
                && matches!(
                    job.state,
                    RuntimeJobState::Failed | RuntimeJobState::Interrupted
                )
        });
        // Acceptance is historical. A later external acceptance may retire a
        // generation, but cannot change whether this plan ever promoted it.
        let accepted_by_plan = generations.iter().any(|generation| {
            generation.acceptance.as_ref().is_some_and(|decision| {
                decision.reason == DecisionReason::IntegrationVerified
                    && decision.plan_id.as_ref() == Some(id)
            })
        });
        let mut failure_evidence = vec![];
        if let Some(rejected) = task_rejected.filter(|rejected| {
            rejected.proof.as_ref().is_some_and(|proof| {
                proof.decision == VerificationDecision::Reject
                    && matches!(
                        &proof.target,
                        VerificationTarget::Packet { task_id, .. } if task_id == &rejected.task
                    )
                    && jobs_by_id.get(&proof.verifier_job_id).is_some_and(|job| {
                        bound_verifier_job(artifacts, &info, id, Some(&rejected.task), job, proof)
                            && run
                                .as_ref()
                                .and_then(|run| run.pending.as_ref())
                                .is_some_and(|pending| {
                                    pending.task_id == rejected.task
                                        && pending.verifier.as_ref() == Some(&proof.verifier_job_id)
                                        && pending.proof.as_ref() == Some(proof)
                                        && matches!(
                                            &proof.target,
                                            VerificationTarget::Packet { executor_job_id, .. }
                                                if executor_job_id == &pending.executor
                                        )
                                        && artifacts
                                            .decode::<SourceSnapshot>(&pending.after)
                                            .and_then(|snapshot| snapshot.source_ref())
                                            .is_ok_and(|source| {
                                                artifacts
                                                    .decode::<provider::JobInput>(&job.input)
                                                    .is_ok_and(|input| input.source == source)
                                            })
                                })
                    })
            })
        }) {
            let mut evidence = CapabilityEvidence::new(
                CapabilityEvidenceKind::FailureBoundary,
                "the rejecting proof, verifier job, task transition, and absence of plan acceptance form one causal chain",
            );
            evidence.sequence = Some(rejected.sequence);
            evidence.task_id = Some(rejected.task.clone());
            failure_evidence.push(evidence);
        }
        if let Some(generation) = integration_rejected.filter(|generation| {
            generation.state == GenerationState::Rejected
                && generation.base.is_some()
                && matches!(
                    &generation.origin,
                    GenerationOrigin::Runtime { plan_id, .. } if plan_id == id
                )
                && generation.closure.as_ref().is_some_and(|decision| {
                    decision.plan_id.as_ref() == Some(id)
                        && decision.reason == DecisionReason::IntegrationRejected
                })
        }) {
            let mut evidence = CapabilityEvidence::new(
                CapabilityEvidenceKind::FailureBoundary,
                "the plan-owned candidate was rejected against its accepted base and was never accepted",
            );
            evidence.generation_id = Some(generation.generation_id.clone());
            failure_evidence.push(evidence);
        }
        if let Some(job) = executor_failed.filter(|job| {
            job.plan_id.as_ref() == Some(id)
                && job.workspace_id == info.workspace_id
                && run.as_ref().is_some_and(|run| {
                    run.plan_id == *id
                        && run.workspace_id == info.workspace_id
                        && run.state == RunState::Blocked
                })
        }) {
            let mut evidence = CapabilityEvidence::new(
                CapabilityEvidenceKind::FailureBoundary,
                "the plan-owned executor failure is bound to the blocked run and no plan generation was accepted",
            );
            evidence.task_id = job.task_id.clone();
            evidence.executor_job_id = Some(job.job_id.clone());
            failure_evidence.push(evidence);
        }
        if let Some(phase) = phases.iter().find(|phase| {
            task_rejected.is_none()
                && integration_rejected.is_none()
                && executor_failed.is_none()
                && phase.phase == "BLOCKED_NEEDS_PLANNER"
                && run.as_ref().is_some_and(|run| {
                    run.plan_id == *id
                        && run.workspace_id == info.workspace_id
                        && run.state == RunState::Blocked
                })
        }) {
            let mut evidence = CapabilityEvidence::new(
                CapabilityEvidenceKind::FailureBoundary,
                "runtime validation blocked the plan without accepting its result",
            );
            evidence.sequence = Some(phase.sequence);
            evidence.executor_job_id = phase.job.clone();
            failure_evidence.push(evidence);
        }
        capabilities.push(finding(
            ControlPlaneCapability::RejectFailureWithoutAdvancingAcceptedTruth,
            if !failure_evidence.is_empty()
                && !accepted_by_plan
                && view.state != PlanState::Complete
            {
                CapabilityStatus::Supported
            } else if runtime_missing {
                CapabilityStatus::Unavailable
            } else {
                CapabilityStatus::NotDemonstrated
            },
            failure_evidence,
            "this plan has no contained executor/verifier/integration failure",
        ));

        let drift: Vec<_> = phases
            .iter()
            .filter(|phase| {
                phase.phase == "SOURCE_DRIFT_DETECTED"
                    && run.as_ref().is_some_and(|run| {
                        run.plan_id == *id
                            && run.workspace_id == info.workspace_id
                            && run.state == RunState::Blocked
                            && run
                                .reason
                                .as_deref()
                                .is_some_and(|reason| reason.contains("SOURCE_DRIFT"))
                            && artifacts.decode::<SourceSnapshot>(&run.expected).is_ok_and(
                                |snapshot| {
                                    snapshot.repository_id == info.repository_id
                                        && snapshot.workspace_id == info.workspace_id
                                        && snapshot.head
                                            == view.plan.metadata.source.observation.head_commit
                                        && run.policy_hash == view.plan.metadata.source.policy_hash
                                },
                            )
                    })
            })
            .map(|phase| {
                let mut evidence = CapabilityEvidence::new(
                    CapabilityEvidenceKind::SourceDrift,
                    "runtime refused work after a canonical source-drift check failed",
                );
                evidence.sequence = Some(phase.sequence);
                evidence.executor_job_id = phase.job.clone();
                evidence
            })
            .collect();
        capabilities.push(finding(
            ControlPlaneCapability::RefuseStaleSource,
            if !drift.is_empty() && !accepted_by_plan && view.state != PlanState::Complete {
                CapabilityStatus::Supported
            } else if runtime_missing {
                CapabilityStatus::Unavailable
            } else {
                CapabilityStatus::NotDemonstrated
            },
            drift,
            "no source-drift refusal is recorded for this plan",
        ));

        let mut context_evidence = vec![];
        if let Some(run) = &run {
            for ledger in run.context.values() {
                for round in &ledger.rounds {
                    if !matches!(
                        round.outcome,
                        context::RoundOutcome::Approved | context::RoundOutcome::PlannerDenied
                    ) {
                        continue;
                    }
                    let Some(decision_ref) = &round.decision else {
                        continue;
                    };
                    let Some(requester) = jobs_by_id.get(&round.job_id) else {
                        continue;
                    };
                    let request_bound = requester.plan_id.as_ref() == Some(id)
                        && requester.workspace_id == info.workspace_id
                        && requester.role == ledger.role
                        && requester.task_id == ledger.task_id
                        && requester.context_request.as_ref() == Some(&round.request)
                        && artifacts
                            .decode::<ContextRequest>(&round.request)
                            .is_ok_and(|request| {
                                request.job_id == round.job_id && request.task_id == ledger.task_id
                            });
                    let artifacts_bound = artifacts
                        .decode::<context::ContextResolution>(&round.resolution)
                        .is_ok_and(|resolution| {
                            resolution.request_hash == round.request.hash
                                && ledger.base.as_ref().is_some_and(|base| {
                                    resolution.graph_generation == base.graph_generation
                                        && resolution.source_hash == base.source_hash
                                })
                        });
                    let Ok(decision) = artifacts.decode::<context::ContextDecision>(decision_ref)
                    else {
                        continue;
                    };
                    let decision_bound = decision.plan_id == *id
                        && Some(&decision.task_id) == ledger.task_id.as_ref()
                        && decision.request_hash == round.request.hash;
                    let valid = match round.outcome {
                        context::RoundOutcome::PlannerDenied => {
                            decision.decision == context::DecisionKind::Deny
                                && decision.read_scope_additions.is_empty()
                                && round.delta.is_none()
                                && run.context.values().all(|other| {
                                    other.deltas().all(|delta_ref| {
                                        artifacts
                                            .decode::<context::ContextDelta>(&delta_ref.artifact)
                                            .is_ok_and(|delta| {
                                                !matches!(
                                                    &delta.grant,
                                                    context::Grant::PlannerApproved {
                                                        decision_hash,
                                                        ..
                                                    } if decision_hash == &decision_ref.hash
                                                )
                                            })
                                    })
                                })
                        }
                        context::RoundOutcome::Approved => {
                            decision.decision == context::DecisionKind::Approve
                                && round.delta.as_ref().is_some_and(|delta_ref| {
                                artifacts
                                    .decode::<context::ContextDelta>(&delta_ref.artifact)
                                    .is_ok_and(|delta| {
                                        delta.delta_id == delta_ref.delta_id
                                            && delta.plan_id == *id
                                            && delta.task_id == ledger.task_id
                                            && delta.role == ledger.role
                                            && delta.parent_job_id == round.job_id
                                            && delta.request_hash == round.request.hash
                                            && delta.round == round.round + 1
                                            && ledger.base.as_ref().is_some_and(|base| {
                                                delta.graph_generation == base.graph_generation
                                                    && delta.source_hash == base.source_hash
                                            })
                                            && matches!(
                                                &delta.grant,
                                                context::Grant::PlannerApproved {
                                                    decision_hash,
                                                    scope_additions,
                                                } if decision_hash == &decision_ref.hash
                                                    && scope_additions
                                                        == &decision.read_scope_additions
                                            )
                                            && jobs.iter().any(|job| {
                                                job.plan_id.as_ref() == Some(id)
                                                    && job.workspace_id == info.workspace_id
                                                    && job.role == ledger.role
                                                    && job.task_id == ledger.task_id
                                                    && job.job_id != round.job_id
                                                    && job.session_id != requester.session_id
                                                    && job.created_at_ms > requester.created_at_ms
                                                    && job.context_manifest.as_ref().is_some_and(
                                                        |manifest| {
                                                            manifest.context_round
                                                                == Some(delta.round)
                                                                && manifest.graph_generation
                                                                    == delta.graph_generation
                                                                && manifest.context_deltas.iter().any(
                                                                    |issued| {
                                                                        issued.delta_id
                                                                            == delta.delta_id
                                                                            && issued.hash
                                                                                == delta_ref.artifact.hash
                                                                            && issued.parent_job_id
                                                                                == round.job_id
                                                                            && issued.planner_approved
                                                                    },
                                                                )
                                                                && artifacts
                                                                    .decode::<provider::JobInput>(
                                                                        &job.input,
                                                                    )
                                                                    .is_ok_and(|input| {
                                                                        input.artifact["deltas"]
                                                                            .as_array()
                                                                            .is_some_and(|deltas| {
                                                                                serde_json::to_value(&delta)
                                                                                    .is_ok_and(|value| {
                                                                                        deltas.contains(&value)
                                                                                    })
                                                                            })
                                                                    })
                                                        },
                                                    )
                                            })
                                    })
                            })
                        }
                        _ => false,
                    };
                    if request_bound && artifacts_bound && decision_bound && valid {
                        let mut evidence = CapabilityEvidence::new(
                            CapabilityEvidenceKind::ContextDecision,
                            format!(
                                "request, decision, exact delta (if approved), and fresh provider job are identity-bound for {:?}",
                                round.outcome
                            ),
                        );
                        evidence.task_id = ledger.task_id.clone();
                        evidence.executor_job_id = Some(round.job_id.clone());
                        evidence.artifact_hash = Some(decision_ref.hash.clone());
                        context_evidence.push(evidence);
                    }
                }
            }
        }
        capabilities.push(finding(
            ControlPlaneCapability::MediateContextEscalation,
            if !context_evidence.is_empty() {
                CapabilityStatus::Supported
            } else if runtime_missing {
                CapabilityStatus::Unavailable
            } else {
                CapabilityStatus::NotDemonstrated
            },
            context_evidence,
            "no escalation has a complete request/decision/grant-or-denial provenance chain",
        ));

        let mut structural = vec![];
        for job in jobs.iter().filter(|job| {
            job.role == AgentRole::Verifier
                && job.task_id.is_none()
                && job.state == RuntimeJobState::Succeeded
        }) {
            let input: provider::JobInput = artifacts.decode(&job.input)?;
            let Some(output) = &job.output else {
                continue;
            };
            let Ok(proof) = artifacts.decode::<VerificationPacket>(output) else {
                continue;
            };
            let Ok(target) =
                serde_json::from_value::<VerificationTarget>(input.artifact["target"].clone())
            else {
                continue;
            };
            let Ok(footprint) = serde_json::from_value::<FootprintOutlook>(
                input.artifact["structural_footprint"].clone(),
            ) else {
                continue;
            };
            let candidate = generations.iter().find(|generation| {
                generation.generation_id == footprint.report.to.generation_id
                    && generation.base.as_ref() == Some(&footprint.report.from.generation_id)
                    && generation.generation == footprint.report.to.generation
                    && generation.snapshot == footprint.report.to.snapshot
                    && matches!(
                        &generation.origin,
                        GenerationOrigin::Runtime { plan_id, .. } if plan_id == id
                    )
            });
            let executor_ids: Vec<_> = run
                .as_ref()
                .map(|run| {
                    run.accepted
                        .values()
                        .map(|task| task.executor.clone())
                        .collect()
                })
                .unwrap_or_default();
            let candidate_was_evaluated =
                candidate.is_some_and(|generation| match proof.decision {
                    VerificationDecision::Pass => {
                        view.state == PlanState::Complete
                            && generation.acceptance.as_ref().is_some_and(|decision| {
                                decision.reason == DecisionReason::IntegrationVerified
                                    && decision.plan_id.as_ref() == Some(id)
                            })
                    }
                    VerificationDecision::Reject => {
                        generation.state == GenerationState::Rejected
                            && generation.closure.as_ref().is_some_and(|decision| {
                                decision.reason == DecisionReason::IntegrationRejected
                                    && decision.plan_id.as_ref() == Some(id)
                            })
                    }
                    VerificationDecision::Blocked => false,
                });
            let target_bound = matches!(
                (&target, &proof.target),
                (
                    VerificationTarget::Integration { plan_id: input_plan, executor_job_ids: input_jobs },
                    VerificationTarget::Integration { plan_id: proof_plan, executor_job_ids: proof_jobs }
                ) if input_plan == id
                    && proof_plan == id
                    && input_jobs == proof_jobs
                    && input_jobs == &executor_ids
            );
            if bound_verifier_job(artifacts, &info, id, None, job, &proof)
                && input.repository_id == info.repository_id
                && input.workspace_id == info.workspace_id
                && input.plan_id.as_ref() == Some(id)
                && input.job_id == job.job_id
                && input.role == AgentRole::Verifier
                && proof.verifier_job_id == job.job_id
                && job.output.as_ref().is_some_and(|output| {
                    artifacts
                        .decode::<VerificationPacket>(output)
                        .is_ok_and(|stored| stored == proof)
                })
                && footprint.report.workspace_id == info.workspace_id
                && footprint.verification.plan_id == *id
                && target_bound
                && run.as_ref().is_some_and(|run| {
                    artifacts
                        .decode::<SourceSnapshot>(&run.expected)
                        .and_then(|snapshot| snapshot.source_ref())
                        .is_ok_and(|source| input.source == source)
                })
                && candidate_was_evaluated
            {
                let mut evidence = CapabilityEvidence::new(
                    CapabilityEvidenceKind::VerificationInput,
                    "the integration job input, proof target, accepted-base delta, and exact evaluated candidate are identity-bound",
                );
                evidence.verifier_job_id = Some(job.job_id.clone());
                evidence.artifact_hash = Some(job.input.hash.clone());
                evidence.generation_id = Some(footprint.report.to.generation_id);
                structural.push(evidence);
            }
        }
        capabilities.push(finding(
            ControlPlaneCapability::ExposeStructuralEvidenceToIntegration,
            if !structural.is_empty() {
                CapabilityStatus::Supported
            } else if runtime_missing {
                CapabilityStatus::Unavailable
            } else {
                CapabilityStatus::NotDemonstrated
            },
            structural,
            "no integration result is bound to structural evidence for the exact candidate it evaluated",
        ));

        let mut resumed = vec![];
        if let Some(run) = &run
            && run.state == RunState::Complete
            && view.state == PlanState::Complete
            && run.accepted.len() == tasks.len()
        {
            for phase in phases
                .iter()
                .filter(|phase| phase.phase == "PLAN_RUNTIME_RESUMED")
            {
                let durable_work_before = run.accepted.iter().any(|(task_id, accepted)| {
                    let accepted_job_completed_before = phases.iter().any(|prior| {
                        prior.sequence < phase.sequence
                            && prior.phase == "JOB_SUCCEEDED"
                            && prior.job.as_ref() == Some(&accepted.executor)
                    });
                    let accepted_executor_is_bound =
                        jobs_by_id.get(&accepted.executor).is_some_and(|executor| {
                            executor.role == AgentRole::Executor
                                && executor.state == RuntimeJobState::Succeeded
                                && executor.plan_id.as_ref() == Some(id)
                                && executor.task_id.as_ref() == Some(task_id)
                                && executor.workspace_id == info.workspace_id
                                && artifacts
                                    .decode::<provider::JobInput>(&executor.input)
                                    .is_ok_and(|input| {
                                        input.repository_id == info.repository_id
                                            && input.workspace_id == info.workspace_id
                                            && input.plan_id.as_ref() == Some(id)
                                            && input.task_id.as_ref() == Some(task_id)
                                            && input.job_id == executor.job_id
                                            && input.role == AgentRole::Executor
                                    })
                        });
                    accepted_job_completed_before && accepted_executor_is_bound
                });
                let completed_after = phases.iter().any(|later| {
                    later.sequence > phase.sequence && later.phase == "PLAN_RUNTIME_COMPLETED"
                });
                if durable_work_before && completed_after {
                    let mut evidence = CapabilityEvidence::new(
                        CapabilityEvidenceKind::ResumeCheckpoint,
                        "a finally accepted executor completed before the resume marker and the same plan completed afterward",
                    );
                    evidence.sequence = Some(phase.sequence);
                    resumed.push(evidence);
                }
            }
        }
        capabilities.push(finding(
            ControlPlaneCapability::ResumeDurableWorkToCompletion,
            if !resumed.is_empty() {
                CapabilityStatus::Supported
            } else if runtime_missing {
                CapabilityStatus::Unavailable
            } else {
                CapabilityStatus::NotDemonstrated
            },
            resumed,
            "no finally accepted work precedes a resume marker followed by coherent completion",
        ));

        let mut integrated = vec![];
        if view.state == PlanState::Complete
            && let (Some(proof), Some(final_source), Some(run)) =
                (&view.integration_proof, &view.final_source, &run)
        {
            let proof_hash = planning::hash(proof)?;
            let source_hash = planning::hash(final_source)?;
            let accepted: Vec<_> = generations
                .iter()
                .filter(|generation| {
                    matches!(
                        generation.state,
                        GenerationState::Accepted | GenerationState::Retired
                    ) && matches!(
                        &generation.origin,
                        GenerationOrigin::Runtime { plan_id, .. } if plan_id == id
                    ) && generation.acceptance.as_ref().is_some_and(|decision| {
                        decision.reason == DecisionReason::IntegrationVerified
                            && decision.plan_id.as_ref() == Some(id)
                            && decision.verification_hash.as_deref() == Some(&proof_hash)
                            && decision.source_hash.as_deref() == Some(&source_hash)
                    })
                })
                .collect();
            let expected_matches = artifacts
                .decode::<SourceSnapshot>(&run.expected)
                .and_then(|snapshot| snapshot.source_ref())
                .is_ok_and(|source| source == *final_source);
            let integration_job = jobs_by_id.get(&proof.verifier_job_id);
            let expected_target = VerificationTarget::Integration {
                plan_id: id.clone(),
                executor_job_ids: run
                    .accepted
                    .values()
                    .map(|task| task.executor.clone())
                    .collect(),
            };
            let target_matches = matches!(
                &proof.target,
                VerificationTarget::Integration { plan_id, executor_job_ids }
                    if plan_id == id
                        && executor_job_ids
                            == &run.accepted.values().map(|task| task.executor.clone()).collect::<Vec<_>>()
            );
            if let [generation] = accepted.as_slice()
                && proof.decision == VerificationDecision::Pass
                && target_matches
                && expected_matches
                && run.state == RunState::Complete
                && integration_job.is_some_and(|job| {
                    bound_verifier_job(artifacts, &info, id, None, job, proof)
                        && artifacts
                            .decode::<provider::JobInput>(&job.input)
                            .is_ok_and(|input| {
                                serde_json::from_value::<FootprintOutlook>(
                                    input.artifact["structural_footprint"].clone(),
                                )
                                .is_ok_and(|footprint| {
                                    input.source == *final_source
                                        && input.plan_id.as_ref() == Some(id)
                                        && input.job_id == job.job_id
                                        && serde_json::from_value::<VerificationTarget>(
                                            input.artifact["target"].clone(),
                                        )
                                        .is_ok_and(|target| target == expected_target)
                                        && footprint.report.to.generation_id
                                            == generation.generation_id
                                        && footprint.report.to.generation == generation.generation
                                        && footprint.report.to.snapshot == generation.snapshot
                                        && generation.base.as_ref()
                                            == Some(&footprint.report.from.generation_id)
                                })
                            })
                })
            {
                let mut evidence = CapabilityEvidence::new(
                    CapabilityEvidenceKind::IntegrationAcceptance,
                    "the COMPLETE plan, PASS target, final source, integration job, and uniquely promoted plan candidate identify one transactional outcome",
                );
                evidence.verifier_job_id = Some(proof.verifier_job_id.clone());
                evidence.generation_id = Some(generation.generation_id.clone());
                evidence.artifact_hash = Some(run.expected.hash.clone());
                integrated.push(evidence);
            }
        }
        capabilities.push(finding(
            ControlPlaneCapability::AtomicallyIntegrateAndAcceptFinalTruth,
            if !integrated.is_empty() {
                CapabilityStatus::Supported
            } else if runtime_missing {
                CapabilityStatus::Unavailable
            } else {
                CapabilityStatus::NotDemonstrated
            },
            integrated,
            "the plan lacks a uniquely bound COMPLETE/PASS/final-source/candidate acceptance outcome",
        ));

        Ok(ControlPlaneCapabilityReport {
            version: "1",
            repository_id: info.repository_id,
            workspace_id: info.workspace_id,
            plan_id: id.clone(),
            plan_state: view.state,
            accepted_generation_id: ontology.accepted.map(|generation| generation.generation_id),
            final_source: view.final_source,
            capabilities,
        })
    }

    fn capability_transitions(
        &self,
        info: &RepositoryInfo,
        id: &PlanId,
    ) -> Result<Vec<Transition>> {
        let mut statement = self.connection.prepare(
            "SELECT sequence,task_id,entry_json FROM events WHERE repo_id=?1 AND workspace_id=?2 AND plan_id=?3 AND json_extract(entry_json,'$.kind')='TASK_STATE_CHANGED' ORDER BY sequence",
        )?;
        let rows = statement.query_map(
            params![
                info.repository_id.as_str(),
                info.workspace_id.as_str(),
                id.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        let mut transitions = vec![];
        for row in rows {
            let (sequence, task, json) = row?;
            let JournalEntry::TaskStateChanged {
                to, verification, ..
            } = serde_json::from_str(&json)?
            else {
                continue;
            };
            transitions.push(Transition {
                sequence,
                task: TaskId::new(task).map_err(Error::Invalid)?,
                to,
                proof: verification,
            });
        }
        Ok(transitions)
    }

    fn capability_phases(&self, info: &RepositoryInfo, id: &PlanId) -> Result<Vec<Phase>> {
        let mut statement = self.connection.prepare(
            "SELECT sequence,entry_json FROM events WHERE repo_id=?1 AND workspace_id=?2 AND plan_id=?3 AND json_extract(entry_json,'$.kind')='RUNTIME' ORDER BY sequence",
        )?;
        let rows = statement.query_map(
            params![
                info.repository_id.as_str(),
                info.workspace_id.as_str(),
                id.as_str()
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )?;
        let mut phases = vec![];
        for row in rows {
            let (sequence, json) = row?;
            let JournalEntry::Runtime { job_id, phase, .. } = serde_json::from_str(&json)? else {
                continue;
            };
            phases.push(Phase {
                sequence,
                phase,
                job: job_id,
            });
        }
        Ok(phases)
    }
}
