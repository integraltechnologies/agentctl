use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{
    Error, Result, migrations, paths,
    repository::{DirectoryIdentity, RepositoryId, RepositoryInfo, WorkspaceId},
    require,
};
use crate::{Validate, protocol::*};

pub const DATABASE_VERSION: i64 = migrations::SCHEMA_VERSION;
pub const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredRepository {
    pub repository_id: RepositoryId,
    pub common_directory: PathBuf,
    pub common_directory_identity: Option<DirectoryIdentity>,
    pub remotes: BTreeMap<String, Vec<String>>,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
}

impl RegisteredRepository {
    pub(crate) fn validate(&self) -> Result<()> {
        paths::absolute_path(&self.common_directory)?;
        require(
            self.repository_id == RepositoryId::for_common_directory(&self.common_directory),
            "repository ID does not match its common directory",
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredWorkspace {
    pub info: RepositoryInfo,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub previous_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredTask {
    pub plan_id: PlanId,
    pub packet: TaskPacket,
    pub state: TaskState,
}

/// Local journal. This is not an extension of the accepted AgentEvent wire enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum JournalEntry {
    MemoryCreated {
        memory_id: super::memory::MemoryId,
        trust: MemoryTrustClass,
        actor: String,
    },
    MemoryPromoted {
        original: super::memory::MemoryId,
        canonical: super::memory::MemoryId,
        actor: String,
    },
    MemorySuperseded {
        original: super::memory::MemoryId,
        replacement: super::memory::MemoryId,
        actor: String,
    },
    MemoryRejected {
        memory_id: super::memory::MemoryId,
        actor: String,
    },
    IndexCompleted {
        stats: super::graph::IndexStats,
    },
    RepositoryObserved {
        /// Frozen v1 observation, retained verbatim across the identity migration.
        repository: serde_json::Value,
    },
    WorkspaceObserved {
        workspace: RegisteredWorkspace,
    },
    PlanCreated {
        plan_id: PlanId,
    },
    TaskCreated {
        task_id: TaskId,
    },
    TaskStateChanged {
        from: TaskState,
        to: TaskState,
        verification: Option<VerificationPacket>,
    },
    JobCreated {
        job_id: JobId,
    },
    JobStateChanged {
        from: JobState,
        to: JobState,
    },
    EvidenceRecorded {
        evidence_id: EvidenceId,
    },
    Agent {
        event: Box<AgentEvent>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredEvent {
    pub sequence: i64,
    pub repository_id: RepositoryId,
    pub workspace_id: Option<WorkspaceId>,
    pub legacy_repository_id: Option<RepositoryId>,
    pub timestamp_ms: u64,
    pub plan_id: Option<PlanId>,
    pub task_id: Option<TaskId>,
    pub job_id: Option<JobId>,
    pub entry: JournalEntry,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateStatus {
    pub schema_version: i64,
    pub journal_mode: String,
    pub repositories: u64,
    pub workspaces: u64,
    pub plans: u64,
    pub tasks: u64,
    pub jobs: u64,
    pub evidence: u64,
    pub events: u64,
}

pub struct Store {
    pub(super) connection: Connection,
}

impl Store {
    /// Opens/initializes a private local database. Normal opens apply known migrations.
    pub fn open(path: &Path, busy_timeout_ms: u64) -> Result<Self> {
        paths::absolute_path(path)?;
        paths::ensure_directory(
            path.parent()
                .ok_or_else(|| Error::Invalid("database needs a parent".into()))?,
        )?;
        check_database_paths(path, true)?;
        paths::create_database_file(path)?;
        let mut connection = connection(path, busy_timeout_ms, false)?;
        migrations::migrate(&mut connection)?;
        let mode: String = connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        require(
            mode.eq_ignore_ascii_case("wal"),
            "SQLite could not enable WAL; use a local filesystem that supports it",
        )?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        Ok(Self { connection })
    }

    /// Inspection never initializes, migrates, or changes journal mode.
    pub fn read_only(path: &Path, busy_timeout_ms: u64) -> Result<Self> {
        paths::absolute_path(path)?;
        paths::check_directory(
            path.parent()
                .ok_or_else(|| Error::Invalid("database needs a parent".into()))?,
        )?;
        check_database_paths(path, false)?;
        let connection = connection(path, busy_timeout_ms, true)?;
        migrations::check(&connection)?;
        Ok(Self { connection })
    }

    pub fn status(&self) -> Result<StateStatus> {
        migrations::check(&self.connection)?;
        let quick: String = self
            .connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        require(
            quick == "ok",
            format!("SQLite integrity check failed: {quick}"),
        )?;
        let mut statement = self.connection.prepare("PRAGMA foreign_key_check")?;
        require(
            statement.query([])?.next()?.is_none(),
            "SQLite foreign-key integrity check failed",
        )?;
        let count = |table: &str| -> Result<u64> {
            Ok(self
                .connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?)
        };
        Ok(StateStatus {
            schema_version: migrations::version(&self.connection)?,
            journal_mode: self
                .connection
                .pragma_query_value(None, "journal_mode", |row| row.get(0))?,
            repositories: count("repositories")?,
            workspaces: count("workspaces")?,
            plans: count("plans")?,
            tasks: count("tasks")?,
            jobs: count("jobs")?,
            evidence: count("evidence")?,
            events: count("events")?,
        })
    }

    /// Registers the discovered workspace and its shared logical repository atomically.
    pub fn register_repository(&mut self, info: RepositoryInfo) -> Result<RegisteredWorkspace> {
        info.validate()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let logical = repository(&tx, &info.repository_id)?;
        if let Some(previous) = &logical {
            require(
                previous.common_directory == info.common_directory
                    && previous
                        .common_directory_identity
                        .as_ref()
                        .is_none_or(|id| Some(id) == info.common_directory_identity.as_ref()),
                "registered Git common directory was replaced; refusing to merge unrelated state",
            )?;
        }
        let logical = RegisteredRepository {
            repository_id: info.repository_id.clone(),
            common_directory: info.common_directory.clone(),
            common_directory_identity: info.common_directory_identity.clone(),
            remotes: info.remotes.clone(),
            first_seen_ms: logical
                .as_ref()
                .map_or(info.source.observed_at_ms, |r| r.first_seen_ms),
            last_seen_ms: logical.as_ref().map_or(info.source.observed_at_ms, |r| {
                r.last_seen_ms.max(info.source.observed_at_ms)
            }),
        };
        let existing = workspace(&tx, &info.workspace_id)?;
        let owner: Option<String> = tx
            .query_row(
                "SELECT workspace_id FROM workspaces WHERE root=?1",
                [utf8(&info.root)?],
                |row| row.get(0),
            )
            .optional()?;
        require(
            owner
                .as_deref()
                .is_none_or(|id| id == info.workspace_id.as_str()),
            "root is registered to a different Git identity; inspect the existing registration before reusing this path",
        )?;
        let mut record = if let Some(mut previous) = existing {
            require(
                previous.info.git_directory == info.git_directory
                    && previous.info.repository_id == info.repository_id,
                "repository identity collision: Git metadata directories differ",
            )?;
            require(
                previous.info.git_directory_identity == info.git_directory_identity,
                "registered Git metadata directory was replaced; refusing to merge unrelated state at the same path",
            )?;
            if previous.info.root != info.root {
                require(
                    !previous.info.root.try_exists()?,
                    format!(
                        "repository moved but old root {} still exists; refusing ambiguous reassociation",
                        previous.info.root.display()
                    ),
                )?;
                previous.previous_roots.push(previous.info.root.clone());
            }
            previous.last_seen_ms = previous.last_seen_ms.max(info.source.observed_at_ms);
            previous.info = info;
            previous
        } else {
            RegisteredWorkspace {
                first_seen_ms: info.source.observed_at_ms,
                last_seen_ms: info.source.observed_at_ms,
                info,
                previous_roots: vec![],
            }
        };
        record.previous_roots.sort();
        record.previous_roots.dedup();
        tx.execute("INSERT INTO repositories(repo_id,common_directory,record_json) VALUES (?1,?2,?3) ON CONFLICT(repo_id) DO UPDATE SET record_json=excluded.record_json",
            params![logical.repository_id.as_str(), utf8(&logical.common_directory)?, encode(&logical, MAX_DOCUMENT_BYTES)?])?;
        tx.execute("INSERT INTO workspaces(workspace_id,repo_id,root,git_directory,record_json) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(workspace_id) DO UPDATE SET root=excluded.root,record_json=excluded.record_json",
            params![record.info.workspace_id.as_str(), record.info.repository_id.as_str(), utf8(&record.info.root)?, utf8(&record.info.git_directory)?, encode(&record, MAX_DOCUMENT_BYTES)?])?;
        append(
            &tx,
            &record.info.repository_id,
            record.last_seen_ms,
            &Links {
                workspace_id: Some(record.info.workspace_id.clone()),
                ..Links::default()
            },
            None,
            &JournalEntry::WorkspaceObserved {
                workspace: record.clone(),
            },
        )?;
        tx.commit()?;
        Ok(record)
    }

    pub fn repository(&self, id: &RepositoryId) -> Result<Option<RegisteredRepository>> {
        repository(&self.connection, id)
    }

    pub fn repositories(&self) -> Result<Vec<RegisteredRepository>> {
        let rows = strings(
            &self.connection,
            "SELECT record_json FROM repositories ORDER BY common_directory,repo_id",
            [],
        )?;
        rows.into_iter()
            .map(|json| {
                let record: RegisteredRepository = serde_json::from_str(&json)?;
                record.validate()?;
                Ok(record)
            })
            .collect()
    }

    pub fn workspace(&self, id: &WorkspaceId) -> Result<Option<RegisteredWorkspace>> {
        workspace(&self.connection, id)
    }

    pub fn workspaces(&self, repo: Option<&RepositoryId>) -> Result<Vec<RegisteredWorkspace>> {
        strings(&self.connection, "SELECT record_json FROM workspaces WHERE (?1 IS NULL OR repo_id=?1) ORDER BY root,workspace_id", [repo.map(RepositoryId::as_str)])?
            .into_iter().map(|json| {
                let record: RegisteredWorkspace = serde_json::from_str(&json)?;
                record.info.validate()?;
                Ok(record)
            }).collect()
    }

    /// Inserts an immutable plan and all its tasks atomically, initially PLANNED.
    /// A single-task plan is the creation API for an individual TaskPacket.
    pub fn create_plan(
        &mut self,
        repo: &RepositoryId,
        plan: &PlanPacket,
        timestamp_ms: u64,
    ) -> Result<()> {
        plan.validate()?;
        let json = encode(plan, MAX_DOCUMENT_BYTES)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO plans(repo_id,plan_id,packet_json) VALUES (?1,?2,?3)",
            params![repo.as_str(), plan.plan_id.as_str(), json],
        )?;
        let mut links = Links {
            plan_id: Some(plan.plan_id.clone()),
            ..Links::default()
        };
        append(
            &tx,
            repo,
            timestamp_ms,
            &links,
            None,
            &JournalEntry::PlanCreated {
                plan_id: plan.plan_id.clone(),
            },
        )?;
        for task in &plan.tasks {
            tx.execute(
                "INSERT INTO tasks(repo_id,task_id,plan_id,state_json) VALUES (?1,?2,?3,?4)",
                params![
                    repo.as_str(),
                    task.task_id.as_str(),
                    plan.plan_id.as_str(),
                    serde_json::to_string(&TaskState::Planned)?
                ],
            )?;
            links.task_id = Some(task.task_id.clone());
            append(
                &tx,
                repo,
                timestamp_ms,
                &links,
                None,
                &JournalEntry::TaskCreated {
                    task_id: task.task_id.clone(),
                },
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn plan(&self, repo: &RepositoryId, id: &PlanId) -> Result<Option<PlanPacket>> {
        plan(&self.connection, repo, id)
    }

    pub fn task(&self, repo: &RepositoryId, id: &TaskId) -> Result<Option<StoredTask>> {
        task(&self.connection, repo, id)
    }

    pub fn tasks(&self, repo: &RepositoryId, plan_id: Option<&PlanId>) -> Result<Vec<StoredTask>> {
        let ids = strings(
            &self.connection,
            "SELECT task_id FROM tasks WHERE repo_id=?1 AND (?2 IS NULL OR plan_id=?2) ORDER BY plan_id,task_id",
            params![repo.as_str(), plan_id.map(PlanId::as_str)],
        )?;
        ids.into_iter()
            .map(|id| {
                task(
                    &self.connection,
                    repo,
                    &TaskId::new(id).map_err(Error::Invalid)?,
                )?
                .ok_or_else(|| Error::Invalid("task disappeared during query".into()))
            })
            .collect()
    }

    pub fn transition_task(
        &mut self,
        repo: &RepositoryId,
        id: &TaskId,
        expected: TaskState,
        next: TaskState,
        verification: Option<&VerificationPacket>,
        timestamp_ms: u64,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current =
            task(&tx, repo, id)?.ok_or_else(|| Error::Invalid("task is not registered".into()))?;
        require(
            current.state == expected,
            format!(
                "task state conflict: expected {expected:?}, found {:?}",
                current.state
            ),
        )?;
        let plan = plan(&tx, repo, &current.plan_id)?
            .ok_or_else(|| Error::Invalid("task plan is missing".into()))?;
        let states = task_states(&tx, repo, &current.plan_id)?;
        plan.validate_task_transition(id, &states, next, verification)?;
        if let Some(proof) = verification {
            validate_verifier(&tx, repo, &current, proof)?;
        }
        tx.execute(
            "UPDATE tasks SET state_json=?1 WHERE repo_id=?2 AND task_id=?3",
            params![serde_json::to_string(&next)?, repo.as_str(), id.as_str()],
        )?;
        let links = Links {
            workspace_id: verification
                .map(|v| {
                    associated_workspace(&tx, "jobs", "job_id", repo, v.verifier_job_id.as_str())
                })
                .transpose()?
                .flatten(),
            plan_id: Some(current.plan_id),
            task_id: Some(id.clone()),
            job_id: verification.map(|v| v.verifier_job_id.clone()),
        };
        append(
            &tx,
            repo,
            timestamp_ms,
            &links,
            None,
            &JournalEntry::TaskStateChanged {
                from: expected,
                to: next,
                verification: verification.cloned(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn register_job(&mut self, repo: &RepositoryId, job: &AgentJob) -> Result<()> {
        self.register_job_at(repo, None, job)
    }

    pub fn register_job_in_workspace(
        &mut self,
        repo: &RepositoryId,
        workspace: &WorkspaceId,
        job: &AgentJob,
    ) -> Result<()> {
        self.register_job_at(repo, Some(workspace), job)
    }

    fn register_job_at(
        &mut self,
        repo: &RepositoryId,
        workspace: Option<&WorkspaceId>,
        job: &AgentJob,
    ) -> Result<()> {
        job.validate()?;
        require(
            job.state == JobState::Queued,
            "new jobs must begin QUEUED; use valid transitions after registration",
        )?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(task_id) = &job.task_id {
            let task = task(&tx, repo, task_id)?
                .ok_or_else(|| Error::Invalid("job task is not registered".into()))?;
            require(
                task.plan_id == job.plan_id,
                "job task belongs to a different plan",
            )?;
        }
        validate_workspace(&tx, repo, workspace)?;
        tx.execute(
            "INSERT INTO jobs(repo_id,job_id,plan_id,task_id,packet_json,workspace_id) VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                repo.as_str(),
                job.job_id.as_str(),
                job.plan_id.as_str(),
                job.task_id.as_ref().map(TaskId::as_str),
                encode(job, MAX_METADATA_BYTES)?, workspace.map(WorkspaceId::as_str)
            ],
        )?;
        append(
            &tx,
            repo,
            job.created_at_ms,
            &Links {
                workspace_id: workspace.cloned(),
                ..Links::for_job(job)
            },
            None,
            &JournalEntry::JobCreated {
                job_id: job.job_id.clone(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn job(&self, repo: &RepositoryId, id: &JobId) -> Result<Option<AgentJob>> {
        job(&self.connection, repo, id)
    }

    pub fn job_workspace(&self, repo: &RepositoryId, id: &JobId) -> Result<Option<WorkspaceId>> {
        associated_workspace(&self.connection, "jobs", "job_id", repo, id.as_str())
    }

    pub fn jobs(&self, repo: &RepositoryId) -> Result<Vec<AgentJob>> {
        strings(
            &self.connection,
            "SELECT packet_json FROM jobs WHERE repo_id=?1 ORDER BY job_id",
            [repo.as_str()],
        )?
        .into_iter()
        .map(|json| decode_validated(&json))
        .collect()
    }

    pub fn transition_job(
        &mut self,
        repo: &RepositoryId,
        id: &JobId,
        expected: JobState,
        next: JobState,
        timestamp_ms: u64,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut job =
            job(&tx, repo, id)?.ok_or_else(|| Error::Invalid("job is not registered".into()))?;
        require(
            job.state == expected,
            format!(
                "job state conflict: expected {expected:?}, found {:?}",
                job.state
            ),
        )?;
        expected.validate_transition(next)?;
        require(
            timestamp_ms >= job.started_at_ms.unwrap_or(job.created_at_ms),
            "job transition timestamp precedes creation/start",
        )?;
        if next == JobState::Running && job.started_at_ms.is_none() {
            job.started_at_ms = Some(timestamp_ms);
        }
        if next.is_terminal() {
            job.finished_at_ms = Some(timestamp_ms);
        }
        job.state = next;
        job.validate()?;
        tx.execute(
            "UPDATE jobs SET packet_json=?1 WHERE repo_id=?2 AND job_id=?3",
            params![
                encode(&job, MAX_METADATA_BYTES)?,
                repo.as_str(),
                id.as_str()
            ],
        )?;
        append(
            &tx,
            repo,
            timestamp_ms,
            &Links {
                workspace_id: associated_workspace(&tx, "jobs", "job_id", repo, id.as_str())?,
                ..Links::for_job(&job)
            },
            None,
            &JournalEntry::JobStateChanged {
                from: expected,
                to: next,
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_evidence(
        &mut self,
        repo: &RepositoryId,
        evidence: &EvidenceRecord,
    ) -> Result<()> {
        self.record_evidence_at(repo, None, evidence)
    }

    pub fn record_evidence_in_workspace(
        &mut self,
        repo: &RepositoryId,
        workspace: &WorkspaceId,
        evidence: &EvidenceRecord,
    ) -> Result<()> {
        self.record_evidence_at(repo, Some(workspace), evidence)
    }

    fn record_evidence_at(
        &mut self,
        repo: &RepositoryId,
        workspace: Option<&WorkspaceId>,
        evidence: &EvidenceRecord,
    ) -> Result<()> {
        evidence.validate()?;
        let json = encode(evidence, MAX_METADATA_BYTES)?;
        if let Some(reference) = &evidence.full_log_ref {
            require(
                !reference
                    .get(..5)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
                    && !reference.contains(['\n', '\r'])
                    && reference.len() <= 4096,
                "full_log_ref must be a compact locator, not embedded log data",
            )?;
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_workspace(&tx, repo, workspace)?;
        tx.execute(
            "INSERT INTO evidence(repo_id,evidence_id,record_json,workspace_id) VALUES (?1,?2,?3,?4)",
            params![repo.as_str(), evidence.evidence_id.as_str(), json, workspace.map(WorkspaceId::as_str)],
        )?;
        append(
            &tx,
            repo,
            evidence.finished_at_ms.unwrap_or(evidence.started_at_ms),
            &Links {
                workspace_id: workspace.cloned(),
                ..Links::default()
            },
            None,
            &JournalEntry::EvidenceRecorded {
                evidence_id: evidence.evidence_id.clone(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn evidence(&self, repo: &RepositoryId, id: &EvidenceId) -> Result<Option<EvidenceRecord>> {
        let json: Option<String> = self
            .connection
            .query_row(
                "SELECT record_json FROM evidence WHERE repo_id=?1 AND evidence_id=?2",
                params![repo.as_str(), id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|json| decode_validated(&json)).transpose()
    }

    pub fn evidence_workspace(
        &self,
        repo: &RepositoryId,
        id: &EvidenceId,
    ) -> Result<Option<WorkspaceId>> {
        associated_workspace(
            &self.connection,
            "evidence",
            "evidence_id",
            repo,
            id.as_str(),
        )
    }

    pub fn append_agent_event(&mut self, repo: &RepositoryId, event: &AgentEvent) -> Result<i64> {
        self.append_agent_event_at(repo, None, event)
    }

    pub fn append_agent_event_in_workspace(
        &mut self,
        repo: &RepositoryId,
        workspace: &WorkspaceId,
        event: &AgentEvent,
    ) -> Result<i64> {
        self.append_agent_event_at(repo, Some(workspace), event)
    }

    fn append_agent_event_at(
        &mut self,
        repo: &RepositoryId,
        workspace: Option<&WorkspaceId>,
        event: &AgentEvent,
    ) -> Result<i64> {
        event.validate()?;
        encode(event, MAX_METADATA_BYTES)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut links = event_links(&tx, repo, event)?;
        if let Some(workspace) = workspace {
            validate_workspace(&tx, repo, Some(workspace))?;
            require(
                links.workspace_id.as_ref().is_none_or(|id| id == workspace),
                "event workspace conflicts with its registered job location",
            )?;
            links.workspace_id = Some(workspace.clone());
        }
        let sequence = append(
            &tx,
            repo,
            event.timestamp_ms,
            &links,
            Some(&event.event_id),
            &JournalEntry::Agent {
                event: Box::new(event.clone()),
            },
        )?;
        tx.commit()?;
        Ok(sequence)
    }

    /// Returns the most recent matching records, in ascending durable sequence order.
    pub fn events(
        &self,
        repo: Option<&RepositoryId>,
        task: Option<&TaskId>,
        job: Option<&JobId>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        require(
            (1..=1000).contains(&limit),
            "event limit must be between 1 and 1000",
        )?;
        let mut statement = self.connection.prepare("SELECT sequence,repo_id,timestamp_ms,plan_id,task_id,job_id,entry_json,workspace_id,legacy_repository_id FROM events WHERE (?1 IS NULL OR repo_id=?1) AND (?2 IS NULL OR task_id=?2) AND (?3 IS NULL OR job_id=?3) ORDER BY sequence DESC LIMIT ?4")?;
        let rows = statement.query_map(
            params![
                repo.map(RepositoryId::as_str),
                task.map(TaskId::as_str),
                job.map(JobId::as_str),
                limit as i64
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            },
        )?;
        let mut events = vec![];
        for row in rows {
            let (sequence, repo, timestamp_ms, plan, task, job, entry, workspace, legacy_repo) =
                row?;
            let entry: JournalEntry = serde_json::from_str(&entry)?;
            if let JournalEntry::Agent { event } = &entry {
                event.validate()?;
            }
            events.push(StoredEvent {
                sequence,
                repository_id: RepositoryId::try_from(repo).map_err(Error::Invalid)?,
                workspace_id: workspace
                    .map(WorkspaceId::try_from)
                    .transpose()
                    .map_err(Error::Invalid)?,
                legacy_repository_id: legacy_repo
                    .map(RepositoryId::try_from)
                    .transpose()
                    .map_err(Error::Invalid)?,
                timestamp_ms,
                plan_id: plan.map(PlanId::new).transpose().map_err(Error::Invalid)?,
                task_id: task.map(TaskId::new).transpose().map_err(Error::Invalid)?,
                job_id: job.map(JobId::new).transpose().map_err(Error::Invalid)?,
                entry,
            });
        }
        events.reverse();
        Ok(events)
    }
}

#[derive(Default)]
pub(super) struct Links {
    workspace_id: Option<WorkspaceId>,
    plan_id: Option<PlanId>,
    task_id: Option<TaskId>,
    job_id: Option<JobId>,
}

impl Links {
    pub(super) fn workspace(id: WorkspaceId) -> Self {
        Self {
            workspace_id: Some(id),
            ..Self::default()
        }
    }
    fn for_job(job: &AgentJob) -> Self {
        Self {
            workspace_id: None,
            plan_id: Some(job.plan_id.clone()),
            task_id: job.task_id.clone(),
            job_id: Some(job.job_id.clone()),
        }
    }
}

fn connection(path: &Path, timeout: u64, read_only: bool) -> Result<Connection> {
    require(
        (1..=60_000).contains(&timeout),
        "SQLite busy timeout must be between 1 and 60000 ms",
    )?;
    let flags = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    // Resolve administrator/user-selected parent aliases (e.g. macOS /var), while
    // keeping SQLite's NOFOLLOW protection on the owned database leaf.
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("database needs a parent".into()))?;
    let canonical = std::fs::canonicalize(parent)?.join(
        path.file_name()
            .ok_or_else(|| Error::Invalid("database needs a filename".into()))?,
    );
    let connection = Connection::open_with_flags(
        canonical,
        flags | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_millis(timeout))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "trusted_schema", false)?;
    Ok(connection)
}

fn check_database_paths(path: &Path, allow_missing: bool) -> Result<()> {
    paths::check_file(path, allow_missing)?;
    check_single_link(path)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        paths::check_file(Path::new(&sidecar), true)?;
        check_single_link(Path::new(&sidecar))?;
    }
    Ok(())
}

fn check_single_link(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match std::fs::symlink_metadata(path) {
            Ok(meta) => require(
                meta.nlink() == 1,
                format!("{}: refusing multiply-linked database file", path.display()),
            )?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn encode(value: &impl Serialize, max: usize) -> Result<String> {
    let json = serde_json::to_string(value)?;
    require(
        json.len() <= max,
        format!("metadata exceeds {max} bytes; keep logs/artifacts in the filesystem"),
    )?;
    Ok(json)
}

fn decode_validated<T: DeserializeOwned + Validate>(json: &str) -> Result<T> {
    let value: T = serde_json::from_str(json)?;
    value.validate()?;
    Ok(value)
}

fn utf8(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::Invalid("Stage 1 requires UTF-8 repository paths".into()))
}

fn strings(
    connection: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<String>> {
    Ok(connection
        .prepare(sql)?
        .query_map(params, |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?)
}

fn repository(connection: &Connection, id: &RepositoryId) -> Result<Option<RegisteredRepository>> {
    let json: Option<String> = connection
        .query_row(
            "SELECT record_json FROM repositories WHERE repo_id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    json.map(|json| {
        let record: RegisteredRepository = serde_json::from_str(&json)?;
        record.validate()?;
        require(
            &record.repository_id == id,
            "stored repository ID differs from its key",
        )?;
        Ok(record)
    })
    .transpose()
}

fn plan(connection: &Connection, repo: &RepositoryId, id: &PlanId) -> Result<Option<PlanPacket>> {
    let json: Option<String> = connection
        .query_row(
            "SELECT packet_json FROM plans WHERE repo_id=?1 AND plan_id=?2",
            params![repo.as_str(), id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    json.map(|json| {
        let plan: PlanPacket = decode_validated(&json)?;
        require(&plan.plan_id == id, "stored plan ID differs from its key")?;
        Ok(plan)
    })
    .transpose()
}

fn workspace(connection: &Connection, id: &WorkspaceId) -> Result<Option<RegisteredWorkspace>> {
    let json: Option<String> = connection
        .query_row(
            "SELECT record_json FROM workspaces WHERE workspace_id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    json.map(|json| {
        let record: RegisteredWorkspace = serde_json::from_str(&json)?;
        record.info.validate()?;
        require(
            &record.info.workspace_id == id,
            "stored workspace ID differs from its key",
        )?;
        Ok(record)
    })
    .transpose()
}

fn validate_workspace(
    connection: &Connection,
    repo: &RepositoryId,
    id: Option<&WorkspaceId>,
) -> Result<()> {
    if let Some(id) = id {
        let record = workspace(connection, id)?
            .ok_or_else(|| Error::Invalid("workspace is not registered".into()))?;
        require(
            &record.info.repository_id == repo,
            "workspace belongs to a different repository",
        )?;
    }
    Ok(())
}

fn associated_workspace(
    connection: &Connection,
    table: &str,
    key: &str,
    repo: &RepositoryId,
    id: &str,
) -> Result<Option<WorkspaceId>> {
    let workspace: Option<Option<String>> = connection
        .query_row(
            &format!("SELECT workspace_id FROM {table} WHERE repo_id=?1 AND {key}=?2"),
            params![repo.as_str(), id],
            |row| row.get(0),
        )
        .optional()?;
    let workspace = workspace
        .flatten()
        .map(WorkspaceId::try_from)
        .transpose()
        .map_err(Error::Invalid)?;
    validate_workspace(connection, repo, workspace.as_ref())?;
    Ok(workspace)
}

fn task(connection: &Connection, repo: &RepositoryId, id: &TaskId) -> Result<Option<StoredTask>> {
    let row: Option<(String, String)> = connection
        .query_row(
            "SELECT plan_id,state_json FROM tasks WHERE repo_id=?1 AND task_id=?2",
            params![repo.as_str(), id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(plan_id, state)| {
        let plan_id = PlanId::new(plan_id).map_err(Error::Invalid)?;
        let plan = plan(connection, repo, &plan_id)?
            .ok_or_else(|| Error::Invalid("stored task plan is missing".into()))?;
        let packet = plan
            .tasks
            .into_iter()
            .find(|t| &t.task_id == id)
            .ok_or_else(|| {
                Error::Invalid("stored task is absent from its immutable plan".into())
            })?;
        Ok(StoredTask {
            plan_id,
            packet,
            state: serde_json::from_str(&state)?,
        })
    })
    .transpose()
}

fn task_states(
    connection: &Connection,
    repo: &RepositoryId,
    plan: &PlanId,
) -> Result<BTreeMap<TaskId, TaskState>> {
    let mut stmt = connection.prepare(
        "SELECT task_id,state_json FROM tasks WHERE repo_id=?1 AND plan_id=?2 ORDER BY task_id",
    )?;
    let rows = stmt.query_map(params![repo.as_str(), plan.as_str()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (id, state) = row?;
        Ok((
            TaskId::new(id).map_err(Error::Invalid)?,
            serde_json::from_str(&state)?,
        ))
    })
    .collect()
}

fn job(connection: &Connection, repo: &RepositoryId, id: &JobId) -> Result<Option<AgentJob>> {
    let json: Option<String> = connection
        .query_row(
            "SELECT packet_json FROM jobs WHERE repo_id=?1 AND job_id=?2",
            params![repo.as_str(), id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    json.map(|json| {
        let job: AgentJob = decode_validated(&json)?;
        require(&job.job_id == id, "stored job ID differs from its key")?;
        Ok(job)
    })
    .transpose()
}

pub(super) fn append(
    connection: &Connection,
    repo: &RepositoryId,
    timestamp: u64,
    links: &Links,
    external_id: Option<&str>,
    entry: &JournalEntry,
) -> Result<i64> {
    let timestamp = i64::try_from(timestamp)
        .map_err(|_| Error::Invalid("event timestamp exceeds SQLite integer range".into()))?;
    validate_workspace(connection, repo, links.workspace_id.as_ref())?;
    connection.execute("INSERT INTO events(repo_id,external_event_id,timestamp_ms,plan_id,task_id,job_id,entry_json,workspace_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)", params![repo.as_str(), external_id, timestamp, links.plan_id.as_ref().map(PlanId::as_str), links.task_id.as_ref().map(TaskId::as_str), links.job_id.as_ref().map(JobId::as_str), encode(entry, MAX_DOCUMENT_BYTES)?, links.workspace_id.as_ref().map(WorkspaceId::as_str)])?;
    Ok(connection.last_insert_rowid())
}

fn validate_evidence(
    connection: &Connection,
    repo: &RepositoryId,
    evidence: &[EvidenceRef],
) -> Result<()> {
    for reference in evidence {
        let json: Option<String> = connection
            .query_row(
                "SELECT record_json FROM evidence WHERE repo_id=?1 AND evidence_id=?2",
                params![repo.as_str(), reference.0.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let json = json.ok_or_else(|| {
            Error::Invalid(format!(
                "evidence {} is not registered in this repository",
                reference.0.as_str()
            ))
        })?;
        let record: EvidenceRecord = decode_validated(&json)?;
        require(
            record.evidence_id == reference.0,
            "evidence ID differs from its storage key",
        )?;
    }
    Ok(())
}

fn validate_verifier(
    connection: &Connection,
    repo: &RepositoryId,
    task: &StoredTask,
    proof: &VerificationPacket,
) -> Result<()> {
    let verifier = job(connection, repo, &proof.verifier_job_id)?.ok_or_else(|| {
        Error::Invalid("verifier job is not registered in this repository".into())
    })?;
    require(
        verifier.role == AgentRole::Verifier
            && verifier.plan_id == task.plan_id
            && verifier.task_id.as_ref() == Some(&task.packet.task_id),
        "verification must reference a registered verifier job for this task and plan",
    )?;
    require(
        verifier.state == JobState::Succeeded,
        "verification decision requires a successfully finished verifier job",
    )?;
    let VerificationTarget::Packet {
        executor_job_id, ..
    } = &proof.target
    else {
        return Err(Error::Invalid("expected packet verification".into()));
    };
    let executor = job(connection, repo, executor_job_id)?.ok_or_else(|| {
        Error::Invalid("executor job is not registered in this repository".into())
    })?;
    require(
        executor.role == AgentRole::Executor
            && executor.plan_id == task.plan_id
            && executor.task_id.as_ref() == Some(&task.packet.task_id),
        "verification executor does not belong to this task and plan",
    )?;
    require(
        executor.state == JobState::Succeeded,
        "verification requires a successfully finished executor job",
    )?;
    validate_evidence(connection, repo, &proof.evidence)
}

fn merge_plan(links: &mut Links, plan: &PlanId) -> Result<()> {
    require(
        links.plan_id.as_ref().is_none_or(|p| p == plan),
        "event references conflicting plans",
    )?;
    links.plan_id = Some(plan.clone());
    Ok(())
}

fn merge_task(links: &mut Links, id: &TaskId) -> Result<()> {
    require(
        links.task_id.as_ref().is_none_or(|t| t == id),
        "event references conflicting tasks",
    )?;
    links.task_id = Some(id.clone());
    Ok(())
}

fn event_links(connection: &Connection, repo: &RepositoryId, event: &AgentEvent) -> Result<Links> {
    let context = &event.context;
    let mut links = Links {
        workspace_id: None,
        plan_id: context.plan_id.clone(),
        task_id: context
            .task_id
            .clone()
            .or_else(|| context.packet_id.clone()),
        job_id: context.job_id.clone(),
    };
    match &event.event {
        AgentEventKind::TaskPacketLoaded { task_id } => merge_task(&mut links, task_id)?,
        AgentEventKind::VerificationStarted { target } => match target {
            VerificationTarget::Packet {
                task_id,
                executor_job_id,
            } => {
                merge_task(&mut links, task_id)?;
                let executor = job(connection, repo, executor_job_id)?.ok_or_else(|| {
                    Error::Invalid("verification references an unknown executor job".into())
                })?;
                require(
                    executor.role == AgentRole::Executor
                        && executor.task_id.as_ref() == Some(task_id),
                    "verification executor does not match its task",
                )?;
                merge_plan(&mut links, &executor.plan_id)?;
            }
            VerificationTarget::Integration {
                plan_id,
                executor_job_ids,
            } => {
                merge_plan(&mut links, plan_id)?;
                for id in executor_job_ids {
                    let executor = job(connection, repo, id)?.ok_or_else(|| {
                        Error::Invalid("integration event references an unknown executor".into())
                    })?;
                    require(
                        executor.role == AgentRole::Executor && &executor.plan_id == plan_id,
                        "integration executor does not belong to this plan",
                    )?;
                }
            }
        },
        AgentEventKind::WaitingOnDependency { task_ids } => {
            for id in task_ids {
                let task = task(connection, repo, id)?
                    .ok_or_else(|| Error::Invalid("event dependency is not registered".into()))?;
                merge_plan(&mut links, &task.plan_id)?;
            }
        }
        AgentEventKind::ToolFinished { evidence, .. }
        | AgentEventKind::CommandFinished { evidence, .. } => {
            validate_evidence(connection, repo, evidence)?
        }
        AgentEventKind::ExperimentBoundary { boundary } => {
            validate_evidence(connection, repo, &boundary.evidence)?
        }
        _ => {}
    }
    if let Some(id) = links.job_id.clone() {
        links.workspace_id = associated_workspace(connection, "jobs", "job_id", repo, id.as_str())?;
        let job = job(connection, repo, &id)?.ok_or_else(|| {
            Error::Invalid("event job is not registered in this repository".into())
        })?;
        merge_plan(&mut links, &job.plan_id)?;
        if let Some(id) = &job.task_id {
            merge_task(&mut links, id)?;
        }
        require(
            context.role.is_none_or(|r| r == job.role),
            "event role does not match registered job",
        )?;
        require(
            context
                .agent_id
                .as_ref()
                .is_none_or(|id| id == &job.agent_id),
            "event agent does not match registered job",
        )?;
        if let AgentEventKind::VerificationStarted { target } = &event.event {
            require(
                job.role == AgentRole::Verifier,
                "verification event requires a verifier job",
            )?;
            let independent = match target {
                VerificationTarget::Packet {
                    executor_job_id, ..
                } => executor_job_id != &job.job_id,
                VerificationTarget::Integration {
                    executor_job_ids, ..
                } => !executor_job_ids.contains(&job.job_id),
            };
            require(independent, "verification job must be independent")?;
        }
    }
    if let Some(id) = links.task_id.clone() {
        let task = task(connection, repo, &id)?.ok_or_else(|| {
            Error::Invalid("event task is not registered in this repository".into())
        })?;
        merge_plan(&mut links, &task.plan_id)?;
    }
    Ok(links)
}
