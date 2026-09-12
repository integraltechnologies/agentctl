use super::*;

/// Formatting aid only: no store access, import, activation or semantic repair.
/// Providers can compute the Stage 4 serialization hashes without guessing them.
pub(crate) fn packet_hashes() -> Result<()> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(256 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    require(bytes.len() <= 256 * 1024, "PlanPacket exceeds input bound")?;
    let packet: PlanPacket = serde_json::from_slice(&bytes)?;
    packet.validate()?;
    let tasks: BTreeMap<_, _> = packet
        .tasks
        .iter()
        .map(|t| Ok((t.task_id.clone(), planning::hash(t)?)))
        .collect::<Result<_>>()?;
    println!(
        "{}",
        serde_json::json!({"plan_packet_hash":planning::hash(&packet)?,"task_packet_hashes":tasks})
    );
    Ok(())
}

pub(super) fn template(
    prepared: &planning::PlannerPacket,
    identity: &str,
) -> Result<planning::ExecutionPlan> {
    use planning::*;
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
    let packet = PlanPacket {
        version: ProtocolVersion::V1,
        plan_id: PlanId::new(format!("plan:{identity}")).map_err(Error::Invalid)?,
        objective: intent.objective.clone(),
        tasks: vec![task],
        integration_verification: intent.verification.clone().expect("checked requirements"),
    };
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
            contracts: packet
                .tasks
                .iter()
                .map(|task| {
                    Ok(VerificationContract {
                        task_id: task.task_id.clone(),
                        task_packet_hash: hash(task)?,
                        independent_verifier: true,
                        input: VerifierInput::PacketDiffAndEvidence,
                        memory_refs: vec![],
                        exclusions: vec![],
                        non_goals: vec![],
                    })
                })
                .collect::<Result<_>>()?,
            integration: IntegrationVerificationContract {
                plan_id: packet.plan_id.clone(),
                plan_packet_hash: hash(&packet)?,
                independent_verifier: true,
                require_all_task_verifications: true,
                require_final_diff_and_evidence: true,
                expectations: intent.definition_of_done.clone(),
            },
            replan: None,
        },
        packet,
    })
}
