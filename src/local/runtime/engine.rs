use super::*;
use process::{ProcessSpec, WorkspaceLease};
use provider::{JobInput, ProviderAdapter};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// Adapters are trusted installed code. Provider *outputs* never receive a Store,
/// SQL capability, or a public lifecycle submission endpoint.
pub struct Runtime<'a> {
    store: &'a mut Store,
    paths: paths::MachinePaths,
    config: RuntimeConfig,
    adapters: BTreeMap<String, Box<dyn ProviderAdapter>>,
    artifacts: Artifacts,
    checks: Box<dyn process::CheckLauncher>,
}
impl<'a> Runtime<'a> {
    pub fn new(
        store: &'a mut Store,
        paths: paths::MachinePaths,
        config: RuntimeConfig,
        adapters: BTreeMap<String, Box<dyn ProviderAdapter>>,
    ) -> Result<Self> {
        config.validate()?;
        let artifacts = Artifacts::new(&paths.data_root.join("runtime/blobs"))?;
        Ok(Self {
            store,
            paths,
            config,
            adapters,
            artifacts,
            checks: Box::new(process::NativeChecks),
        })
    }
    pub fn with_check_launcher(mut self, launcher: Box<dyn process::CheckLauncher>) -> Self {
        self.checks = launcher;
        self
    }
    fn lease(&self, info: &RepositoryInfo) -> Result<WorkspaceLease> {
        require(
            !self.paths.data_root.starts_with(&info.root)
                && !self.paths.config_root.starts_with(&info.root),
            "runtime machine state must be outside the workspace",
        )?;
        WorkspaceLease::acquire(
            &self
                .paths
                .data_root
                .join("runtime/locks")
                .join(info.workspace_id.as_str()),
        )
    }
    fn cancelled(&self, info: &RepositoryInfo, plan: Option<&PlanId>) -> Result<bool> {
        Ok(if let Some(id) = plan {
            self.store
                .connection
                .query_row(
                    "SELECT cancel_requested FROM runtime_runs WHERE repo_id=?1 AND plan_id=?2",
                    params![info.repository_id.as_str(), id.as_str()],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(false)
        } else {
            false
        })
    }
    fn process_spec(
        &self,
        info: &RepositoryInfo,
        id: &str,
        role: AgentRole,
        lease: &WorkspaceLease,
    ) -> Result<ProcessSpec> {
        let scratch = self
            .paths
            .data_root
            .join("runtime/scratch")
            .join(id.replace(':', "-"));
        paths::ensure_directory(&scratch)?;
        Ok(ProcessSpec {
            native_auth: None,
            api_key: None,
            executable: PathBuf::new(),
            args: vec![],
            input: vec![],
            cwd: info.root.clone(),
            workspace: info.root.clone(),
            scratch: std::fs::canonicalize(scratch)?,
            data_root: std::fs::canonicalize(&self.paths.data_root)?,
            config_root: std::fs::canonicalize(&self.paths.config_root)?,
            writable: role == AgentRole::Executor,
            network: true,
            timeout_ms: self.config.timeout_ms,
            git_directories: vec![info.git_directory.clone(), info.common_directory.clone()],
            protected: ProjectConfig::load(&info.root)?.protected,
            credential_env: vec![],
            lock_fd: lease.fd(),
        })
    }
    fn identity(&self) -> Result<(JobId, String)> {
        let hex: String =
            self.store
                .connection
                .query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))?;
        let session = format!(
            "{}-{}-4{}-a{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[13..16],
            &hex[17..20],
            &hex[20..]
        );
        Ok((
            JobId::new(format!("runtime:{hex}")).map_err(Error::Invalid)?,
            session,
        ))
    }
    #[allow(clippy::too_many_arguments)] // One issued job boundary, shared by all roles.
    fn invoke(
        &mut self,
        info: &RepositoryInfo,
        plan: Option<&PlanId>,
        request: Option<&planning::PlanningRequestId>,
        task: Option<&TaskId>,
        role: AgentRole,
        source: &SourceSnapshot,
        artifact: Value,
        lease: &WorkspaceLease,
    ) -> Result<(RuntimeJob, Value)> {
        let config = self.config.role(role)?.clone();
        let (job_id, session_id) = self.identity()?;
        let ownership = session::issued(self.store, info, plan, request, task, role, &job_id)?;
        let input = JobInput {
            ownership: ownership.clone(),
            job_id: job_id.clone(),
            session_id: session_id.clone(),
            role,
            plan_id: plan.cloned(),
            task_id: task.cloned(),
            repository_id: info.repository_id.clone(),
            workspace_id: info.workspace_id.clone(),
            source: source.source_ref()?,
            artifact,
        };
        require(
            serde_json::to_vec(&input)?.len() <= 256 * 1024,
            "bounded runtime input exceeds 256 KiB",
        )?;
        let mut job = RuntimeJob {
            ownership: Some(ownership),
            task_id: task.cloned(),
            job_id: job_id.clone(),
            session_id,
            role,
            plan_id: plan.cloned(),
            request_id: request.cloned(),
            workspace_id: info.workspace_id.clone(),
            config: config.clone(),
            state: RuntimeJobState::Queued,
            input: self.artifacts.json(&input)?,
            output: None,
            stdout: None,
            stderr: None,
            pid: None,
            created_at_ms: now_ms()?,
            started_at_ms: None,
            finished_at_ms: None,
            failure: None,
        };
        save_job(self.store, info, &job, "JOB_CREATED")?;
        if let Some(plan) = plan {
            let canonical = AgentJob {
                version: ProtocolVersion::V1,
                job_id: job_id.clone(),
                agent_id: AgentId::new(format!("agent:{}", job_id.as_str()))
                    .map_err(Error::Invalid)?,
                role,
                plan_id: plan.clone(),
                task_id: task.cloned(),
                state: JobState::Queued,
                provider: Some(ProviderMetadata {
                    provider: config.provider.clone(),
                    model: config.model.clone(),
                }),
                created_at_ms: job.created_at_ms,
                started_at_ms: None,
                finished_at_ms: None,
            };
            self.store.register_job_in_workspace(
                &info.repository_id,
                &info.workspace_id,
                &canonical,
            )?;
            self.store.transition_job(
                &info.repository_id,
                &job_id,
                JobState::Queued,
                JobState::Running,
                now_ms()?,
            )?;
        }
        let result: Result<Value> = (|| {
            let spec = self.process_spec(info, job_id.as_str(), role, lease)?;
            let _scratch = process::ScratchCleanup(spec.scratch.clone());
            let adapter = self.adapters.get_mut(&config.provider).ok_or_else(|| {
                Error::Invalid("configured provider adapter is unavailable".into())
            })?;
            require(
                adapter.capabilities().fresh_session,
                "provider cannot guarantee a fresh session",
            )?;
            let mut process = adapter.launch(&input, spec, &config)?;
            job.pid = process.pid();
            job.started_at_ms = Some(now_ms()?);
            job.state = RuntimeJobState::Running;
            save_job(self.store, info, &job, "JOB_STARTED")?;
            let started = Instant::now();
            let mut interruption = None;
            let output = loop {
                if self.cancelled(info, plan)? {
                    interruption = Some("cancelled".to_string());
                    process.cancel()?;
                }
                if started.elapsed().as_millis() > self.config.timeout_ms as u128 {
                    interruption = Some("timeout".into());
                    process.cancel()?;
                }
                if let Some(output) = process.poll()? {
                    break output;
                }
                std::thread::sleep(Duration::from_millis(25));
            };
            job.stdout = Some(self.artifacts.put(&output.stdout)?);
            job.stderr = Some(self.artifacts.put(&output.stderr)?);
            event(
                self.store,
                info,
                plan,
                Some(&job_id),
                "JOB_OUTPUT_RECEIVED",
                "provider output captured; not yet accepted",
            )?;
            if plan.is_some() {
                let usage = self.adapters[&config.provider]
                    .usage(&output)
                    .unwrap_or_default();
                let timestamp_ms = now_ms()?;
                let context = EventContext {
                    agent_id: Some(
                        AgentId::new(format!("agent:{}", job_id.as_str()))
                            .map_err(Error::Invalid)?,
                    ),
                    plan_id: plan.cloned(),
                    task_id: task.cloned(),
                    packet_id: task.cloned(),
                    job_id: Some(job_id.clone()),
                    role: Some(role),
                    provider: Some(ProviderMetadata {
                        provider: config.provider.clone(),
                        model: config.model.clone(),
                    }),
                };
                let usage = TokenUsageEvent {
                    version: ProtocolVersion::V1,
                    timestamp_ms,
                    context: context.clone(),
                    provenance: usage.provenance,
                    input_tokens: usage.input,
                    output_tokens: usage.output,
                    cached_tokens: usage.cached,
                    reasoning_tokens: None,
                    total_tokens: None,
                };
                self.store.append_agent_event_in_workspace(
                    &info.repository_id,
                    &info.workspace_id,
                    &AgentEvent {
                        version: ProtocolVersion::V1,
                        event_id: format!("usage:{}", job_id.as_str()),
                        timestamp_ms,
                        context,
                        event: AgentEventKind::TokenUsageObserved { usage },
                    },
                )?;
            }
            require(
                interruption.is_none() && output.failure.is_none() && output.exit == Some(0),
                interruption
                    .or(output.failure.clone())
                    .unwrap_or_else(|| format!("provider exited {:?}", output.exit)),
            )?;
            let value = self
                .adapters
                .get(&config.provider)
                .expect("selected adapter")
                .collect(&output)?;
            match role {
                AgentRole::Executor => {
                    let result: ResultPacket = serde_json::from_value(value.clone())?;
                    result.validate()?;
                    require(
                        Some(&result.task_id) == task
                            && result.executor_job_id == job_id
                            && result.status == ResultStatus::Succeeded
                            && result.evidence.is_empty(),
                        "executor result has incorrect issued identity/status or invented evidence",
                    )?;
                }
                AgentRole::Verifier => {
                    let proof: VerificationPacket = serde_json::from_value(value.clone())?;
                    proof.validate()?;
                    require(
                        proof.verifier_job_id == job_id
                            && serde_json::to_value(&proof.target)? == input.artifact["target"]
                            && serde_json::to_value(&proof.evidence)? == input.artifact["evidence"],
                        "verifier result does not match issued job/target/captured evidence",
                    )?;
                }
                AgentRole::Planner => {
                    let output: planning::ExecutionPlan = serde_json::from_value(value.clone())?;
                    output.packet.validate()?;
                }
            }
            job.output = Some(self.artifacts.json(&value)?);
            Ok(value)
        })();
        job.finished_at_ms = Some(now_ms()?);
        let cancelled = self.cancelled(info, plan)?;
        job.state = if cancelled {
            RuntimeJobState::Cancelled
        } else if result.is_ok() {
            RuntimeJobState::Succeeded
        } else {
            RuntimeJobState::Failed
        };
        job.failure = result
            .as_ref()
            .err()
            .map(|e| e.to_string().chars().take(1024).collect());
        if plan.is_some() {
            self.store.transition_job(
                &info.repository_id,
                &job_id,
                JobState::Running,
                if cancelled {
                    JobState::Cancelled
                } else if result.is_ok() {
                    JobState::Succeeded
                } else {
                    JobState::Failed
                },
                now_ms()?,
            )?;
        }
        save_job(
            self.store,
            info,
            &job,
            if cancelled {
                "JOB_CANCELLED"
            } else if result.is_ok() {
                "JOB_SUCCEEDED"
            } else {
                "JOB_FAILED"
            },
        )?;
        Ok((job, result?))
    }
    fn expected(&self, root: &Path, reference: &ArtifactRef) -> Result<SourceSnapshot> {
        let expected: SourceSnapshot = self.artifacts.decode(reference)?;
        let current = source::capture(root, &self.artifacts)?;
        require(
            expected == current,
            "SOURCE_DRIFT: current workspace differs from the captured result; explicit replan required",
        )?;
        Ok(current)
    }
    fn completed_verifier(
        &self,
        info: &RepositoryInfo,
        plan: &PlanId,
        task: Option<&TaskId>,
        source: &SourceSnapshot,
    ) -> Result<Option<(RuntimeJob, VerificationPacket, JobInput)>> {
        for job in self.store.runtime_jobs(&info.root, Some(plan))? {
            session::validate_job(self.store, info, &job)?;
            if job.role != AgentRole::Verifier || job.state != RuntimeJobState::Succeeded {
                continue;
            }
            let input: JobInput = self.artifacts.decode(&job.input)?;
            require(
                Some(&input.ownership) == job.ownership.as_ref(),
                "stored verifier input/session ownership mismatch",
            )?;
            if input.task_id.as_ref() != task || input.source != source.source_ref()? {
                continue;
            }
            let proof =
                self.artifacts.decode(job.output.as_ref().ok_or_else(|| {
                    Error::Invalid("successful verifier output missing".into())
                })?)?;
            return Ok(Some((job, proof, input)));
        }
        Ok(None)
    }
    fn task_input(
        &self,
        info: &RepositoryInfo,
        task: &planning::TaskInspection,
        source: &SourceSnapshot,
    ) -> Result<Value> {
        let query = task
            .packet
            .objective
            .split_whitespace()
            .take(20)
            .collect::<Vec<_>>()
            .join(" ");
        let mut graph = self.store.graph(&info.root)?.context(
            &query,
            graph::ContextLimits {
                primary: 3,
                depth: 1,
                neighbors: 6,
                tests: 3,
            },
        )?;
        let allowed = |path: &str| task.packet.read_scope.iter().any(|s| permits(s, path));
        graph.primary.retain(|e| allowed(&e.entity.provenance.path));
        graph.neighbors.retain(|e| allowed(&e.provenance.path));
        graph.tests.retain(|e| allowed(&e.provenance.path));
        graph.relations.clear();
        let memory = self.store.memory_for_task(
            &info.root,
            &task.packet,
            memory::MemoryLimits {
                canonical: 3,
                facts: 3,
                notes: 0,
                bytes: 4096,
            },
        )?;
        let mut files = vec![];
        for (path, file) in source.files.iter().filter(|(p, _)| allowed(p)).take(16) {
            let bytes = self.artifacts.get(&file.content)?;
            let text = std::str::from_utf8(&bytes).map_err(|_| {
                Error::Invalid("task context contains binary source; narrow its read scope".into())
            })?;
            files.push(json!({"path":path,"hash":file.content.hash,"text":text.chars().take(4096).collect::<String>(),"truncated":text.len()>4096}));
        }
        Ok(
            json!({"task":task.packet,"contract":task.contract,"invariants":task.invariants,"constraints":task.constraints,"graph":graph,"memory":memory,"files":files,"result_schema":schemars::schema_for!(ResultPacket),"instruction":"Return ResultPacket with this invocation's job_id and task_id. evidence must be []; agentctl captures evidence independently."}),
        )
    }
    fn diff_input(&self, reference: &ArtifactRef) -> Result<Value> {
        let diff: CapturedDiff = self.artifacts.decode(reference)?;
        let mut changes = vec![];
        for c in &diff.changes {
            let text = |file: &Option<source::FileState>| -> Result<Option<String>> {
                file.as_ref()
                    .map(|f| {
                        String::from_utf8(self.artifacts.get(&f.content)?).map_err(|_| {
                            Error::Invalid(
                                "binary diff cannot be verified by this bounded text runtime"
                                    .into(),
                            )
                        })
                    })
                    .transpose()
            };
            changes.push(json!({"path":c.path,"before":text(&c.before)?,"after":text(&c.after)?,"before_state":c.before,"after_state":c.after}));
        }
        let value = json!({"binding":diff,"artifact":reference,"changes":changes});
        require(
            serde_json::to_vec(&value)?.len() <= 128 * 1024,
            "exact diff exceeds 128 KiB verifier context; split/replan task",
        )?;
        Ok(value)
    }
    fn evidence(
        &mut self,
        info: &RepositoryInfo,
        source: &SourceSnapshot,
        summary: &str,
        reference: &ArtifactRef,
    ) -> Result<EvidenceRef> {
        let (id, _) = self.identity()?;
        let evidence_id =
            EvidenceId::new(format!("evidence:{}", id.as_str())).map_err(Error::Invalid)?;
        let now = now_ms()?;
        self.store.record_evidence_in_workspace(
            &info.repository_id,
            &info.workspace_id,
            &EvidenceRecord {
                version: ProtocolVersion::V1,
                evidence_id: evidence_id.clone(),
                command: None,
                source_state: Some(source.source_ref()?),
                started_at_ms: now,
                finished_at_ms: Some(now),
                exit_status: None,
                stdout_hash: Some(reference.hash.clone()),
                stderr_hash: None,
                full_log_ref: Some(self.artifacts.path(reference)?.display().to_string()),
                summary: summary.into(),
            },
        )?;
        Ok(EvidenceRef(evidence_id))
    }
    fn evidence_input(
        &self,
        info: &RepositoryInfo,
        refs: &[EvidenceRef],
    ) -> Result<Vec<EvidenceRecord>> {
        refs.iter()
            .map(|r| {
                self.store
                    .evidence(&info.repository_id, &r.0)?
                    .ok_or_else(|| Error::Invalid("captured evidence missing".into()))
            })
            .collect()
    }
    fn checks(
        &mut self,
        info: &RepositoryInfo,
        plan: &PlanId,
        requirements: &VerificationRequirements,
        source: &SourceSnapshot,
        lease: &WorkspaceLease,
    ) -> Result<Vec<EvidenceRef>> {
        let policy = ProjectConfig::load(&info.root)?;
        let mut refs = vec![];
        let mut commands = BTreeSet::new();
        for name in &requirements.requirement_refs {
            let profile = policy.verification.get(name).ok_or_else(|| {
                Error::Invalid("canonical verification profile disappeared".into())
            })?;
            require(
                !profile.command_refs.is_empty(),
                format!(
                    "verification {name} has no deterministic command; configure project policy"
                ),
            )?;
            commands.extend(profile.command_refs.iter().cloned());
        }
        for name in commands {
            let command = &policy.commands[&name];
            command.validate()?;
            let (id, _) = self.identity()?;
            let mut spec = self.process_spec(info, id.as_str(), AgentRole::Verifier, lease)?;
            let _scratch = process::ScratchCleanup(spec.scratch.clone());
            spec.network = false;
            spec.args = command.args.clone();
            spec.executable = PathBuf::from(&command.program);
            let cwd = if command.cwd == "." {
                info.root.clone()
            } else {
                info.root.join(&command.cwd)
            };
            require(
                std::fs::canonicalize(&cwd)?.starts_with(&info.root),
                "verification cwd escapes workspace",
            )?;
            spec.cwd = cwd;
            event(
                self.store,
                info,
                Some(plan),
                Some(&id),
                "VERIFICATION_CHECK_STARTED",
                &name,
            )?;
            let started_at_ms = now_ms()?;
            let mut process = self.checks.launch(&spec)?;
            let started = Instant::now();
            let mut interrupted = false;
            let output = loop {
                if self.cancelled(info, Some(plan))?
                    || started.elapsed().as_millis() > self.config.timeout_ms as u128
                {
                    interrupted = true;
                    process.cancel()?;
                }
                if let Some(output) = process.poll()? {
                    break output;
                }
                std::thread::sleep(Duration::from_millis(25));
            };
            let stdout = self.artifacts.put(&output.stdout)?;
            let stderr = self.artifacts.put(&output.stderr)?;
            let log = self.artifacts.json(&json!({"stdout":stdout,"stderr":stderr,"command":command,"runner":self.checks.provenance(),"environment":"isolated HOME/TMPDIR; no provider credentials; network denied"}))?;
            let evidence_id =
                EvidenceId::new(format!("evidence:{}", id.as_str())).map_err(Error::Invalid)?;
            self.store.record_evidence_in_workspace(
                &info.repository_id,
                &info.workspace_id,
                &EvidenceRecord {
                    version: ProtocolVersion::V1,
                    evidence_id: evidence_id.clone(),
                    command: Some(command.clone()),
                    source_state: Some(source.source_ref()?),
                    started_at_ms,
                    finished_at_ms: Some(now_ms()?),
                    exit_status: output.exit,
                    stdout_hash: Some(stdout.hash),
                    stderr_hash: Some(stderr.hash),
                    full_log_ref: Some(self.artifacts.path(&log)?.display().to_string()),
                    summary: format!(
                        "canonical check {name}: exit {:?}, failure {:?}",
                        output.exit, output.failure
                    ),
                },
            )?;
            event(
                self.store,
                info,
                Some(plan),
                Some(&id),
                "VERIFICATION_CHECK_FINISHED",
                &name,
            )?;
            require(
                !interrupted && output.exit == Some(0) && output.failure.is_none(),
                format!("deterministic check {name} failed; model prose cannot override it"),
            )?;
            require(
                source::capture(&info.root, &self.artifacts)? == *source,
                "SOURCE_DRIFT: verification command changed workspace",
            )?;
            refs.push(EvidenceRef(evidence_id));
        }
        Ok(refs)
    }
    pub fn plan(
        &mut self,
        root: &Path,
        request: &planning::PlanningRequestId,
    ) -> Result<planning::ExecutionPlanView> {
        let info = graph::checked_workspace(self.store, root)?;
        let lease = self.lease(&info)?;
        let prepared = self.store.planning_context(root, request)?;
        let source = source::capture(root, &self.artifacts)?;
        let _permit = auth::authorize(
            &self.store.connection,
            &info.repository_id,
            request.as_str(),
            &session::for_request(&info, request)?.id,
        )?;
        let (template_id, _) = self.identity()?;
        let template = super::planner::template(&prepared, template_id.as_str())?;
        let (mut job,value) = self.invoke(&info,None,Some(request),None,AgentRole::Planner,&source,json!({"planner_packet":prepared,"output_template":template,"packet_schema":schemars::schema_for!(PlanPacket),"hash_helper":{"executable":std::env::current_exe()?,"argv":["run","packet-hashes"],"stdin":"the exact PlanPacket JSON"},"instruction":"Return an ExecutionPlan envelope shaped like output_template. Decompose tasks as needed with unique IDs and one contract per task; preserve the frozen source and request. Recompute task_packet_hash and plan_packet_hash using the read-only hash_helper (scratch files in TMPDIR are allowed). Hashes are BLAKE3 of typed compact serde serialization, not raw JSON formatting. Do not activate or write source. All output still undergoes Stage 4 validation."}),&lease)?;
        let result = (|| {
            require(
                source::capture(root, &self.artifacts)? == source,
                "SOURCE_DRIFT during planner invocation",
            )?;
            let plan: planning::ExecutionPlan = serde_json::from_value(value)?;
            require(
                plan.metadata.request_id == *request,
                "planner output belongs to another request",
            )?;
            self.store.import_execution_plan(root, &plan)
        })();
        if let Err(error) = &result {
            job.state = RuntimeJobState::Failed;
            job.failure = Some(error.to_string().chars().take(1024).collect());
            save_job(self.store, &info, &job, "PLANNER_OUTPUT_REJECTED")?;
        }
        result // Import publishes VALIDATED; activation remains explicit.
    }
    pub fn run(&mut self, root: &Path, id: &PlanId) -> Result<RunRecord> {
        let info = graph::checked_workspace(self.store, root)?;
        let lease = self.lease(&info)?;
        let _permit = auth::authorize(
            &self.store.connection,
            &info.repository_id,
            id.as_str(),
            &session::for_plan(self.store, &info, id)?.id,
        )?;
        let view = self.store.execution_plan(root, id)?;
        let mut run = if let Some(run) = load_run(self.store, &info, id)? {
            run
        } else {
            require(
                view.state == planning::PlanState::Active,
                "runtime requires an ACTIVE execution plan",
            )?;
            require(
                self.store
                    .tasks(&info.repository_id, Some(id))?
                    .iter()
                    .all(|t| matches!(t.state, TaskState::Planned | TaskState::Ready)),
                "runtime cannot adopt externally executed/verified tasks",
            )?;
            require(
                self.store
                    .jobs(&info.repository_id)?
                    .iter()
                    .all(|j| j.plan_id != *id),
                "runtime cannot adopt pre-registered external jobs",
            )?;
            let current = source::capture(root, &self.artifacts)?;
            let baseline = self.artifacts.json(&current)?;
            let mut round = 0;
            if let Some(replan) = &view.plan.metadata.replan {
                if let Some(previous) = load_run(self.store, &info, &replan.previous_plan_id)? {
                    round = previous.correction_round + 1;
                }
            }
            let mut run = RunRecord {
                engineering_session: Some(session::for_plan(self.store, &info, id)?),
                plan_id: id.clone(),
                workspace_id: info.workspace_id.clone(),
                state: RunState::Running,
                baseline: baseline.clone(),
                expected: baseline,
                policy_hash: view.plan.metadata.source.policy_hash.clone(),
                accepted: BTreeMap::new(),
                pending: None,
                reason: None,
                correction_round: round,
            };
            let adoption = (|| {
                require(
                    !view.plan.metadata.source.observation.dirty
                        && !info.source.dirty
                        && info.source.head_commit
                            == view.plan.metadata.source.observation.head_commit,
                    "SOURCE_DRIFT: Stage 5 adoption requires the plan's clean committed baseline; reprepare/replan dirty work",
                )?;
                require(
                    self.store.index_status(root)?.fresh,
                    "SOURCE_DRIFT: graph/source assumptions changed; replan",
                )?;
                for file in &view.plan.metadata.source.support {
                    require(
                        current
                            .files
                            .get(&file.path)
                            .is_some_and(|f| f.content.hash == file.content_hash),
                        "SOURCE_DRIFT: planning support changed",
                    )?;
                }
                Ok::<(), Error>(())
            })();
            if let Err(error) = adoption {
                run.state = RunState::Blocked;
                run.reason = Some(error.to_string());
                save_run(self.store, &info, &run, "SOURCE_DRIFT_DETECTED")?;
                return Err(error);
            }
            save_run(self.store, &info, &run, "PLAN_RUNTIME_STARTED")?;
            run
        };
        require(
            run.engineering_session.as_ref() == Some(&session::for_plan(self.store, &info, id)?),
            "legacy/foreign engineering-session ownership: history is inspectable but resume requires an explicit replan",
        )?;
        if view.state == planning::PlanState::Complete {
            if run.state != RunState::Complete {
                run.state = RunState::Complete;
                save_run(self.store, &info, &run, "PLAN_RUNTIME_COMPLETED")?;
            }
            return Ok(run);
        }
        require(
            run.state == RunState::Running,
            "runtime is blocked/cancelled; explicit planner decision and replacement plan required",
        )?;
        let outcome = self.drive(&info, &view.plan, &mut run, &lease);
        if let Err(error) = outcome {
            run.reason = Some(error.to_string().chars().take(1024).collect());
            run.state = if self.cancelled(&info, Some(id))? {
                RunState::Cancelled
            } else {
                RunState::Blocked
            };
            for task in self.store.tasks(&info.repository_id, Some(id))? {
                if matches!(
                    task.state,
                    TaskState::Ready
                        | TaskState::Executing
                        | TaskState::AwaitingVerification
                        | TaskState::Verifying
                ) {
                    self.store.transition_task(
                        &info.repository_id,
                        &task.packet.task_id,
                        task.state,
                        TaskState::Blocked,
                        None,
                        now_ms()?,
                    )?;
                }
            }
            save_run(
                self.store,
                &info,
                &run,
                if run
                    .reason
                    .as_ref()
                    .is_some_and(|s| s.contains("SOURCE_DRIFT"))
                {
                    "SOURCE_DRIFT_DETECTED"
                } else {
                    "BLOCKED_NEEDS_PLANNER"
                },
            )?;
            return Err(error);
        }
        Ok(run)
    }
    /// Explicit human/orchestrator decision; never an automatic retry. Stage 4
    /// still validates the replacement and retains all prior proof/history.
    pub fn replace(&mut self, root: &Path, old: &PlanId, new: &PlanId) -> Result<()> {
        let info = graph::checked_workspace(self.store, root)?;
        let _lease = self.lease(&info)?;
        let run = load_run(self.store, &info, old)?
            .ok_or_else(|| Error::Invalid("runtime plan missing".into()))?;
        require(
            matches!(run.state, RunState::Blocked | RunState::Cancelled),
            "only stopped runtime plans can be replaced",
        )?;
        require(
            run.correction_round < self.config.max_correction_rounds,
            "correction limit reached; human escalation required",
        )?;
        let _permit = auth::authorize(
            &self.store.connection,
            &info.repository_id,
            old.as_str(),
            &session::for_plan(self.store, &info, old)?.id,
        )?;
        self.store.supersede_execution_plan(root, old, new)
    }
    fn drive(
        &mut self,
        info: &RepositoryInfo,
        plan: &planning::ExecutionPlan,
        run: &mut RunRecord,
        lease: &WorkspaceLease,
    ) -> Result<()> {
        require(
            run.correction_round <= self.config.max_correction_rounds,
            "correction limit reached; needs planner escalation",
        )?;
        let mut interrupted = false;
        for mut job in self.store.runtime_jobs(&info.root, Some(&run.plan_id))? {
            session::validate_job(self.store, info, &job)?;
            if matches!(
                job.state,
                RuntimeJobState::Queued | RuntimeJobState::Running
            ) {
                job.state = RuntimeJobState::Interrupted;
                job.failure = Some(
                    "controller interrupted; external success is unproven; no automatic retry"
                        .into(),
                );
                job.finished_at_ms = Some(now_ms()?);
                if let Some(canonical) = self.store.job(&info.repository_id, &job.job_id)? {
                    if !canonical.state.is_terminal() {
                        self.store.transition_job(
                            &info.repository_id,
                            &job.job_id,
                            canonical.state,
                            if canonical.state == JobState::Queued {
                                JobState::Cancelled
                            } else {
                                JobState::Failed
                            },
                            now_ms()?,
                        )?;
                    }
                }
                save_job(self.store, info, &job, "JOB_INTERRUPTED")?;
                interrupted = true;
            }
        }
        require(
            !interrupted,
            "interrupted job requires explicit planner review",
        )?;
        // Repair only the derived graph if a crash occurred after the durable
        // VERIFIED checkpoint and before its refresh. Recheck source first.
        if run.pending.is_none() && !run.accepted.is_empty() {
            self.expected(&info.root, &run.expected)?;
            require(
                self.store.index_repository(&info.root)?.failed == 0,
                "accepted source could not be indexed",
            )?;
        }
        loop {
            require(
                !self.cancelled(info, Some(&run.plan_id))?,
                "runtime cancelled",
            )?;
            require(
                planning::hash(&ProjectConfig::load(&info.root)?)? == run.policy_hash,
                "SOURCE_DRIFT: canonical policy changed",
            )?;
            if run.pending.is_some() {
                self.verify_pending(info, plan, run, lease)?;
                continue;
            }
            let current = self.expected(&info.root, &run.expected)?;
            let tasks = self.store.execution_tasks(&info.root, &run.plan_id)?;
            if tasks.iter().all(|t| t.state == TaskState::Verified) {
                return self.integrate(info, plan, run, &current, lease);
            }
            let task=tasks.into_iter().find(|t|t.structurally_ready).ok_or_else(||Error::Invalid("no runnable task; unverified/rejected/interrupted work requires planner decision".into()))?;
            let artifact = self.task_input(info, &task, &current)?;
            self.expected(&info.root, &run.expected)?;
            if task.state == TaskState::Planned {
                self.store.transition_task(
                    &info.repository_id,
                    &task.packet.task_id,
                    TaskState::Planned,
                    TaskState::Ready,
                    None,
                    now_ms()?,
                )?;
            }
            self.store.transition_task(
                &info.repository_id,
                &task.packet.task_id,
                TaskState::Ready,
                TaskState::Executing,
                None,
                now_ms()?,
            )?;
            let invocation = self.invoke(
                info,
                Some(&run.plan_id),
                None,
                Some(&task.packet.task_id),
                AgentRole::Executor,
                &current,
                artifact,
                lease,
            );
            // A malformed/nonzero provider may still have edited source. Capture
            // that result and scope evidence before surfacing its failure.
            let executor_id = match &invocation {
                Ok((job, _)) => job.job_id.clone(),
                Err(_) => self
                    .store
                    .runtime_jobs(&info.root, Some(&run.plan_id))?
                    .into_iter()
                    .filter(|j| j.role == AgentRole::Executor)
                    .find_map(|j| {
                        let input: JobInput = self.artifacts.decode(&j.input).ok()?;
                        (input.task_id.as_ref() == Some(&task.packet.task_id)).then_some(j.job_id)
                    })
                    .ok_or_else(|| {
                        Error::Invalid(
                            "executor launch failed before issued job was persisted".into(),
                        )
                    })?,
            };
            let after = source::capture(&info.root, &self.artifacts)?;
            let diff = source::diff(
                &current,
                &after,
                &run.plan_id,
                Some(&task.packet),
                Some(&executor_id),
                &ProjectConfig::load(&info.root)?,
                &self.artifacts,
            )?;
            let reference = self.artifacts.json(&diff)?;
            let evidence = self.evidence(
                info,
                &after,
                &format!(
                    "captured exact file changes; scope violations: {}",
                    diff.scope_violations.len()
                ),
                &reference,
            )?;
            event(
                self.store,
                info,
                Some(&run.plan_id),
                Some(&executor_id),
                "DIFF_CAPTURED",
                &reference.hash,
            )?;
            let (job, value) = invocation?;
            require(
                diff.scope_violations.is_empty(),
                "executor wrote outside allowed scope; result retained but not accepted",
            )?;
            require(
                !diff
                    .changes
                    .iter()
                    .any(|c| task.contract.exclusions.iter().any(|s| permits(s, &c.path))),
                "executor violated verification contract exclusions",
            )?;
            let reported: ResultPacket = serde_json::from_value(value)?;
            require(
                reported.changed_paths.iter().collect::<BTreeSet<_>>()
                    == diff.changes.iter().map(|c| &c.path).collect(),
                "executor changed-path report differs from captured files",
            )?;
            self.store.transition_task(
                &info.repository_id,
                &task.packet.task_id,
                TaskState::Executing,
                TaskState::AwaitingVerification,
                None,
                now_ms()?,
            )?;
            run.pending = Some(PendingTask {
                task_id: task.packet.task_id,
                executor: job.job_id,
                before: diff.before,
                after: diff.after,
                diff: reference,
                evidence: vec![evidence],
                verifier: None,
                proof: None,
            });
            save_run(self.store, info, run, "TASK_EXECUTION_COMPLETED")?;
        }
    }
    fn verify_pending(
        &mut self,
        info: &RepositoryInfo,
        plan: &planning::ExecutionPlan,
        run: &mut RunRecord,
        lease: &WorkspaceLease,
    ) -> Result<()> {
        let mut pending = run.pending.clone().expect("pending task");
        let after = self.expected(&info.root, &pending.after)?;
        let task = plan
            .packet
            .tasks
            .iter()
            .find(|t| t.task_id == pending.task_id)
            .ok_or_else(|| Error::Invalid("pending task not in plan".into()))?;
        let current = self
            .store
            .task(&info.repository_id, &pending.task_id)?
            .ok_or_else(|| Error::Invalid("pending task missing".into()))?;
        if current.state == TaskState::Verified {
            require(
                pending.proof.is_some(),
                "verified task lacks runtime checkpoint",
            )?;
        } else {
            if pending.proof.is_none() {
                if let Some((job, proof, input)) =
                    self.completed_verifier(info, &run.plan_id, Some(&pending.task_id), &after)?
                {
                    require(
                        input.artifact["evidence"] == serde_json::to_value(&pending.evidence)?,
                        "recovery evidence mismatch",
                    )?;
                    pending.verifier = Some(job.job_id);
                    pending.proof = Some(proof);
                    run.pending = Some(pending.clone());
                    save_run(self.store, info, run, "VERIFIER_OUTPUT_RECOVERED")?;
                }
            }
            if pending.proof.is_none() {
                if pending.evidence.len() == 1 {
                    pending.evidence.extend(self.checks(
                        info,
                        &run.plan_id,
                        &task.verification,
                        &after,
                        lease,
                    )?);
                }
                run.pending = Some(pending.clone());
                save_run(self.store, info, run, "PACKET_CHECKS_COMPLETED")?;
                if current.state == TaskState::AwaitingVerification {
                    self.store.transition_task(
                        &info.repository_id,
                        &pending.task_id,
                        TaskState::AwaitingVerification,
                        TaskState::Verifying,
                        None,
                        now_ms()?,
                    )?;
                }
                let inspection = self
                    .store
                    .execution_tasks(&info.root, &run.plan_id)?
                    .into_iter()
                    .find(|t| t.packet.task_id == pending.task_id)
                    .expect("plan task");
                let target = VerificationTarget::Packet {
                    task_id: pending.task_id.clone(),
                    executor_job_id: pending.executor.clone(),
                };
                let artifact = json!({"task":task,"contract":inspection.contract,"invariants":inspection.invariants,"target":target,"evidence":pending.evidence,"evidence_records":self.evidence_input(info,&pending.evidence)?,"diff":self.diff_input(&pending.diff)?,"verification_schema":schemars::schema_for!(VerificationPacket)});
                event(
                    self.store,
                    info,
                    Some(&run.plan_id),
                    None,
                    "VERIFICATION_STARTED",
                    "fresh packet verifier; no executor transcript",
                )?;
                let (job, value) = self.invoke(
                    info,
                    Some(&run.plan_id),
                    None,
                    Some(&pending.task_id),
                    AgentRole::Verifier,
                    &after,
                    artifact,
                    lease,
                )?;
                pending.verifier = Some(job.job_id);
                pending.proof = Some(serde_json::from_value(value)?);
                run.pending = Some(pending.clone());
                save_run(self.store, info, run, "VERIFIER_OUTPUT_CAPTURED")?;
            }
            self.expected(&info.root, &pending.after)?;
            let proof = pending.proof.as_ref().expect("recorded proof");
            let next = match proof.decision {
                VerificationDecision::Pass => TaskState::Verified,
                VerificationDecision::Reject => TaskState::Rejected,
                VerificationDecision::Blocked => TaskState::Blocked,
            };
            self.store.transition_task(
                &info.repository_id,
                &pending.task_id,
                TaskState::Verifying,
                next,
                Some(proof),
                now_ms()?,
            )?;
            require(
                next == TaskState::Verified,
                "verifier rejected/blocked task; dependents remain locked; explicit correction plan required",
            )?;
        }
        run.expected = pending.after.clone();
        run.accepted.insert(
            pending.task_id,
            AcceptedTask {
                executor: pending.executor,
                verifier: pending
                    .verifier
                    .ok_or_else(|| Error::Invalid("missing fresh verifier identity".into()))?,
                diff: pending.diff,
                evidence: pending.evidence,
            },
        );
        run.pending = None;
        save_run(self.store, info, run, "TASK_VERIFIED")?;
        require(
            self.store.index_repository(&info.root)?.failed == 0,
            "accepted source could not be indexed; planner review required",
        )?;
        Ok(())
    }
    fn integrate(
        &mut self,
        info: &RepositoryInfo,
        plan: &planning::ExecutionPlan,
        run: &mut RunRecord,
        current: &SourceSnapshot,
        lease: &WorkspaceLease,
    ) -> Result<()> {
        require(
            run.accepted.len() == plan.packet.tasks.len(),
            "runtime lacks authentic packet acceptance records",
        )?;
        if let Some((_, proof, _)) = self.completed_verifier(info, &run.plan_id, None, current)? {
            self.expected(&info.root, &run.expected)?;
            self.store.complete_execution_plan(
                &info.root,
                &run.plan_id,
                &proof,
                &current.source_ref()?,
            )?;
            run.state = RunState::Complete;
            return save_run(self.store, info, run, "PLAN_RUNTIME_COMPLETED");
        }
        let baseline: SourceSnapshot = self.artifacts.decode(&run.baseline)?;
        let diff = source::diff(
            &baseline,
            current,
            &run.plan_id,
            None,
            None,
            &ProjectConfig::load(&info.root)?,
            &self.artifacts,
        )?;
        let reference = self.artifacts.json(&diff)?;
        event(
            self.store,
            info,
            Some(&run.plan_id),
            None,
            "INTEGRATION_STARTED",
            &reference.hash,
        )?;
        let mut evidence = self.checks(
            info,
            &run.plan_id,
            &plan.packet.integration_verification,
            current,
            lease,
        )?;
        evidence.push(self.evidence(info, current, "final combined runtime diff", &reference)?);
        let target = VerificationTarget::Integration {
            plan_id: run.plan_id.clone(),
            executor_job_ids: run.accepted.values().map(|a| a.executor.clone()).collect(),
        };
        let invariants: BTreeMap<_, _> = self
            .store
            .execution_tasks(&info.root, &run.plan_id)?
            .into_iter()
            .flat_map(|t| t.invariants)
            .collect();
        let artifact = json!({"plan":plan.packet,"contract":plan.metadata.integration,"invariants":invariants,"target":target,"evidence":evidence,"evidence_records":self.evidence_input(info,&evidence)?,"diff":self.diff_input(&reference)?,"verification_schema":schemars::schema_for!(VerificationPacket)});
        let (_, value) = self.invoke(
            info,
            Some(&run.plan_id),
            None,
            None,
            AgentRole::Verifier,
            current,
            artifact,
            lease,
        )?;
        let proof: VerificationPacket = serde_json::from_value(value)?;
        self.expected(&info.root, &run.expected)?;
        self.store.complete_execution_plan(
            &info.root,
            &run.plan_id,
            &proof,
            &current.source_ref()?,
        )?;
        run.state = RunState::Complete;
        save_run(self.store, info, run, "PLAN_RUNTIME_COMPLETED")
    }
}
