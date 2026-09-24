use super::*;

/// What a planner actually decides. Everything else in an [`ExecutionPlan`] is
/// derived by agentctl from the frozen planning request, so it is never asked
/// for and never trusted: the packet hashes, the frozen source, the request
/// identity and the creation timestamp are computed here.
///
/// Measured (2026-09-21, Codex `gpt-5.6-sol`): asking a provider to hand-write
/// the whole envelope produced a valid plan followed by one surplus closing
/// brace on every attempt, so the plan was correctly refused and no plan could
/// be produced at all. The same model returns well-formed JSON for this
/// smaller decision. The contract is not weakened by the change — binding a
/// contract to its exact packet stops being something a provider asserts and
/// starts being something agentctl guarantees.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanDecision {
    pub version: ProtocolVersion,
    pub packet: PlanPacket,
    /// One entry per task in `packet`.
    pub task_contracts: Vec<TaskContractDecision>,
    /// Must cover every requested done criterion; plan validation rechecks that.
    pub integration_expectations: Vec<String>,
    #[serde(default)]
    pub replan: Option<planning::ReplanReference>,
}

/// The parts of a verification contract a planner authors. The rest
/// (`task_packet_hash`, `independent_verifier`, `input`) is fixed or derived.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskContractDecision {
    pub task_id: TaskId,
    /// Existing memory entries this task's verifier must weigh. Only ids that
    /// already begin with `memory:` are entries; live project policy appears in
    /// the planner packet with a `config:` id and is never referenceable.
    pub memory_refs: Vec<memory::MemoryId>,
    /// Paths *inside* this task's declared scope that it must neither read nor
    /// write. They carve a hole in the scope, so an exclusion may never overlap
    /// a path the task itself declares in `read_scope` or `write_scope`; a path
    /// outside the declared scope is already forbidden and does not belong
    /// here. Use `[]` when the declared scope is already exact.
    pub exclusions: Vec<ScopePath>,
    pub non_goals: Vec<String>,
}

/// Formatting aid only: a filled-in decision of the shape the planner must
/// return. No store access, import, activation or semantic repair.
pub(super) fn template(prepared: &planning::PlannerPacket, identity: &str) -> Result<PlanDecision> {
    let intent = &prepared.request.intent;
    let task = TaskPacket {
        version: ProtocolVersion::V1,
        task_id: TaskId::new(format!("task:{identity}:1")).map_err(Error::Invalid)?,
        objective: intent.objective.clone(),
        read_scope: intent.scope.clone(),
        write_scope: intent.scope.clone(),
        graph_entities: vec![],
        invariant_refs: intent.invariant_refs.clone(),
        dependencies: vec![],
        definition_of_done: intent.definition_of_done.clone(),
        verification: intent
            .verification
            .clone()
            .ok_or_else(|| Error::Invalid("prepared verification requirements missing".into()))?,
    };
    Ok(PlanDecision {
        version: ProtocolVersion::V1,
        task_contracts: vec![TaskContractDecision {
            task_id: task.task_id.clone(),
            memory_refs: vec![],
            exclusions: vec![],
            non_goals: intent.constraints.clone(),
        }],
        integration_expectations: intent.definition_of_done.clone(),
        replan: None,
        packet: PlanPacket {
            version: ProtocolVersion::V1,
            plan_id: PlanId::new(format!("plan:{identity}")).map_err(Error::Invalid)?,
            objective: intent.objective.clone(),
            tasks: vec![task],
            integration_verification: intent.verification.clone().expect("checked requirements"),
        },
    })
}

/// Builds the canonical execution-plan envelope around a planner decision. Every
/// derived field comes from the frozen request or from the packet itself, so a
/// provider cannot state a hash, a source observation or an identity at all.
pub(super) fn envelope(
    prepared: &planning::PlannerPacket,
    decision: PlanDecision,
) -> Result<planning::ExecutionPlan> {
    use planning::*;
    let PlanDecision {
        version: _,
        packet,
        task_contracts,
        integration_expectations,
        replan,
    } = decision;
    let mut seen = BTreeSet::new();
    let contracts = task_contracts
        .into_iter()
        .map(|contract| {
            require(
                seen.insert(contract.task_id.clone()),
                "[PLAN-OUTPUT] duplicate task_contracts entry in planner decision",
            )?;
            let task = packet
                .tasks
                .iter()
                .find(|task| task.task_id == contract.task_id)
                .ok_or_else(|| {
                    Error::Invalid(format!(
                        "[PLAN-OUTPUT] task_contracts names {}, which is not a packet task",
                        contract.task_id.as_str()
                    ))
                })?;
            Ok(VerificationContract {
                task_id: contract.task_id,
                task_packet_hash: hash(task)?,
                independent_verifier: true,
                input: VerifierInput::PacketDiffAndEvidence,
                memory_refs: contract.memory_refs,
                exclusions: contract.exclusions,
                non_goals: contract.non_goals,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ExecutionPlan {
        metadata: PlanMetadata {
            version: ProtocolVersion::V1,
            request_id: prepared.request.request_id.clone(),
            source: prepared.request.source.clone(),
            created_at_ms: now_ms()?,
            provenance: PlanningProvenance {
                actor: "runtime-planner".into(),
                source_refs: vec![prepared.request.request_id.as_str().into()],
                provider: None,
            },
            integration: IntegrationVerificationContract {
                plan_id: packet.plan_id.clone(),
                plan_packet_hash: hash(&packet)?,
                independent_verifier: true,
                require_all_task_verifications: true,
                require_final_diff_and_evidence: true,
                expectations: integration_expectations,
            },
            contracts,
            replan,
        },
        packet,
    })
}
