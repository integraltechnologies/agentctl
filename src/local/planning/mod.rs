//! Deterministic plan control, not planner intelligence or execution.
pub(crate) mod cli;
pub(super) mod completion;
mod context;
pub(super) mod legacy_completion;
mod model;
mod validation;
use super::{
    Error, Result,
    config::ProjectConfig,
    graph, memory, now_ms,
    repository::RepositoryInfo,
    require,
    store::{self, JournalEntry, Links, Store},
};
use crate::{Validate, protocol::*};
pub use model::*;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use validation::*;

pub fn hash(value: &impl serde::Serialize) -> Result<String> {
    Ok(graph::content_hash(&serde_json::to_vec(value)?))
}
pub fn size(value: &impl serde::Serialize) -> Result<usize> {
    Ok(serde_json::to_vec(value)?.len())
}

impl Store {
    pub fn import_execution_plan(
        &mut self,
        start: &Path,
        plan: &ExecutionPlan,
    ) -> Result<ExecutionPlanView> {
        let info = graph::checked_workspace(self, start)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate(&tx, &info, plan)?;
        store::insert_plan(&tx, &info.repository_id, &plan.packet, now_ms()?)?;
        tx.execute("INSERT INTO execution_plans(repo_id,plan_id,request_id,workspace_id,metadata_json,state,updated_at_ms) VALUES (?1,?2,?3,?4,?5,'VALIDATED',?6)",params![info.repository_id.as_str(),plan.packet.plan_id.as_str(),plan.metadata.request_id.as_str(),info.workspace_id.as_str(),serde_json::to_string(&plan.metadata)?,now_ms()?])?;
        audit(
            &tx,
            &info,
            Some(&plan.packet.plan_id),
            &JournalEntry::ExecutionPlanImported {
                plan_id: plan.packet.plan_id.clone(),
            },
        )?;
        audit(
            &tx,
            &info,
            Some(&plan.packet.plan_id),
            &JournalEntry::ExecutionPlanValidated {
                plan_id: plan.packet.plan_id.clone(),
            },
        )?;
        let result = load(&tx, &info, &plan.packet.plan_id)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn execution_plan(&self, start: &Path, id: &PlanId) -> Result<ExecutionPlanView> {
        let info = graph::checked_workspace(self, start)?;
        let tx = self.connection.unchecked_transaction()?;
        load(&tx, &info, id)
    }
    /// Revalidate the frozen contract, source and live references; never rebuild context.
    pub fn validate_execution_plan(
        &mut self,
        start: &Path,
        id: &PlanId,
    ) -> Result<ExecutionPlanView> {
        let info = graph::checked_workspace(self, start)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let view = load(&tx, &info, id)?;
        require(
            view.state == PlanState::Validated,
            "only pending VALIDATED plans can be revalidated; inspect active/historical plans without rewriting them",
        )?;
        validate(&tx, &info, &view.plan)?;
        audit(
            &tx,
            &info,
            Some(id),
            &JournalEntry::ExecutionPlanValidated {
                plan_id: id.clone(),
            },
        )?;
        tx.commit()?;
        Ok(view)
    }
    pub fn activate_execution_plan(&mut self, start: &Path, id: &PlanId) -> Result<()> {
        let info = graph::checked_workspace(self, start)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let view = load(&tx, &info, id)?;
        require(
            view.state == PlanState::Validated,
            "activation requires VALIDATED plan",
        )?;
        validate(&tx, &info, &view.plan)?;
        tx.execute("UPDATE execution_plans SET state='ACTIVE',updated_at_ms=?3 WHERE repo_id=?1 AND plan_id=?2",params![info.repository_id.as_str(),id.as_str(),now_ms()?])?;
        audit(
            &tx,
            &info,
            Some(id),
            &JournalEntry::ExecutionPlanActivated {
                plan_id: id.clone(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn cancel_execution_plan(&mut self, start: &Path, id: &PlanId, reason: &str) -> Result<()> {
        text(reason, 1024, "cancellation reason")?;
        let info = graph::checked_workspace(self, start)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let view = load(&tx, &info, id)?;
        mutable(&view)?;
        quiescent(&tx, &info, id)?;
        tx.execute("UPDATE execution_plans SET state='CANCELLED',updated_at_ms=?3 WHERE repo_id=?1 AND plan_id=?2",params![info.repository_id.as_str(),id.as_str(),now_ms()?])?;
        audit(
            &tx,
            &info,
            Some(id),
            &JournalEntry::ExecutionPlanCancelled {
                plan_id: id.clone(),
                reason: reason.into(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn supersede_execution_plan(
        &mut self,
        start: &Path,
        old: &PlanId,
        new: &PlanId,
    ) -> Result<()> {
        require(old != new, "plan cannot supersede itself")?;
        let info = graph::checked_workspace(self, start)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let original = load(&tx, &info, old)?;
        let replacement = load(&tx, &info, new)?;
        mutable(&original)?;
        quiescent(&tx, &info, old)?;
        require(
            replacement.state == PlanState::Validated,
            "replacement must be VALIDATED",
        )?;
        let replan = replacement.plan.metadata.replan.as_ref().ok_or_else(|| {
            Error::Invalid("replacement must explicitly reference prior plan and reason".into())
        })?;
        require(
            &replan.previous_plan_id == old,
            "replacement names another prior plan",
        )?;
        validate(&tx, &info, &replacement.plan)?;
        tx.execute("UPDATE execution_plans SET state='SUPERSEDED',superseded_by=?3,updated_at_ms=?4 WHERE repo_id=?1 AND plan_id=?2",params![info.repository_id.as_str(),old.as_str(),new.as_str(),now_ms()?])?;
        audit(
            &tx,
            &info,
            Some(old),
            &JournalEntry::ExecutionPlanSuperseded {
                original: old.clone(),
                replacement: new.clone(),
                reason: replan.reason.clone(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn execution_plans(
        &self,
        start: &Path,
        all: bool,
        limit: usize,
    ) -> Result<Vec<PlanSummary>> {
        require((1..=100).contains(&limit), "plan list limit must be 1–100")?;
        let info = graph::checked_workspace(self, start)?;
        self.connection.prepare("SELECT e.plan_id,e.state,e.workspace_id,json_extract(p.packet_json,'$.objective') FROM execution_plans e JOIN plans p USING(repo_id,plan_id) WHERE e.repo_id=?1 AND (?2 OR e.workspace_id=?3) AND (?2 OR e.state IN ('VALIDATED','ACTIVE')) ORDER BY e.updated_at_ms DESC,e.plan_id LIMIT ?4")?
            .query_map(params![info.repository_id.as_str(),all,info.workspace_id.as_str(),limit],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?
            .map(|row|{let(id,state,ws,objective)=row?;Ok(PlanSummary{plan_id:PlanId::new(id).map_err(Error::Invalid)?,state:state_decode(&state)?,workspace_id:ws.try_into().map_err(Error::Invalid)?,objective})}).collect()
    }
    pub fn execution_tasks(&self, start: &Path, id: &PlanId) -> Result<Vec<TaskInspection>> {
        let info = graph::checked_workspace(self, start)?;
        let tx = self.connection.unchecked_transaction()?;
        let view = load(&tx, &info, id)?;
        let states = store::task_states(&tx, &info.repository_id, id)?;
        let prepared = request(&tx, &info, &view.plan.metadata.request_id)?;
        let mut tasks = vec![];
        for t in &view.plan.packet.tasks {
            let state = *states
                .get(&t.task_id)
                .ok_or_else(|| Error::Invalid("task state missing".into()))?;
            let mut reasons = vec![];
            if !matches!(view.state, PlanState::Validated | PlanState::Active) {
                reasons.push(format!("plan is {:?}", view.state));
            }
            if !matches!(state, TaskState::Planned | TaskState::Ready) {
                reasons.push(format!(
                    "task is {state:?}; only PLANNED/READY tasks are candidates"
                ));
            }
            for dep in &t.dependencies {
                if states.get(dep) != Some(&TaskState::Verified) {
                    reasons.push(format!(
                        "{} is {:?}, not VERIFIED",
                        dep.as_str(),
                        states.get(dep)
                    ));
                }
            }
            let contract = view
                .plan
                .metadata
                .contracts
                .iter()
                .find(|c| c.task_id == t.task_id)
                .ok_or_else(|| Error::Invalid("verification contract missing".into()))?
                .clone();
            tasks.push(TaskInspection {
                planning_request_id: view.plan.metadata.request_id.clone(),
                source: view.plan.metadata.source.clone(),
                constraints: prepared.request.intent.constraints.clone(),
                invariants: prepared
                    .context
                    .invariants
                    .iter()
                    .filter(|(key, _)| t.invariant_refs.contains(key))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                packet: t.clone(),
                packet_bytes: size(t)?,
                contract_bytes: size(&contract)?,
                contract,
                state,
                structurally_ready: reasons.is_empty()
                    && match state {
                        TaskState::Planned => view
                            .plan
                            .packet
                            .validate_task_transition(&t.task_id, &states, TaskState::Ready, None)
                            .is_ok(),
                        TaskState::Ready => {
                            view.plan.packet.task_is_runnable(&t.task_id, &states)?
                        }
                        _ => false,
                    },
                reasons,
            });
        }
        tasks.sort_by(|a, b| a.packet.task_id.cmp(&b.packet.task_id));
        Ok(tasks)
    }
    /// Accept externally recorded integration proof; does not run any check or job.
    pub fn complete_execution_plan(
        &mut self,
        start: &Path,
        id: &PlanId,
        proof: &VerificationPacket,
        source: &SourceStateRef,
    ) -> Result<()> {
        source.validate()?;
        let info = graph::checked_workspace(self, start)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let view = load(&tx, &info, id)?;
        require(
            view.state == PlanState::Active,
            "completion requires ACTIVE plan",
        )?;
        let states = store::task_states(&tx, &info.repository_id, id)?;
        view.plan.packet.validate_completion(&states, proof)?;
        integration_proof(&tx, &info, &view, proof, source)?;
        let integration_json = serde_json::to_string(proof)?;
        let final_source_json = serde_json::to_string(source)?;
        audit(
            &tx,
            &info,
            Some(id),
            &JournalEntry::ExecutionPlanCompleted {
                plan_id: id.clone(),
                verification: proof.clone(),
                source: source.clone(),
            },
        )?;
        let permit = completion::authorize(
            &tx,
            [
                info.repository_id.as_str().to_owned(),
                id.as_str().to_owned(),
                info.workspace_id.as_str().to_owned(),
                integration_json.clone(),
                final_source_json.clone(),
            ],
        )?;
        tx.execute("UPDATE execution_plans SET state='COMPLETE',updated_at_ms=?3,integration_json=?4,final_source_json=?5 WHERE repo_id=?1 AND plan_id=?2",params![info.repository_id.as_str(),id.as_str(),now_ms()?,integration_json,final_source_json])?;
        drop(permit);
        tx.commit()?;
        Ok(())
    }
}

fn request(c: &Connection, info: &RepositoryInfo, id: &PlanningRequestId) -> Result<PlannerPacket> {
    let json:Option<String>=c.query_row("SELECT packet_json FROM planning_requests WHERE request_id=?1 AND repo_id=?2 AND workspace_id=?3",params![id.as_str(),info.repository_id.as_str(),info.workspace_id.as_str()],|r|r.get(0)).optional()?;
    serde_json::from_str(
        &json
            .ok_or_else(|| Error::Invalid("planning request not found in this workspace".into()))?,
    )
    .map_err(Error::from)
}
fn load(c: &Connection, info: &RepositoryInfo, id: &PlanId) -> Result<ExecutionPlanView> {
    type PlanRow = (
        String,
        String,
        u64,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row:Option<PlanRow>=c.query_row("SELECT metadata_json,state,updated_at_ms,superseded_by,integration_json,final_source_json FROM execution_plans WHERE repo_id=?1 AND workspace_id=?2 AND plan_id=?3",params![info.repository_id.as_str(),info.workspace_id.as_str(),id.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
    let (metadata, state, updated_at_ms, replacement, proof, source) =
        row.ok_or_else(|| Error::Invalid("execution plan not found in this workspace".into()))?;
    let packet = store::plan(c, &info.repository_id, id)?
        .ok_or_else(|| Error::Invalid("plan packet missing".into()))?;
    Ok(ExecutionPlanView {
        plan: ExecutionPlan {
            metadata: serde_json::from_str(&metadata)?,
            packet,
        },
        state: state_decode(&state)?,
        updated_at_ms,
        superseded_by: replacement
            .map(PlanId::new)
            .transpose()
            .map_err(Error::Invalid)?,
        integration_proof: proof.map(|p| serde_json::from_str(&p)).transpose()?,
        final_source: source.map(|p| serde_json::from_str(&p)).transpose()?,
    })
}
fn state_decode(s: &str) -> Result<PlanState> {
    Ok(serde_json::from_value(serde_json::Value::String(s.into()))?)
}
fn mutable(v: &ExecutionPlanView) -> Result<()> {
    require(
        matches!(v.state, PlanState::Validated | PlanState::Active),
        "plan is terminal; history cannot be rewritten",
    )
}
fn quiescent(c: &Connection, info: &RepositoryInfo, id: &PlanId) -> Result<()> {
    let n:i64=c.query_row("SELECT count(*) FROM jobs WHERE repo_id=?1 AND plan_id=?2 AND json_extract(packet_json,'$.state') IN ('QUEUED','RUNNING','WAITING')",params![info.repository_id.as_str(),id.as_str()],|r|r.get(0))?;
    require(
        n == 0,
        "plan has unfinished jobs; settle them explicitly before cancellation/supersession",
    )
}
fn audit(
    c: &Connection,
    info: &RepositoryInfo,
    plan: Option<&PlanId>,
    entry: &JournalEntry,
) -> Result<()> {
    store::append(
        c,
        &info.repository_id,
        now_ms()?,
        &Links::planning(info.workspace_id.clone(), plan.cloned()),
        None,
        entry,
    )?;
    Ok(())
}
