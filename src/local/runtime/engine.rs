use super::*;
use process::{CancellationOutcome, ProcessSpec, WorkspaceLease};
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
    overrides: BTreeMap<String, routing::RolePatch>,
    policy_observer: Option<PolicyObserver>,
    boundary_observer: Option<PolicyObserver>,
    persist_run: bool,
    branch_workspace: Option<String>,
    branch_compatibility: Vec<concurrency::CompatibilityDecision>,
    branch_generation: Option<String>,
    branch_physical_index: Option<String>,
    branch_canonical_index: Option<String>,
    authority_info: Option<RepositoryInfo>,
}
type PolicyObserver = Box<dyn FnMut(&str)>;
/// Outcome of relaying one worker context request.
enum Relay {
    /// A ContextDelta was approved; re-issue the work as a fresh job.
    Granted,
    /// Something outside the envelope is needed; a planner must decide.
    Escalated,
    Denied(String),
}
/// One fresh verification, with its own independent context relay. `build`
/// composes the issued artifact from the diff material, this verifier's own
/// deltas and its relay budget; executor request history is never in it.
struct Verification<'a> {
    task: Option<&'a TaskId>,
    diff: &'a ArtifactRef,
    invariants: Vec<String>,
    /// The paths this verification may request context from.
    envelope: Vec<ScopePath>,
    build: &'a dyn Fn(Value, Value, Value) -> Value,
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
            overrides: BTreeMap::new(),
            policy_observer: None,
            boundary_observer: None,
            persist_run: true,
            branch_workspace: None,
            branch_compatibility: vec![],
            branch_generation: None,
            branch_physical_index: None,
            branch_canonical_index: None,
            authority_info: None,
        })
    }
    fn for_branch(
        mut self,
        workspace: String,
        compatibility: Vec<concurrency::CompatibilityDecision>,
        generation: String,
        physical_index: String,
        canonical_index: String,
        authority_info: RepositoryInfo,
    ) -> Self {
        self.persist_run = false;
        self.branch_workspace = Some(workspace);
        self.branch_compatibility = compatibility;
        self.branch_generation = Some(generation);
        self.branch_physical_index = Some(physical_index);
        self.branch_canonical_index = Some(canonical_index);
        self.authority_info = Some(authority_info);
        self
    }
    fn checkpoint(&mut self, info: &RepositoryInfo, run: &RunRecord, phase: &str) -> Result<()> {
        if self.persist_run {
            save_run(self.store, info, run, phase)?;
        }
        Ok(())
    }
    pub fn with_check_launcher(mut self, launcher: Box<dyn process::CheckLauncher>) -> Self {
        self.checks = launcher;
        self
    }
    /// Trusted embedding/test observer. Cannot replace the frozen policy or
    /// bypass its validation. Boundaries are before_snapshot, validated, prelaunch.
    #[doc(hidden)]
    pub fn with_policy_observer(mut self, observer: impl FnMut(&str) + 'static) -> Self {
        self.policy_observer = Some(Box::new(observer));
        self
    }
    /// Trusted embedding/test hook for deterministic authority and publication
    /// crash-boundary exercises. It cannot alter production decisions.
    #[doc(hidden)]
    pub fn with_boundary_observer(mut self, observer: impl FnMut(&str) + 'static) -> Self {
        self.boundary_observer = Some(Box::new(observer));
        self
    }
    fn observe_boundary(&mut self, boundary: &str) {
        if let Some(observer) = &mut self.boundary_observer {
            observer(boundary);
        }
    }
    /// Trusted caller/user policy only; never populated from provider output.
    pub fn with_role_overrides(
        mut self,
        overrides: BTreeMap<String, routing::RolePatch>,
    ) -> Result<Self> {
        routing::validate_patches(&overrides)?;
        self.overrides = overrides;
        Ok(self)
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
        policy: &ProjectConfig,
    ) -> Result<ProcessSpec> {
        let scratch = self
            .paths
            .data_root
            .join("runtime/scratch")
            .join(id.replace(':', "-"));
        paths::ensure_directory(&scratch)?;
        Ok(ProcessSpec {
            project_policy_hash: Some(planning::hash(policy)?),
            native_auth: None,
            api_key: None,
            executable: PathBuf::new(),
            project_executable: false,
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
            protected: policy.protected.clone(),
            credential_env: vec![],
            experiment_event_file: None,
            class: crate::local::security::WorkerClass::ProviderFrontend,
            cache_root: self.paths.cache_root.clone(),
            security: self.config.security.tightened(&policy.security),
            issued: None,
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
        inventory: manifest::ContextInventory,
        lease: &WorkspaceLease,
        expected_policy_hash: &str,
    ) -> Result<(RuntimeJob, Value)> {
        let name = routing::role_name(role);
        if let Some(observer) = &mut self.policy_observer {
            observer("before_snapshot");
        }
        let project = ProjectConfig::load(&info.root);
        if project
            .as_ref()
            .ok()
            .and_then(|p| planning::hash(p).ok())
            .as_deref()
            != Some(expected_policy_hash)
        {
            event(
                self.store,
                info,
                plan,
                None,
                "POLICY_DRIFT",
                "SOURCE_DRIFT: project policy changed; replan/revalidation required",
            )?;
            return Err(Error::Invalid(
                "SOURCE_DRIFT: project policy changed; replan/revalidation required".into(),
            ));
        }
        let project = project?;
        if let Some(observer) = &mut self.policy_observer {
            observer("validated");
        }
        // Machine-owned hard ceiling; a project may only further lower it,
        // never raise it. Provider/planner output has no path to this value.
        let max_agents = self.config.effective_max_agents(&project.routing);
        let resolved = routing::resolve(
            &self.config,
            &project.routing,
            name,
            self.overrides.get(name),
        )?;
        if !resolved.policy_skipped.is_empty() {
            event(
                self.store,
                info,
                plan,
                None,
                "ROUTE_POLICY_FILTERED",
                &serde_json::to_string(
                    &serde_json::json!({"role":name,"primary":resolved.configured_primary,"skipped":resolved.policy_skipped,"selected":resolved.primary,"reason":"PROJECT_POLICY_RESTRICTION"}),
                )?,
            )?;
        }
        let mut failures = Vec::new();
        for (attempt, config) in std::iter::once(&resolved.primary)
            .chain(&resolved.fallbacks)
            .take(1 + resolved.profile.max_fallback_attempts)
            .enumerate()
        {
            if attempt > 0 {
                require(
                    self.capture(info)? == *source,
                    "source changed during failed startup; fallback blocked",
                )?;
                require(!self.cancelled(info, plan)?, "cancelled before fallback")?;
                event(
                    self.store,
                    info,
                    plan,
                    None,
                    "ROUTE_FALLBACK",
                    &serde_json::to_string(
                        &serde_json::json!({"role":name,"primary":resolved.primary,"selected":config,"attempt":attempt,"failures":failures}),
                    )?,
                )?;
            }
            let snapshot = routing::RouteSnapshot {
                policy_skipped: resolved.policy_skipped.clone(),
                requested_role: name.into(),
                primary: resolved.configured_primary.clone(),
                selected: config.clone(),
                attempt,
                failures: failures.clone(),
                sources: resolved.sources.clone(),
                profile_hash: planning::hash(&resolved.profile)?,
                project_policy_hash: expected_policy_hash.into(),
            };
            let mut retries = 0;
            loop {
                match self.invoke_attempt(
                    info,
                    plan,
                    request,
                    task,
                    role,
                    source,
                    artifact.clone(),
                    inventory.clone(),
                    lease,
                    &resolved.profile,
                    snapshot.clone(),
                    &project,
                    max_agents,
                ) {
                    Err(Error::ProviderAvailability(reason)) => {
                        failures.push(routing::FailedRoute {
                            route: config.clone(),
                            reason,
                        });
                        break;
                    }
                    Err(Error::Provider { class, detail })
                        if retries < MAX_PROVIDER_RETRIES
                            && self.retry_safe(info, plan, role, class, source)? =>
                    {
                        retries += 1;
                        event(
                            self.store,
                            info,
                            plan,
                            None,
                            "PROVIDER_RETRY",
                            &serde_json::to_string(&serde_json::json!({
                                "role": name,
                                "task": task,
                                "retry": retries,
                                "max_retries": MAX_PROVIDER_RETRIES,
                                "class": class,
                                "failure": detail,
                            }))?,
                        )?;
                    }
                    Err(Error::Provider { class, detail }) if retries > 0 => {
                        return Err(Error::Provider {
                            class,
                            detail: format!(
                                "{detail} (after {retries} automatic retr{})",
                                if retries == 1 { "y" } else { "ies" }
                            ),
                        });
                    }
                    result => return result,
                }
            }
        }
        Err(Error::Invalid(format!(
            "role {name}: all permitted route attempts failed: {}",
            serde_json::to_string(&failures)?
        )))
    }
    #[allow(clippy::too_many_arguments)]
    fn invoke_attempt(
        &mut self,
        info: &RepositoryInfo,
        plan: Option<&PlanId>,
        request: Option<&planning::PlanningRequestId>,
        task: Option<&TaskId>,
        role: AgentRole,
        source: &SourceSnapshot,
        artifact: Value,
        inventory: manifest::ContextInventory,
        lease: &WorkspaceLease,
        profile: &routing::RoleProfile,
        route: routing::RouteSnapshot,
        project: &ProjectConfig,
        max_agents: usize,
    ) -> Result<(RuntimeJob, Value)> {
        let config = route.selected.clone();
        let (job_id, session_id) = self.identity()?;
        let authority = self.authority_info.clone().unwrap_or_else(|| info.clone());
        let ownership =
            session::issued(self.store, &authority, plan, request, task, role, &job_id)?;
        let mut input = JobInput {
            compiled: None,
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
        let compiled = prompt::compile(profile, &route.project_policy_hash, &input)?;
        let prompt_provenance = compiled.provenance.clone();
        // Opt-in hard visibility: a worker may read only the repository files
        // issued to it in full, plus (when writable) its own write scope.
        let issued = (self.config.context.visibility == ContextVisibility::Issued
            && role != AgentRole::Planner)
            .then(|| crate::local::security::IssuedVisibility {
                read_files: inventory
                    .paths
                    .iter()
                    .filter(|p| p.kind == manifest::SuppliedKind::File && !p.truncated)
                    .map(|p| info.root.join(&p.path))
                    .collect(),
                write_paths: inventory
                    .write_scope
                    .iter()
                    .map(|s| info.root.join(s.path()))
                    .collect(),
            });
        let context_manifest = manifest::for_job(&input, request, &prompt_provenance, inventory)?;
        input.compiled = Some(compiled);
        let mut job = RuntimeJob {
            execution_root: Some(info.root.display().to_string()),
            planner_usage: None,
            reported_verification: None,
            availability_failure: None,
            route: Some(route),
            prompt: Some(prompt_provenance),
            context_manifest: Some(context_manifest),
            context_request: None,
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
            failure_class: None,
        };
        create_job(self.store, &authority, &job, max_agents)?;
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
        let mut launched = false;
        let result: Result<Value> = (|| {
            let mut spec = self.process_spec(info, job_id.as_str(), role, lease, project)?;
            spec.issued = issued.clone();
            spec.writable &= !profile.read_only;
            spec.network &= profile.network;
            spec.timeout_ms = profile.timeout_ms;
            let _scratch = process::ScratchCleanup(spec.scratch.clone());
            {
                let adapter = self.adapters.get_mut(&config.provider).ok_or_else(|| {
                    Error::ProviderAvailability(routing::FailureClass::ProviderUnavailable)
                })?;
                routing::validate_capabilities(&adapter.capabilities(), &config)?;
                adapter.preflight()?;
            }
            if let Some(observer) = &mut self.policy_observer {
                observer("prelaunch");
            }
            if ProjectConfig::load(&info.root)
                .ok()
                .and_then(|p| planning::hash(&p).ok())
                .as_deref()
                != Some(
                    job.route
                        .as_ref()
                        .expect("issued route")
                        .project_policy_hash
                        .as_str(),
                )
            {
                event(
                    self.store,
                    info,
                    plan,
                    Some(&job_id),
                    "POLICY_DRIFT",
                    "SOURCE_DRIFT: project policy changed before launch; replan/revalidation required",
                )?;
                return Err(Error::Invalid("SOURCE_DRIFT: project policy changed before launch; replan/revalidation required".into()));
            }
            self.revalidate_launch_at_adapter(info, plan, task, role, source)?;
            let adapter = self.adapters.get_mut(&config.provider).ok_or_else(|| {
                Error::ProviderAvailability(routing::FailureClass::ProviderUnavailable)
            })?;
            let mut process = adapter.launch(&input, spec, &config).map_err(|e| match e {
                Error::Io(ref io)
                    if matches!(
                        io.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                    ) =>
                {
                    Error::ProviderAvailability(routing::FailureClass::StartupFailure)
                }
                other => other,
            })?;
            launched = true;
            let liveness = liveness::Guard::new(
                self.store.connection.path().unwrap_or(""),
                info.repository_id.as_str(),
                info.workspace_id.as_str(),
                &input.ownership.engineering_session_id,
                input.ownership.agent_instance_id.as_str(),
                job_id.as_str(),
            );
            job.pid = process.pid();
            job.started_at_ms = Some(now_ms()?);
            job.state = RuntimeJobState::Running;
            save_job(self.store, &authority, &job, "JOB_STARTED")?;
            let started = Instant::now();
            let mut interruption = None;
            let mut cancellation_checked = false;
            let mut timeout_checked = false;
            let output = loop {
                // Revoke before polling/cancellation (including errors/unwind).
                liveness.set(false);
                if let Some(output) = process.poll()? {
                    break output;
                }
                if !cancellation_checked && self.cancelled(info, plan)? {
                    cancellation_checked = true;
                    if process.cancel()? == CancellationOutcome::Applied {
                        interruption = Some("cancelled".to_string());
                    }
                }
                if interruption.is_none()
                    && !timeout_checked
                    && started.elapsed().as_millis() > profile.timeout_ms as u128
                {
                    timeout_checked = true;
                    if process.cancel()? == CancellationOutcome::Applied {
                        interruption = Some("timeout".into());
                    }
                }
                liveness.set(process.liveness_confirmed());
                std::thread::sleep(Duration::from_millis(25));
            };
            drop(liveness);
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
            let usage = self.adapters[&config.provider]
                .usage(&output)
                .unwrap_or_default();
            let timestamp_ms = now_ms()?;
            let context = EventContext {
                agent_id: Some(
                    AgentId::new(format!("agent:{}", job_id.as_str())).map_err(Error::Invalid)?,
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
            if plan.is_some() {
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
            } else if usage.validate().is_ok() {
                job.planner_usage = Some(usage);
            }
            let contract = super::contract::RoleContract::of(role, &input.artifact);
            let document = contract.output().0;
            let adapter = &self.adapters[&config.provider];
            if let Some(reason) = &interruption {
                return Err(if reason == "timeout" {
                    provider_failure(
                        OutcomeClass::RetryableProviderFailure,
                        format!("provider timed out after {} ms", profile.timeout_ms),
                    )
                } else {
                    Error::Invalid(reason.clone())
                });
            }
            if let Some(failure) = &output.failure {
                return Err(provider_failure(
                    OutcomeClass::RetryableProviderFailure,
                    failure.clone(),
                ));
            }
            // Provider-emitted infrastructure signals are consulted only on a
            // failed exchange, never to reinterpret a successful one.
            let faulted = |fallback: String| match adapter.fault(&output) {
                Some((true, summary)) => provider_failure(
                    OutcomeClass::RetryableProviderFailure,
                    format!("{fallback}: {summary}"),
                ),
                Some((false, summary)) => {
                    provider_failure(OutcomeClass::NonretryableProviderFailure, summary)
                }
                None => provider_failure(OutcomeClass::RetryableProviderFailure, fallback),
            };
            if output.exit != Some(0) {
                return Err(faulted(format!("provider exited {:?}", output.exit)));
            }
            let value = adapter.collect(&output).map_err(|e| {
                faulted(format!(
                    "reply is not exactly one {document} JSON document ({e})"
                ))
            })?;
            let malformed = |e: serde_json::Error| {
                provider_failure(
                    OutcomeClass::RetryableProviderFailure,
                    format!("reply does not match the {document} schema ({e})"),
                )
            };
            let invalid = |rule: &str, detail: String| {
                provider_failure(
                    OutcomeClass::ValidationFailure,
                    format!("[{rule}] {detail}"),
                )
            };
            match role {
                AgentRole::Executor => {
                    let result: ResultPacket =
                        serde_json::from_value(value.clone()).map_err(malformed)?;
                    result
                        .validate()
                        .map_err(|e| invalid("EXEC-OUTPUT", e.to_string()))?;
                    // Status is judged by the caller: a valid BLOCKED/FAILED
                    // result is the executor's honest answer, not a malformed one.
                    if Some(&result.task_id) != task || result.executor_job_id != job_id {
                        return Err(invalid(
                            "EXEC-OUTPUT",
                            format!(
                                "result names task {} / executor job {}, but this job is task {} / job {}",
                                result.task_id.as_str(),
                                result.executor_job_id.as_str(),
                                task.map(TaskId::as_str).unwrap_or("-"),
                                job_id.as_str()
                            ),
                        ));
                    }
                    if !result.evidence.is_empty() {
                        return Err(invalid(
                            "EXEC-OUTPUT",
                            "evidence must be []; agentctl captures evidence itself".into(),
                        ));
                    }
                }
                AgentRole::Verifier => {
                    let proof: VerificationPacket =
                        serde_json::from_value(value.clone()).map_err(malformed)?;
                    proof
                        .validate()
                        .map_err(|e| invalid("VER-REFS", e.to_string()))?;
                    if proof.verifier_job_id != job_id
                        || serde_json::to_value(&proof.target)? != input.artifact["target"]
                        || serde_json::to_value(&proof.evidence)? != input.artifact["evidence"]
                    {
                        return Err(invalid(
                            "VER-OUTPUT",
                            "verifier_job_id, target and evidence must equal the issued job id, target and evidence verbatim".into(),
                        ));
                    }
                    // A context request is not a decision and is never counted.
                    job.reported_verification =
                        proof.context_request.is_none().then_some(proof.decision);
                }
                AgentRole::Planner => {
                    // Shape only; the plan contract is validated by the caller,
                    // which owns the bounded correction attempt.
                    let _: super::planner::PlanDecision =
                        serde_json::from_value(value.clone()).map_err(malformed)?;
                }
            }
            job.output = Some(self.artifacts.json(&value)?);
            Ok(value)
        })();
        // Availability fallback is only a prelaunch boundary. Errors from a
        // running process or result parser cannot reclassify engineering work.
        let result = result.map_err(|error| match error {
            Error::ProviderAvailability(_) if launched => {
                Error::Invalid("unclassified failure after provider launch; no fallback".into())
            }
            other => other,
        });
        job.finished_at_ms = Some(now_ms()?);
        job.availability_failure = match &result {
            Err(Error::ProviderAvailability(reason)) => Some(*reason),
            _ => None,
        };
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
        job.failure_class = match &result {
            Err(Error::Provider { class, .. }) => Some(*class),
            _ => None,
        };
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
            &authority,
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
    /// Whether a failed exchange may be repeated as a fresh job without any
    /// risk to canonical state. Planners and verifiers write nothing; an
    /// executor only when the workspace is byte-identical to its issued source,
    /// so no uncertain mutation can be carried into the retry. A planner's
    /// contract violation goes to its own correction attempt instead.
    fn retry_safe(
        &self,
        info: &RepositoryInfo,
        plan: Option<&PlanId>,
        role: AgentRole,
        class: OutcomeClass,
        source: &SourceSnapshot,
    ) -> Result<bool> {
        let eligible = match class {
            OutcomeClass::RetryableProviderFailure => true,
            OutcomeClass::ValidationFailure => role != AgentRole::Planner,
            OutcomeClass::NonretryableProviderFailure | OutcomeClass::SemanticRejection => false,
        };
        if !eligible || self.cancelled(info, plan)? {
            return Ok(false);
        }
        Ok(role != AgentRole::Executor || self.capture(info)? == *source)
    }
    /// The workspace is byte-identical to the last source this run recorded:
    /// a captured pending result when one exists, otherwise the accepted source.
    /// Captured concurrent branches are durable results in their own retained
    /// worktrees and resume through normal reconciliation; only a publication
    /// intent that is still unresolved makes the canonical tree uncertain.
    fn unchanged_since_recorded(&self, info: &RepositoryInfo, run: &RunRecord) -> Result<bool> {
        if run.reconciliation.is_some() {
            return Ok(false);
        }
        let recorded = run.pending.as_ref().map_or(&run.expected, |p| &p.after);
        Ok(self.capture(info)? == self.artifacts.decode::<SourceSnapshot>(recorded)?)
    }
    fn capture(&self, info: &RepositoryInfo) -> Result<SourceSnapshot> {
        let mut captured = source::capture_bound(
            &info.root,
            &self.artifacts,
            &info.repository_id,
            &info.workspace_id,
        )?;
        if let Some(physical) = &self.branch_physical_index {
            require(
                &captured.index_hash == physical,
                "SOURCE_DRIFT: isolated executor changed its Git index",
            )?;
            captured.index_hash = self
                .branch_canonical_index
                .clone()
                .expect("branch canonical index");
        }
        Ok(captured)
    }
    fn expected(&self, info: &RepositoryInfo, reference: &ArtifactRef) -> Result<SourceSnapshot> {
        let expected: SourceSnapshot = self.artifacts.decode(reference)?;
        let current = self.capture(info)?;
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
            // A job that only requested context reached no decision, so it can
            // never be recovered as verification proof.
            if job.role != AgentRole::Verifier
                || job.state != RuntimeJobState::Succeeded
                || job.context_request.is_some()
            {
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
    /// The executor's issued input: planner-authored base context plus every
    /// approved ContextDelta, in round order. The runtime never selects
    /// repository material of its own, and a Directory read scope contributes
    /// no files: it authorizes what the executor may *request*.
    fn task_input(
        &self,
        task: &planning::TaskInspection,
        base: &context::IssuedContext,
        deltas: &[(context::ContextDelta, ArtifactRef)],
        relay: Value,
        round: u32,
    ) -> Result<(Value, manifest::ContextInventory)> {
        let mut inventory = manifest::ContextInventory {
            invariants: task.invariants.keys().cloned().collect(),
            write_scope: task.packet.write_scope.clone(),
            visibility: Some(self.config.context.visibility),
            ..Default::default()
        };
        context::inventory(&mut inventory, Some(base), deltas, round)?;
        let issued: Vec<&context::ContextDelta> = deltas.iter().map(|(d, _)| d).collect();
        Ok((
            json!({"task":task.packet,"contract":task.contract,"invariants":task.invariants,"constraints":task.constraints,"context":base,"deltas":issued,"context_relay":relay,"result_schema":schemars::schema_for!(ResultPacket),"instruction":super::contract::EXECUTOR_INSTRUCTION}),
            inventory,
        ))
    }
    /// What the relay tells a worker about its remaining context budget. These
    /// are machine-owned numbers; a worker cannot raise them by asking.
    fn relay_state(&self, ledger: &context::ContextLedger, max_rounds: u32) -> Value {
        let limits = &self.config.context;
        json!({
            "round": ledger.rounds_used(),
            "max_rounds": max_rounds,
            "rounds_remaining": max_rounds.saturating_sub(ledger.rounds_used()),
            "max_request_bytes": limits.max_round_bytes,
            "granted_bytes": ledger.granted_bytes,
            "task_bytes_remaining": (limits.max_task_bytes as usize).saturating_sub(ledger.granted_bytes),
            "escalations_used": ledger.escalations,
            "item_kinds": ["SYMBOL_DEFINITION","SYMBOL_BY_NAME","SYMBOL_RELATIONS","RELATED_TESTS","NEIGHBORHOOD","FILE_RANGE","MEMORY"],
            "policy": "agentctl resolves a request deterministically inside your read scope; anything outside it is not granted automatically and blocks the task for a planner decision.",
        })
    }
    /// The bounded verifier diff and, for the manifest, the changed paths it
    /// carries (bound by their after-state, or before-state for deletions).
    fn diff_input(
        &self,
        reference: &ArtifactRef,
    ) -> Result<(Value, Vec<manifest::SuppliedSource>)> {
        let diff: CapturedDiff = self.artifacts.decode(reference)?;
        diffview::view(&diff, reference, &self.artifacts)
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
        let policy_hash = self
            .store
            .execution_plan(&info.root, plan)?
            .plan
            .metadata
            .source
            .policy_hash;
        require(
            planning::hash(&policy)? == policy_hash,
            "SOURCE_DRIFT: check policy changed; replan/revalidation required",
        )?;
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
            let mut spec =
                self.process_spec(info, id.as_str(), AgentRole::Verifier, lease, &policy)?;
            let _scratch = process::ScratchCleanup(spec.scratch.clone());
            spec.class = crate::local::security::WorkerClass::Tool;
            spec.network = false;
            spec.args = command.args.clone();
            // Repository-declared program: it may be run, but it must not widen
            // the worker's read policy to its own install directory.
            spec.executable = PathBuf::from(&command.program);
            spec.project_executable = true;
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
            require(
                ProjectConfig::load(&info.root)
                    .ok()
                    .and_then(|p| planning::hash(&p).ok())
                    .as_deref()
                    == Some(policy_hash.as_str()),
                "SOURCE_DRIFT: check policy changed before launch; replan/revalidation required",
            )?;
            let mut process = self.checks.launch(&spec)?;
            let started = Instant::now();
            let mut interrupted = false;
            let mut termination_checked = false;
            let output = loop {
                if let Some(output) = process.poll()? {
                    break output;
                }
                if !termination_checked
                    && (self.cancelled(info, Some(plan))?
                        || started.elapsed().as_millis() > self.config.timeout_ms as u128)
                {
                    termination_checked = true;
                    interrupted = process.cancel()? == CancellationOutcome::Applied;
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
                self.capture(info)? == *source,
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
        let inventory = manifest::ContextInventory::planner(&prepared);
        let base = json!({"planner_packet":prepared,"output_template":template,"decision_schema":schemars::schema_for!(super::planner::PlanDecision),"instruction":super::contract::PLANNER_INSTRUCTION});
        let mut correction: Option<Value> = None;
        let mut attempt = 0;
        loop {
            let mut artifact = base.clone();
            if let Some(correction) = &correction {
                artifact["correction"] = correction.clone();
            }
            let (mut job, value) = self.invoke(
                &info,
                None,
                Some(request),
                None,
                AgentRole::Planner,
                &source,
                artifact,
                inventory.clone(),
                &lease,
                &prepared.request.source.policy_hash,
            )?;
            let result = (|| {
                require(
                    source::capture(root, &self.artifacts)? == source,
                    "SOURCE_DRIFT during planner invocation",
                )?;
                let decision: super::planner::PlanDecision = serde_json::from_value(value.clone())?;
                let plan = super::planner::envelope(&prepared, decision)?;
                self.store.import_execution_plan(root, &plan)
            })();
            let Err(error) = result else {
                return result; // Import publishes VALIDATED; activation remains explicit.
            };
            let message = error.to_string();
            job.state = RuntimeJobState::Failed;
            job.failure = Some(message.chars().take(1024).collect());
            // Only a refusal that cites a planner-contract rule is the planner's
            // own mistake; drift, policy or storage failures are not.
            let correctable = message.contains("[PLAN-");
            job.failure_class = correctable.then_some(OutcomeClass::ValidationFailure);
            save_job(self.store, &info, &job, "PLANNER_OUTPUT_REJECTED")?;
            if !correctable || attempt >= super::config::MAX_PLAN_CORRECTIONS {
                return Err(error);
            }
            attempt += 1;
            event(
                self.store,
                &info,
                None,
                Some(&job.job_id),
                "PLAN_CORRECTION_REQUESTED",
                &message.chars().take(1024).collect::<String>(),
            )?;
            // Structured feedback for one fresh planner job: the exact refusal
            // (rule id, task, offending value, expected form) and the decision
            // it refers to. Validation is unchanged; nothing else is issued.
            correction = Some(json!({
                "attempt": attempt,
                "max_attempts": super::config::MAX_PLAN_CORRECTIONS,
                "refusal": message.chars().take(1024).collect::<String>(),
                "previous_decision": value,
                "instruction": "agentctl refused previous_decision for the reason in `refusal`, which names the violated planner-contract rule. Return a complete corrected PlanDecision that satisfies every planner-contract rule; change only what the refusal requires."
            }));
        }
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
        let existing = load_run(self.store, &info, id)?;
        let resumed = existing.is_some();
        let mut run = if let Some(run) = existing {
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
            if let Some(replan) = &view.plan.metadata.replan
                && let Some(previous) = load_run(self.store, &info, &replan.previous_plan_id)?
            {
                round = previous.correction_round + 1;
            }
            let mut run = RunRecord {
                engineering_session: Some(session::for_plan(self.store, &info, id)?),
                plan_id: id.clone(),
                workspace_id: info.workspace_id.clone(),
                state: RunState::Running,
                baseline: baseline.clone(),
                expected: baseline.clone(),
                verified: Some(baseline),
                policy_hash: view.plan.metadata.source.policy_hash.clone(),
                accepted: BTreeMap::new(),
                pending: None,
                branches: BTreeMap::new(),
                batch_authority: None,
                reconciliation: None,
                reason: None,
                correction_round: round,
                context: BTreeMap::new(),
                executor_relaunches: BTreeMap::new(),
                refused: None,
            };
            let adoption = (|| {
                require(
                    !view.plan.metadata.source.observation.dirty
                        && !info.source.dirty
                        && info.source.head_commit
                            == view.plan.metadata.source.observation.head_commit,
                    "SOURCE_DRIFT: adopting the plan requires the plan's clean committed baseline; reprepare/replan dirty work",
                )?;
                require(
                    self.store.index_status(root)?.fresh,
                    "SOURCE_DRIFT: graph/source assumptions changed; replan",
                )?;
                // The plan must have been prepared against the accepted
                // ontology, and the index must still materialize it.
                graph::require_accepted(
                    &self.store.connection,
                    &info,
                    view.plan.metadata.source.graph_generation.as_ref(),
                )
                .map_err(|e| Error::Invalid(format!("SOURCE_DRIFT: {e}")))?;
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
                self.checkpoint(&info, &run, "SOURCE_DRIFT_DETECTED")?;
                return Err(error);
            }
            self.checkpoint(&info, &run, "PLAN_RUNTIME_STARTED")?;
            run
        };
        require(
            run.engineering_session.as_ref() == Some(&session::for_plan(self.store, &info, id)?),
            "legacy/foreign engineering-session ownership: history is inspectable but resume requires an explicit replan",
        )?;
        if view.state == planning::PlanState::Complete {
            if run.state != RunState::Complete {
                run.state = RunState::Complete;
                self.checkpoint(&info, &run, "PLAN_RUNTIME_COMPLETED")?;
            }
            return Ok(run);
        }
        require(
            run.state == RunState::Running,
            "runtime is blocked/cancelled; explicit planner decision and replacement plan required",
        )?;
        if resumed {
            // A durable marker distinguishes a controller/process recovery from
            // an uninterrupted invocation. It carries no competing state: the
            // existing RunRecord remains the sole resumable checkpoint.
            self.checkpoint(&info, &run, "PLAN_RUNTIME_RESUMED")?;
        }
        let outcome = self.drive(&info, &view.plan, &mut run, &lease);
        if let Err(error) = outcome {
            run.reason = Some(error.to_string().chars().take(1024).collect());
            if run.reconciliation.is_some() {
                // This is a recoverable or explicitly unresolved publication,
                // not a second lifecycle. Keep the sole RunRecord resumable;
                // drive() will do nothing except recover/refuse this intent on
                // the next invocation.
                run.state = RunState::Running;
                self.checkpoint(&info, &run, "RECONCILIATION_PENDING")?;
                return Err(error);
            }
            // Provider infrastructure (not the work) failed, and the workspace
            // is exactly what this run last recorded: nothing uncertain exists,
            // so the run stays resumable instead of ending. A launch that did
            // not start is relaunched on resume within MAX_EXECUTOR_RELAUNCHES;
            // a pending result is re-verified; integration is re-run.
            if matches!(
                &error,
                Error::Provider {
                    class: OutcomeClass::RetryableProviderFailure
                        | OutcomeClass::NonretryableProviderFailure,
                    ..
                }
            ) && !self.cancelled(&info, Some(id))?
                && self.unchanged_since_recorded(&info, &run)?
            {
                run.state = RunState::Running;
                for task in self.store.tasks(&info.repository_id, Some(id))? {
                    if matches!(task.state, TaskState::Ready | TaskState::Executing) {
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
                self.checkpoint(&info, &run, "PROVIDER_UNAVAILABLE")?;
                return Err(Error::Provider {
                    class: match &error {
                        Error::Provider { class, .. } => *class,
                        _ => unreachable!("matched above"),
                    },
                    detail: format!(
                        "{}; no uncertain change exists, so `agentctl run resume {}` continues when the provider is available",
                        match &error {
                            Error::Provider { detail, .. } => detail.as_str(),
                            _ => "",
                        },
                        id.as_str()
                    ),
                });
            }
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
            self.checkpoint(
                &info,
                &run,
                match run.reason.as_deref().unwrap_or_default() {
                    reason if reason.contains("SOURCE_DRIFT") => "SOURCE_DRIFT_DETECTED",
                    reason if reason.contains("NEEDS_PLANNER_CONTEXT_APPROVAL") => {
                        "NEEDS_PLANNER_CONTEXT_APPROVAL"
                    }
                    _ => "BLOCKED_NEEDS_PLANNER",
                },
            )?;
            return Err(error);
        }
        Ok(run)
    }
    /// Discards a refused executor result and puts the workspace back to the
    /// source this run last accepted. Never an acceptance: task states, the
    /// ontology and dependency locks are untouched, so a rejected task stays
    /// REJECTED and its dependents stay locked. It exists so the operator does
    /// not have to do Git surgery before replanning.
    ///
    /// Safety: only files inside this plan's own write scopes may differ from
    /// the accepted source. Anything else means the tree also carries changes
    /// the control plane never authorized, and restoring would destroy them,
    /// so the command refuses and names them.
    pub fn restore(&mut self, root: &Path, id: &PlanId) -> Result<RunRecord> {
        let info = graph::checked_workspace(self.store, root)?;
        let _lease = self.lease(&info)?;
        let _permit = auth::authorize(
            &self.store.connection,
            &info.repository_id,
            id.as_str(),
            &session::for_plan(self.store, &info, id)?.id,
        )?;
        let mut run = load_run(self.store, &info, id)?.ok_or_else(|| {
            Error::Invalid(format!(
                "no runtime history for {}; there is nothing agentctl captured to discard",
                id.as_str()
            ))
        })?;
        require(
            run.state != RunState::Complete,
            "plan completed; its result is accepted truth and is not discardable",
        )?;
        // Refuses unless this plan is the workspace's own, and its history is
        // readable, before anything is rewritten.
        self.store.execution_plan(root, id)?;
        // The last *verified* source, never `expected`: reconciliation advances
        // `expected` to a published branch before any verifier has accepted it,
        // and rewinding to that would leave the refused work in place.
        let verified = run.verified.clone().ok_or_else(|| {
            Error::Invalid(
                "this run predates verified-source tracking, so the source it last verified is unknown; cancel it with agentctl run cancel and plan the replacement".into(),
            )
        })?;
        let accepted: SourceSnapshot = self.artifacts.decode(&verified)?;
        let current = self.capture(&info)?;
        require(
            current != accepted,
            "workspace already equals the last verified source; nothing to discard",
        )?;
        // Exactly the files agentctl watched this plan's executor change, and
        // nothing else, may be rewritten.
        let captured = run
            .refused
            .clone()
            .or_else(|| run.pending.as_ref().map(|pending| pending.diff.clone()))
            .ok_or_else(|| {
                Error::Invalid(
                    "this run holds no captured executor result to discard, so nothing it recorded accounts for the current workspace; resolve those changes explicitly".into(),
                )
            })?;
        let captured: CapturedDiff = self.artifacts.decode(&captured)?;
        require(
            captured.plan_id == *id,
            "captured result belongs to another plan",
        )?;
        let executor_paths: BTreeSet<&String> =
            captured.changes.iter().map(|change| &change.path).collect();
        let unauthorized: Vec<&String> = current
            .files
            .keys()
            .chain(accepted.files.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|path| current.files.get(*path) != accepted.files.get(*path))
            .filter(|path| !executor_paths.contains(*path))
            .collect();
        require(
            unauthorized.is_empty(),
            format!(
                "workspace also differs from the last verified source in files this plan's executor never wrote ({}); resolve those changes explicitly before discarding",
                unauthorized
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )?;
        concurrency::materialize(&info.root, &accepted, &self.artifacts)?;
        // Rewriting tracked files does not reproduce the Git index's own hash,
        // which is why `materialize` normalizes it; assert the same equality it
        // guarantees rather than a stricter one that can never hold.
        let mut observed = self.capture(&info)?;
        observed.index_hash = accepted.index_hash.clone();
        require(
            observed == accepted,
            "workspace could not be restored to the last verified source",
        )?;
        run.pending = None;
        run.refused = None;
        // The tree is now the verified source, so the run's expected source is
        // that too: leaving it on the discarded publication would make every
        // later drift check compare against source that no longer exists.
        run.expected = verified;
        run.state = RunState::Blocked;
        run.reason =
            Some("refused executor result discarded; a replacement plan is required".into());
        self.checkpoint(&info, &run, "REFUSED_RESULT_DISCARDED")?;
        Ok(run)
    }
    /// Consumes a planner's explicit decision on an escalated context request.
    /// This is the only path that can widen a task's relay envelope: executor
    /// output has no route to it, and an approval is revalidated (policy,
    /// source, ontology generation, request scope, exclusions) and re-resolved
    /// before any delta exists. Denial leaves the task blocked.
    pub fn decide_context(
        &mut self,
        root: &Path,
        decision: &context::ContextDecision,
    ) -> Result<RunRecord> {
        decision.check()?;
        let info = graph::checked_workspace(self.store, root)?;
        let _lease = self.lease(&info)?;
        let plan = decision.plan_id.clone();
        let _permit = auth::authorize(
            &self.store.connection,
            &info.repository_id,
            plan.as_str(),
            &session::for_plan(self.store, &info, &plan)?.id,
        )?;
        let mut run = load_run(self.store, &info, &plan)?
            .ok_or_else(|| Error::Invalid("runtime plan not found".into()))?;
        let key = context::subject_key(AgentRole::Executor, Some(&decision.task_id));
        let ledger = run
            .context
            .get(&key)
            .cloned()
            .ok_or_else(|| Error::Invalid("task has no context relay ledger".into()))?;
        require(
            ledger.state == context::LedgerState::NeedsPlannerContextApproval,
            "no escalated context request awaits a decision for this task",
        )?;
        let round = ledger
            .rounds
            .last()
            .cloned()
            .ok_or_else(|| Error::Invalid("escalated ledger has no round".into()))?;
        require(
            round.outcome == context::RoundOutcome::Escalated
                && round.request.hash == decision.request_hash,
            "decision names another context request",
        )?;
        let decision_ref = self.artifacts.json(decision)?;
        if decision.decision == context::DecisionKind::Deny {
            let entry = run.context.get_mut(&key).expect("issued ledger");
            entry.state = context::LedgerState::PlannerDenied;
            if let Some(last) = entry.rounds.last_mut() {
                last.outcome = context::RoundOutcome::PlannerDenied;
                last.decision = Some(decision_ref);
            }
            run.state = RunState::Blocked;
            run.reason = Some(format!(
                "CONTEXT_ESCALATION_DENIED: {}",
                decision.reason.chars().take(512).collect::<String>()
            ));
            self.checkpoint(&info, &run, "CONTEXT_ESCALATION_DENIED")?;
            event(
                self.store,
                &info,
                Some(&plan),
                Some(&round.job_id),
                "CONTEXT_ESCALATION_DENIED",
                &json!({"subject":key,"actor":decision.actor}).to_string(),
            )?;
            return Ok(run);
        }
        let view = self.store.execution_plan(root, &plan)?;
        let task = self
            .store
            .execution_tasks(root, &plan)?
            .into_iter()
            .find(|t| t.packet.task_id == decision.task_id)
            .ok_or_else(|| Error::Invalid("decision names an unknown task".into()))?;
        let policy = ProjectConfig::load(&info.root)?;
        require(
            planning::hash(&policy)? == run.policy_hash,
            "SOURCE_DRIFT: canonical policy changed; replan required",
        )?;
        let current = self.expected(&info, &run.expected)?;
        let base = ledger
            .base
            .clone()
            .ok_or_else(|| Error::Invalid("escalated ledger has no issued base".into()))?;
        self.revalidate(&info, &base, &current)?;
        let prepared = self
            .store
            .planning_context(root, &view.plan.metadata.request_id)?;
        for scope in &decision.read_scope_additions {
            planning::safe_scope(&info.root, &policy, scope, false)?;
            require(
                prepared.request.intent.scope.is_empty()
                    || prepared
                        .request
                        .intent
                        .scope
                        .iter()
                        .any(|parent| permits(parent, scope.path())),
                "approved scope exceeds the planning request's own scope",
            )?;
            require(
                !task.contract.exclusions.iter().any(|excluded| {
                    permits(excluded, scope.path()) || permits(scope, excluded.path())
                }),
                "approved scope contradicts a verification contract exclusion",
            )?;
        }
        let envelope = context::Envelope {
            scopes: [
                task.packet.read_scope.clone(),
                ledger.approved_scope.clone(),
                decision.read_scope_additions.clone(),
            ]
            .concat(),
            memory: task.contract.memory_refs.clone(),
        };
        let request: ContextRequest = self.artifacts.decode(&round.request)?;
        request.validate()?;
        let limits = self.config.context;
        let budget = context::Budget {
            request_max_bytes: request.max_bytes as usize,
            round_limit: (request.max_bytes as usize).min(limits.max_round_bytes as usize),
            task_remaining: (limits.max_task_bytes as usize).saturating_sub(ledger.granted_bytes),
            rounds_used: ledger.rounds_used(),
            max_rounds: limits.max_rounds,
            escalations_used: 0,
            max_escalations: 0,
        };
        let (resolution, items) = context::resolve(
            &*self.store,
            &info,
            &self.artifacts,
            &current,
            &key,
            base.graph_generation.as_ref(),
            &envelope,
            &request,
            &round.request.hash,
            budget,
        )?;
        require(
            resolution.verdict == context::Verdict::Granted,
            format!(
                "the approved scope still does not resolve this request within budget ({:?}); deny it or replan",
                resolution.code
            ),
        )?;
        let delta = context::delta(
            &plan,
            Some(&decision.task_id),
            AgentRole::Executor,
            &round.job_id,
            ledger.rounds_used() + 1,
            &resolution,
            context::Grant::PlannerApproved {
                decision_hash: decision_ref.hash.clone(),
                scope_additions: decision.read_scope_additions.clone(),
            },
            items,
        )?;
        let reference = context::DeltaRef {
            delta_id: delta.delta_id.clone(),
            artifact: self.artifacts.json(&delta)?,
            bytes: delta.bytes,
        };
        let resolution_ref = self.artifacts.json(&resolution)?;
        let entry = run.context.get_mut(&key).expect("issued ledger");
        entry.state = context::LedgerState::Open;
        entry
            .approved_scope
            .extend(decision.read_scope_additions.clone());
        entry.granted_bytes += delta.bytes;
        if let Some(last) = entry.rounds.last_mut() {
            last.outcome = context::RoundOutcome::Approved;
            last.decision = Some(decision_ref);
            last.resolution = resolution_ref;
            last.delta = Some(reference);
        }
        // Back to PLANNED so `run resume` re-issues the task as a fresh job.
        if self
            .store
            .task(&info.repository_id, &decision.task_id)?
            .is_some_and(|t| t.state == TaskState::Blocked)
        {
            self.store.transition_task(
                &info.repository_id,
                &decision.task_id,
                TaskState::Blocked,
                TaskState::Planned,
                None,
                now_ms()?,
            )?;
        }
        run.state = RunState::Running;
        run.reason = None;
        self.checkpoint(&info, &run, "CONTEXT_ESCALATION_APPROVED")?;
        event(
            self.store,
            &info,
            Some(&plan),
            Some(&round.job_id),
            "CONTEXT_ESCALATION_APPROVED",
            &json!({"subject":key,"delta":delta.delta_id,"bytes":delta.bytes,"actor":decision.actor,"additions":decision.read_scope_additions.len()}).to_string(),
        )?;
        Ok(run)
    }
    /// Stops a plan the controller owns, for good. A running plan is asked to
    /// stop (the controller notices and winds down); a plan that has already
    /// stopped is cancelled outright under controller authorization, which is
    /// the only way to free the workspace for a replacement plan once runtime
    /// history exists.
    pub fn cancel(&mut self, root: &Path, id: &PlanId) -> Result<RunState> {
        let info = graph::checked_workspace(self.store, root)?;
        let _lease = self.lease(&info)?;
        let mut run = load_run(self.store, &info, id)?
            .ok_or_else(|| Error::Invalid("runtime plan not found".into()))?;
        if run.state == RunState::Running {
            self.store.runtime_cancel(root, id)?;
            return Ok(RunState::Running);
        }
        require(
            run.state != RunState::Complete,
            "plan completed; its result is accepted truth and is not cancellable",
        )?;
        let _permit = auth::authorize(
            &self.store.connection,
            &info.repository_id,
            id.as_str(),
            &session::for_plan(self.store, &info, id)?.id,
        )?;
        for task in self.store.tasks(&info.repository_id, Some(id))? {
            // Verified, Rejected and Blocked are settled; the rest are in flight
            // and are stopped so no dependent can ever read them as runnable.
            if !matches!(
                task.state,
                TaskState::Verified | TaskState::Rejected | TaskState::Blocked
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
        self.store.cancel_execution_plan_owned(
            &info,
            id,
            "cancelled by the controller after the run stopped",
        )?;
        run.state = RunState::Cancelled;
        run.reason = Some("plan cancelled; a replacement plan is required".into());
        self.checkpoint(&info, &run, "PLAN_RUNTIME_CANCELLED")?;
        Ok(RunState::Cancelled)
    }
    /// Explicit human/orchestrator decision; never an automatic retry. Plan
    /// validation still checks the replacement and retains all prior proof/history.
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
    fn fork_adapters(&self) -> Option<BTreeMap<String, Box<dyn ProviderAdapter>>> {
        self.adapters
            .iter()
            .map(|(name, adapter)| adapter.fork().map(|fork| (name.clone(), fork)))
            .collect()
    }

    /// Re-prove and claim the entire batch at one authority boundary. The
    /// workspace lease excludes another controller; BEGIN IMMEDIATE excludes
    /// concurrent database/ontology writers. Canonical source is captured
    /// inside that transaction, and the authority record plus every task claim
    /// commit together.
    fn authorize_batch(
        &mut self,
        info: &RepositoryInfo,
        plan: &planning::ExecutionPlan,
        run: &mut RunRecord,
        tasks: &[planning::TaskInspection],
    ) -> Result<(
        SourceSnapshot,
        String,
        Vec<concurrency::CompatibilityDecision>,
    )> {
        let mut compatibility = vec![];
        for (index, left) in tasks.iter().enumerate() {
            for right in &tasks[index + 1..] {
                let decision =
                    concurrency::decide(self.store, info, plan, &left.packet, &right.packet)?;
                require(
                    decision.decision == concurrency::Compatibility::Compatible,
                    "STALE_CONCURRENCY_AUTHORITY: pairwise compatibility is no longer proven",
                )?;
                compatibility.push(decision);
            }
        }
        let expected: SourceSnapshot = self.artifacts.decode(&run.expected)?;
        self.observe_boundary("batch_revalidated");
        let tx = self
            .store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = source::capture_bound(
            &info.root,
            &self.artifacts,
            &info.repository_id,
            &info.workspace_id,
        )?;
        require(
            current == expected,
            "STALE_CONCURRENCY_AUTHORITY: canonical source changed before batch claim",
        )?;
        let graph_generation = graph::generation(&tx, info)?.ok_or_else(|| {
            Error::Invalid("STALE_CONCURRENCY_AUTHORITY: accepted ontology is unavailable".into())
        })?;
        require(
            compatibility.iter().all(|decision| {
                decision.reasons.iter().any(|reason| {
                    matches!(reason, concurrency::CompatibilityReason::OntologyProvesNoInterference { generation } if generation == &graph_generation)
                })
            }),
            "STALE_CONCURRENCY_AUTHORITY: compatibility does not bind the current ontology",
        )?;
        let generation: String = tx.query_row(
            "SELECT generation_id FROM ontology_generations WHERE workspace_id=?1 AND sequence=?2 AND fingerprint=?3 ORDER BY ordinal DESC LIMIT 1",
            params![info.workspace_id.as_str(), graph_generation.sequence as i64, graph_generation.fingerprint],
            |row| row.get(0),
        )?;
        let plan_state: String = tx.query_row(
            "SELECT state FROM execution_plans WHERE repo_id=?1 AND plan_id=?2 AND workspace_id=?3",
            params![
                info.repository_id.as_str(),
                run.plan_id.as_str(),
                info.workspace_id.as_str()
            ],
            |row| row.get(0),
        )?;
        require(
            plan_state == "ACTIVE",
            "STALE_CONCURRENCY_AUTHORITY: execution plan is no longer active",
        )?;
        let cancelled: bool = tx.query_row(
            "SELECT cancel_requested FROM runtime_runs WHERE repo_id=?1 AND plan_id=?2 AND workspace_id=?3",
            params![info.repository_id.as_str(), run.plan_id.as_str(), info.workspace_id.as_str()],
            |row| row.get(0),
        )?;
        require(
            !cancelled,
            "STALE_CONCURRENCY_AUTHORITY: runtime was cancelled before batch claim",
        )?;
        let mut states = store::task_states(&tx, &info.repository_id, &run.plan_id)?;
        for task in tasks {
            let id = &task.packet.task_id;
            let mut state = *states
                .get(id)
                .ok_or_else(|| Error::Invalid("batch task state disappeared".into()))?;
            if state == TaskState::Planned {
                plan.packet
                    .validate_task_transition(id, &states, TaskState::Ready, None)?;
                tx.execute(
                    "UPDATE tasks SET state_json='\"READY\"' WHERE repo_id=?1 AND task_id=?2 AND state_json='\"PLANNED\"'",
                    params![info.repository_id.as_str(), id.as_str()],
                )?;
                store::append(
                    &tx,
                    &info.repository_id,
                    now_ms()?,
                    &Links::planning_task(
                        info.workspace_id.clone(),
                        run.plan_id.clone(),
                        id.clone(),
                    ),
                    None,
                    &JournalEntry::TaskStateChanged {
                        from: TaskState::Planned,
                        to: TaskState::Ready,
                        verification: None,
                    },
                )?;
                states.insert(id.clone(), TaskState::Ready);
                state = TaskState::Ready;
            }
            require(
                state == TaskState::Ready,
                "STALE_CONCURRENCY_AUTHORITY: selected task is no longer READY",
            )?;
            plan.packet
                .validate_task_transition(id, &states, TaskState::Executing, None)?;
            tx.execute(
                "UPDATE tasks SET state_json='\"EXECUTING\"' WHERE repo_id=?1 AND task_id=?2 AND state_json='\"READY\"'",
                params![info.repository_id.as_str(), id.as_str()],
            )?;
            store::append(
                &tx,
                &info.repository_id,
                now_ms()?,
                &Links::planning_task(info.workspace_id.clone(), run.plan_id.clone(), id.clone()),
                None,
                &JournalEntry::TaskStateChanged {
                    from: TaskState::Ready,
                    to: TaskState::Executing,
                    verification: None,
                },
            )?;
            states.insert(id.clone(), TaskState::Executing);
        }
        let source_ref = self.artifacts.json(&current)?;
        run.batch_authority = Some(BatchLaunchAuthority {
            source: source_ref,
            ontology_generation: generation.clone(),
            tasks: tasks
                .iter()
                .map(|task| task.packet.task_id.clone())
                .collect(),
            compatibility: compatibility.clone(),
        });
        tx.execute(
            "UPDATE runtime_runs SET record_json=?1 WHERE repo_id=?2 AND plan_id=?3",
            params![
                serde_json::to_string(run)?,
                info.repository_id.as_str(),
                run.plan_id.as_str()
            ],
        )?;
        store::append(
            &tx,
            &info.repository_id,
            now_ms()?,
            &Links::planning(info.workspace_id.clone(), Some(run.plan_id.clone())),
            None,
            &JournalEntry::Runtime {
                job_id: None,
                phase: "CONCURRENT_BATCH_AUTHORIZED".into(),
                detail: generation.clone(),
            },
        )?;
        tx.commit()?;
        Ok((current, generation, compatibility))
    }

    /// Execute a proven-compatible READY set in isolated Git worktrees. Only
    /// executor jobs overlap; captured results are reconciled and verified one
    /// at a time by `drive`, preserving the existing acceptance boundary.
    fn execute_batch(
        &mut self,
        info: &RepositoryInfo,
        plan: &planning::ExecutionPlan,
        run: &mut RunRecord,
        tasks: &[planning::TaskInspection],
        lease: &WorkspaceLease,
    ) -> Result<()> {
        let baseline = self.expected(info, &run.expected)?;
        self.observe_boundary("batch_selected");
        let mut launches = vec![];
        let prepare = (|| -> Result<()> {
            for task in tasks {
                let mut compatibility = vec![];
                for other in tasks {
                    if task.packet.task_id < other.packet.task_id {
                        compatibility.push(concurrency::decide(
                            self.store,
                            info,
                            plan,
                            &task.packet,
                            &other.packet,
                        )?);
                    } else if task.packet.task_id > other.packet.task_id {
                        compatibility.push(concurrency::decide(
                            self.store,
                            info,
                            plan,
                            &other.packet,
                            &task.packet,
                        )?);
                    }
                }
                require(
                    !compatibility.is_empty()
                        && compatibility.iter().all(|decision| {
                            decision.decision == concurrency::Compatibility::Compatible
                        }),
                    "UNKNOWN: batch contains a task without positive pairwise compatibility evidence",
                )?;
                let adapters = self
                    .fork_adapters()
                    .ok_or_else(|| Error::Invalid("provider adapter cannot fork safely".into()))?;
                let (workspace, branch_info) = concurrency::create_workspace(
                    self.store,
                    &self.paths,
                    info,
                    &run.plan_id,
                    &task.packet.task_id,
                    &baseline,
                    &self.artifacts,
                )?;
                launches.push((
                    task.clone(),
                    workspace,
                    branch_info,
                    compatibility,
                    adapters,
                ));
            }
            Ok(())
        })();
        if let Err(error) = prepare {
            for (_, workspace, _, _, _) in &launches {
                let _ = concurrency::remove_workspace(info, &workspace.root);
            }
            return Err(error);
        }
        self.observe_boundary("batch_preclaim");
        let authority = self.authorize_batch(info, plan, run, tasks);
        let (_, generation, fresh_compatibility) = match authority {
            Ok(authority) => authority,
            Err(error) => {
                for (_, workspace, _, _, _) in &launches {
                    let _ = concurrency::remove_workspace(info, &workspace.root);
                }
                return Err(error);
            }
        };
        for (_, _, _, compatibility, _) in &mut launches {
            *compatibility = fresh_compatibility.clone();
        }
        self.observe_boundary("batch_authorized");
        let database = self
            .store
            .connection
            .path()
            .ok_or_else(|| Error::Invalid("runtime database has no durable path".into()))?
            .to_owned();
        let paths = self.paths.clone();
        let config = self.config.clone();
        let overrides = self.overrides.clone();
        let plan_id = run.plan_id.clone();
        let base_run = run.clone();
        let results = std::thread::scope(|scope| {
            let mut handles = vec![];
            for (task, workspace, branch_info, compatibility, adapters) in launches {
                let database = database.clone();
                let paths = paths.clone();
                let config = config.clone();
                let overrides = overrides.clone();
                let plan_id = plan_id.clone();
                let mut branch_run = base_run.clone();
                let generation = generation.clone();
                let canonical_index = baseline.index_hash.clone();
                handles.push((
                    task.packet.task_id.clone(),
                    workspace.clone(),
                    scope.spawn(move || -> Result<()> {
                        let mut store = Store::open(Path::new(&database), 5000)?;
                        let authority_info = info.clone();
                        let session = session::for_plan(&store, &authority_info, &plan_id)?;
                        let _permit = auth::authorize(
                            &store.connection,
                            &authority_info.repository_id,
                            plan_id.as_str(),
                            &session.id,
                        )?;
                        Runtime::new(&mut store, paths, config, adapters)?
                            .with_role_overrides(overrides)?
                            .for_branch(
                                workspace.root.clone(),
                                compatibility,
                                generation,
                                workspace.physical_index_hash.clone(),
                                canonical_index,
                                authority_info,
                            )
                            .execute_task(&branch_info, &mut branch_run, &task, lease)
                    }),
                ));
            }
            handles
                .into_iter()
                .map(|(task, workspace, handle)| {
                    (
                        task,
                        workspace,
                        handle.join().unwrap_or_else(|_| {
                            Err(Error::Invalid("concurrent executor thread panicked".into()))
                        }),
                    )
                })
                .collect::<Vec<_>>()
        });
        for (task, workspace, result) in results {
            if let Err(error) = result {
                if let Some(stored) = self.store.task(&info.repository_id, &task)?
                    && matches!(
                        stored.state,
                        TaskState::Executing | TaskState::AwaitingVerification
                    )
                {
                    self.store.transition_task(
                        &info.repository_id,
                        &task,
                        stored.state,
                        TaskState::Blocked,
                        None,
                        now_ms()?,
                    )?;
                }
                event(
                    self.store,
                    info,
                    Some(&run.plan_id),
                    None,
                    "BRANCH_FAILED",
                    &format!("{}: {error}", task.as_str()),
                )?;
                // A branch that produced no captured result owns nothing the
                // control plane can still use: the journal holds the failure,
                // and the worktree would otherwise accumulate forever.
                let _ = concurrency::remove_workspace(info, &workspace.root);
                let _ = std::fs::remove_dir_all(&workspace.root);
            }
        }
        *run = load_run(self.store, info, &run.plan_id)?.ok_or_else(|| {
            Error::Invalid("runtime run disappeared after branch execution".into())
        })?;
        run.batch_authority = None;
        self.checkpoint(info, run, "CONCURRENT_BATCH_FINISHED")?;
        Ok(())
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
        // Managed worktrees this run's durable state no longer refers to are
        // abandoned: the exclusive workspace lease means nothing else can own
        // them. Reclaim them before anything else so a crashed branch cannot
        // accumulate, and so Git's registrations stay accurate for the
        // `worktree add` below.
        self.reclaim_worktrees(info, run)?;
        // Publication recovery precedes job interruption handling, graph
        // refresh, verification, dependency release, and every new launch.
        if run.reconciliation.is_some() {
            self.recover_reconciliation(info, run)?;
        }
        let mut fatal_interruption = false;
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
                if let Some(canonical) = self.store.job(&info.repository_id, &job.job_id)?
                    && !canonical.state.is_terminal()
                {
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
                save_job(self.store, info, &job, "JOB_INTERRUPTED")?;
                if job.role == AgentRole::Executor {
                    if let Some(task_id) = &job.task_id
                        && let Some(task) = self.store.task(&info.repository_id, task_id)?
                        && matches!(task.state, TaskState::Ready | TaskState::Executing)
                    {
                        self.store.transition_task(
                            &info.repository_id,
                            task_id,
                            task.state,
                            TaskState::Blocked,
                            None,
                            now_ms()?,
                        )?;
                    }
                    event(
                        self.store,
                        info,
                        Some(&run.plan_id),
                        Some(&job.job_id),
                        "BRANCH_INTERRUPTED",
                        "executor branch contained; durable sibling results remain valid",
                    )?;
                } else {
                    // A task/integration verifier interruption sits on the
                    // serialized acceptance path. Its absence proves no
                    // decision, so resumption requires explicit review.
                    fatal_interruption = true;
                }
            }
        }
        require(
            !fatal_interruption,
            "interrupted job requires explicit planner review",
        )?;
        // Repair only the derived graph if a crash occurred after the durable
        // VERIFIED checkpoint and before its refresh. Recheck source first.
        if run.pending.is_none() && !run.accepted.is_empty() {
            self.expected(info, &run.expected)?;
            let origin = graph::GenerationOrigin::Runtime {
                plan_id: run.plan_id.clone(),
                task_id: None,
            };
            require(
                self.store.index_observed(&info.root, &origin)?.failed == 0,
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
            if let Some((task_id, mut pending)) = run
                .branches
                .iter()
                .next()
                .map(|(task, pending)| (task.clone(), pending.clone()))
            {
                let task = plan
                    .packet
                    .tasks
                    .iter()
                    .find(|task| task.task_id == task_id)
                    .ok_or_else(|| Error::Invalid("branch task is absent from plan".into()))?;
                let generation = pending.ontology_generation.as_deref().ok_or_else(|| {
                    Error::Invalid("UNKNOWN: branch lacks issued ontology generation".into())
                })?;
                if let Err(error) =
                    concurrency::revalidate_semantic(self.store, info, task, generation)
                {
                    // What verified siblings changed can no longer be proven
                    // harmless to this branch (or provably is not). Its result
                    // is not published; the task runs again, serially, against
                    // the current accepted source. Nothing is accepted on the
                    // way, and verified work stays verified.
                    let message = error.to_string();
                    let key = context::subject_key(AgentRole::Executor, Some(&task_id));
                    let clean_ledger = run.context.get(&key).is_none_or(|ledger| {
                        ledger.rounds_used() == 0 && ledger.approved_scope.is_empty()
                    });
                    if !(message.starts_with("UNKNOWN:")
                        || message.starts_with("SEMANTIC_INTERFERENCE:"))
                        || !clean_ledger
                    {
                        return Err(error);
                    }
                    self.serialize_branch(info, run, &task_id, &pending, &message)?;
                    continue;
                }
                let current = self.expected(info, &run.expected)?;
                let intent =
                    concurrency::reconciliation_intent(task, &pending, &current, &self.artifacts)?;
                run.reconciliation = Some(intent);
                self.checkpoint(info, run, "RECONCILIATION_INTENT")?;
                self.observe_boundary("reconciliation_intent_durable");
                self.recover_reconciliation(info, run)?;
                pending = run.pending.clone().expect("recovered pending task");
                self.verify_pending(info, plan, run, lease)?;
                if let Some(root) = &pending.execution_workspace {
                    concurrency::remove_workspace(info, root)?;
                }
                continue;
            }
            let current = self.expected(info, &run.expected)?;
            let tasks = self.store.execution_tasks(&info.root, &run.plan_id)?;
            if tasks.iter().all(|t| t.state == TaskState::Verified) {
                return self.integrate(info, plan, run, &current, lease);
            }
            if !tasks.iter().any(|task| task.structurally_ready)
                && self.relaunch(info, run, &tasks)?
            {
                continue;
            }
            let ready: Vec<_> = tasks
                .into_iter()
                .filter(|task| task.structurally_ready)
                .collect();
            let task = ready.first().cloned().ok_or_else(|| {
                Error::Invalid(
                    "no runnable task; unverified/rejected/interrupted work requires planner decision"
                        .into(),
                )
            })?;
            let project = ProjectConfig::load(&info.root)?;
            let limit = self.config.effective_max_agents(&project.routing);
            if limit > 1 && self.fork_adapters().is_some() {
                let mut batch = vec![task.clone()];
                'candidate: for candidate in ready.into_iter().skip(1) {
                    if batch.len() >= limit {
                        break;
                    }
                    for selected in &batch {
                        let decision = concurrency::decide(
                            self.store,
                            info,
                            plan,
                            &selected.packet,
                            &candidate.packet,
                        )?;
                        if decision.decision != concurrency::Compatibility::Compatible {
                            continue 'candidate;
                        }
                    }
                    batch.push(candidate);
                }
                if batch.len() > 1 {
                    self.execute_batch(info, plan, run, &batch, lease)?;
                    continue;
                }
            }
            self.execute_task(info, run, &task, lease)?;
        }
    }
    /// Returns a blocked task to the schedulable set when its launch provably
    /// mutated nothing: the workspace still equals this run's expected source,
    /// no captured result names it, and its context relay is still open. That
    /// is the whole safety argument — a controller crash, a provider timeout or
    /// a malformed reply that left the tree byte-identical cannot have produced
    /// work worth keeping, so relaunching reuses no unverified mutation.
    ///
    /// A rejected task, a deterministic check failure and any executor that
    /// did touch the tree all leave the workspace different from `expected`
    /// (or a state other than BLOCKED) and are never relaunched here.
    fn relaunch(
        &mut self,
        info: &RepositoryInfo,
        run: &mut RunRecord,
        tasks: &[planning::TaskInspection],
    ) -> Result<bool> {
        if run.pending.is_some() || !run.branches.is_empty() {
            return Ok(false);
        }
        // Proof obligation: the tree is exactly what it was before the launch.
        if self.capture(info)? != self.artifacts.decode::<SourceSnapshot>(&run.expected)? {
            return Ok(false);
        }
        let Some(task) = tasks.iter().find(|task| {
            task.state == TaskState::Blocked
                && !run.accepted.contains_key(&task.packet.task_id)
                && task
                    .packet
                    .dependencies
                    .iter()
                    .all(|dependency| run.accepted.contains_key(dependency))
                && run
                    .context
                    .get(&context::subject_key(
                        AgentRole::Executor,
                        Some(&task.packet.task_id),
                    ))
                    .is_none_or(|ledger| ledger.state == context::LedgerState::Open)
        }) else {
            return Ok(false);
        };
        let id = task.packet.task_id.clone();
        let spent = run.executor_relaunches.entry(id.clone()).or_default();
        if *spent >= MAX_EXECUTOR_RELAUNCHES {
            return Ok(false);
        }
        *spent += 1;
        let attempt = *spent;
        self.store.transition_task(
            &info.repository_id,
            &id,
            TaskState::Blocked,
            TaskState::Planned,
            None,
            now_ms()?,
        )?;
        event(
            self.store,
            info,
            Some(&run.plan_id),
            None,
            "EXECUTOR_RELAUNCHED",
            &format!(
                "{}: attempt {attempt} of {MAX_EXECUTOR_RELAUNCHES}; workspace proven unchanged since the issued baseline",
                id.as_str()
            ),
        )?;
        self.checkpoint(info, run, "EXECUTOR_RELAUNCHED")?;
        Ok(true)
    }
    /// Withdraws a captured concurrent branch whose result can no longer be
    /// published safely, and returns its task to PLANNED so it executes again,
    /// serially, on the current canonical source. The durable record changes
    /// first; the worktree is then dropped (reclaimed later if that fails).
    fn serialize_branch(
        &mut self,
        info: &RepositoryInfo,
        run: &mut RunRecord,
        task_id: &TaskId,
        pending: &PendingTask,
        reason: &str,
    ) -> Result<()> {
        run.branches.remove(task_id);
        run.context
            .remove(&context::subject_key(AgentRole::Executor, Some(task_id)));
        self.checkpoint(info, run, "BRANCH_SERIALIZED")?;
        for (from, to) in [
            (TaskState::AwaitingVerification, TaskState::Blocked),
            (TaskState::Blocked, TaskState::Planned),
        ] {
            self.store
                .transition_task(&info.repository_id, task_id, from, to, None, now_ms()?)?;
        }
        if let Some(root) = &pending.execution_workspace {
            let _ = concurrency::remove_workspace(info, root);
        }
        event(
            self.store,
            info,
            Some(&run.plan_id),
            Some(&pending.executor),
            "BRANCH_SERIALIZED",
            &format!(
                "{}: captured branch withdrawn, re-executing serially ({})",
                task_id.as_str(),
                reason.chars().take(300).collect::<String>()
            ),
        )
    }
    /// Reclaims every managed worktree of this workspace that the durable run
    /// record does not still own. A captured branch and an unresolved
    /// reconciliation intent are the only things that keep one alive.
    fn reclaim_worktrees(&mut self, info: &RepositoryInfo, run: &RunRecord) -> Result<()> {
        let keep: BTreeSet<String> = run
            .branches
            .values()
            .chain(run.pending.iter())
            .filter_map(|pending| pending.execution_workspace.clone())
            .collect();
        let removed = concurrency::prune_workspaces(&self.paths, info, &keep);
        if !removed.is_empty() {
            event(
                self.store,
                info,
                Some(&run.plan_id),
                None,
                "WORKTREES_RECLAIMED",
                &serde_json::to_string(&removed)?,
            )?;
        }
        Ok(())
    }
    fn recover_reconciliation(&mut self, info: &RepositoryInfo, run: &mut RunRecord) -> Result<()> {
        let intent = run
            .reconciliation
            .clone()
            .ok_or_else(|| Error::Invalid("reconciliation intent missing".into()))?;
        let mut pending =
            run.branches.get(&intent.task_id).cloned().ok_or_else(|| {
                Error::Invalid("reconciliation intent has no captured branch".into())
            })?;
        require(
            pending.executor == intent.executor && pending.diff == intent.diff,
            "RECONCILIATION_UNRESOLVED: durable intent does not match captured branch",
        )?;
        let reconciled = concurrency::publish_reconciliation(&info.root, &intent, &self.artifacts)?;
        self.observe_boundary("reconciliation_files_complete");
        pending.after = self.artifacts.json(&reconciled)?;
        // Publication refuses unless it reached `intended_source`, so these two
        // identities are exactly "canonical before" and "canonical after this
        // branch". Retaining the before is what lets the task verifier see the
        // chain instead of inferring it.
        pending.reconciled_from = Some(intent.expected_source.clone());
        run.branches.remove(&intent.task_id);
        run.pending = Some(pending);
        run.expected = intent.intended_source;
        run.reconciliation = None;
        self.checkpoint(info, run, "BRANCH_RECONCILED")
    }
    /// Runs one ready task to a captured result. While the executor reports
    /// CONTEXT_REQUIRED and the relay grants it, each round is a brand-new
    /// provider job issued the same base context plus the approved deltas.
    fn execute_task(
        &mut self,
        info: &RepositoryInfo,
        run: &mut RunRecord,
        task: &planning::TaskInspection,
        lease: &WorkspaceLease,
    ) -> Result<()> {
        let id = task.packet.task_id.clone();
        let key = context::subject_key(AgentRole::Executor, Some(&id));
        let limits = self.config.context;
        loop {
            let current = self.expected(info, &run.expected)?;
            let ledger = run
                .context
                .entry(key.clone())
                .or_insert_with(|| {
                    context::ContextLedger::new(AgentRole::Executor, Some(id.clone()))
                })
                .clone();
            require(
                ledger.state == context::LedgerState::Open,
                format!(
                    "task context relay is {:?}; an explicit planner decision is required (agentctl run context)",
                    ledger.state
                ),
            )?;
            let envelope = context::Envelope {
                scopes: [
                    task.packet.read_scope.clone(),
                    ledger.approved_scope.clone(),
                ]
                .concat(),
                memory: task.contract.memory_refs.clone(),
            };
            let base = match &ledger.base {
                Some(base) => {
                    self.revalidate(info, base, &current)?;
                    self.artifacts.decode(&base.artifact)?
                }
                None => {
                    graph::require_issuable(&self.store.connection, info, &run.plan_id)?;
                    let authority = self.authority_info.as_ref().unwrap_or(info);
                    let built = context::base_executor(
                        &*self.store,
                        authority,
                        &self.artifacts,
                        &current,
                        task,
                        &envelope,
                    )?;
                    let base = context::BaseRef {
                        artifact: self.artifacts.json(&built)?,
                        graph_generation: built.graph_generation.clone(),
                        source_hash: built.source_hash.clone(),
                    };
                    run.context.get_mut(&key).expect("issued ledger").base = Some(base);
                    self.checkpoint(info, run, "CONTEXT_BASE_ISSUED")?;
                    built
                }
            };
            let ledger = run.context.get(&key).cloned().expect("issued ledger");
            let deltas = self.issued_deltas(&ledger)?;
            let round = ledger.rounds_used();
            let relay = self.relay_state(&ledger, limits.max_rounds);
            let (artifact, inventory) = self.task_input(task, &base, &deltas, relay, round)?;
            self.expected(info, &run.expected)?;
            let state = self
                .store
                .task(&info.repository_id, &id)?
                .ok_or_else(|| Error::Invalid("runnable task disappeared".into()))?
                .state;
            if state == TaskState::Planned {
                self.store.transition_task(
                    &info.repository_id,
                    &id,
                    TaskState::Planned,
                    TaskState::Ready,
                    None,
                    now_ms()?,
                )?;
            }
            if state != TaskState::Executing {
                self.store.transition_task(
                    &info.repository_id,
                    &id,
                    TaskState::Ready,
                    TaskState::Executing,
                    None,
                    now_ms()?,
                )?;
            }
            let previous: BTreeSet<JobId> = self
                .store
                .runtime_jobs(
                    &self.authority_info.as_ref().unwrap_or(info).root,
                    Some(&run.plan_id),
                )?
                .into_iter()
                .map(|j| j.job_id)
                .collect();
            self.revalidate_branch_launch(run, &id)?;
            let invocation = self.invoke(
                info,
                Some(&run.plan_id),
                None,
                Some(&id),
                AgentRole::Executor,
                &current,
                artifact,
                inventory,
                lease,
                &run.policy_hash,
            );
            // A malformed/nonzero provider may still have edited source. Capture
            // that result and scope evidence before surfacing its failure.
            let executor_id = match &invocation {
                Ok((job, _)) => job.job_id.clone(),
                Err(error) => self
                    .store
                    .runtime_jobs(
                        &self.authority_info.as_ref().unwrap_or(info).root,
                        Some(&run.plan_id),
                    )?
                    .into_iter()
                    .filter(|j| j.role == AgentRole::Executor && !previous.contains(&j.job_id))
                    .filter(|j| {
                        self.artifacts
                            .decode::<JobInput>(&j.input)
                            .is_ok_and(|input| input.task_id.as_ref() == Some(&id))
                    })
                    // Retries are fresh jobs; the diff belongs to the last one.
                    .max_by_key(|j| (j.created_at_ms, j.job_id.clone()))
                    .map(|j| j.job_id)
                    .ok_or_else(|| {
                        Error::Invalid(format!(
                            "executor launch failed before issued job was persisted: {error}"
                        ))
                    })?,
            };
            let after = self.capture(info)?;
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
            // Captured, not accepted. Recording it before the scope, exclusion
            // and changed-path checks is what lets `run restore` discard a
            // refused result later without guessing which files were the
            // executor's.
            run.refused = Some(reference.clone());
            let (mut job, value) = invocation?;
            let reported: ResultPacket = serde_json::from_value(value)?;
            if reported.status != ResultStatus::Succeeded && reported.context_request.is_none() {
                // The executor's own, valid answer that it cannot do this task.
                // Its code and summary are the blocking reason; agentctl never
                // replaces a structured diagnosis with a generic refusal.
                let failure = reported.failure.clone().unwrap_or(FailureInfo {
                    code: "UNSPECIFIED".into(),
                    summary: "no failure detail".into(),
                });
                let next = if diff.changes.is_empty() {
                    "no files were changed; correct the plan with a replacement (agentctl run replace)"
                } else {
                    "its changes are retained but not accepted; discard them with agentctl run restore, then correct the plan"
                };
                let status = serde_json::to_value(reported.status)?;
                let reason = format!(
                    "EXECUTOR_{}: {}: {} [task {}, executor job {}]; {next}",
                    status.as_str().unwrap_or("DECLINED"),
                    failure.code,
                    failure.summary.chars().take(600).collect::<String>(),
                    id.as_str(),
                    job.job_id.as_str()
                );
                job.failure = Some(reason.chars().take(1024).collect());
                job.failure_class = Some(OutcomeClass::SemanticRejection);
                save_job(
                    self.store,
                    self.authority_info.as_ref().unwrap_or(info),
                    &job,
                    "EXECUTOR_DECLINED",
                )?;
                return Err(Error::Provider {
                    class: OutcomeClass::SemanticRejection,
                    detail: reason,
                });
            }
            if let Some(request) = &reported.context_request {
                // Fail closed: a context request must not leave unverified work
                // behind, and this runtime has no safe rollback primitive.
                require(
                    diff.changes.is_empty(),
                    "CONTEXT_REQUEST_WITH_EDITS: an executor requesting context must leave the workspace unchanged; its changes are retained but never accepted",
                )?;
                match self.relay(
                    info,
                    run,
                    &key,
                    &envelope,
                    &job,
                    request,
                    &current,
                    limits.max_rounds,
                    limits.max_escalations,
                )? {
                    Relay::Granted => continue,
                    Relay::Escalated => {
                        return Err(Error::Invalid(format!(
                            "NEEDS_PLANNER_CONTEXT_APPROVAL: task {} requested context outside its read scope; decide it with agentctl run context",
                            id.as_str()
                        )));
                    }
                    Relay::Denied(code) => {
                        return Err(Error::Invalid(format!(
                            "CONTEXT_REQUEST_DENIED: {code}; task {} remains blocked",
                            id.as_str()
                        )));
                    }
                }
            }
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
            require(
                reported.changed_paths.iter().collect::<BTreeSet<_>>()
                    == diff.changes.iter().map(|c| &c.path).collect(),
                "executor changed-path report differs from captured files",
            )?;
            let pending = PendingTask {
                task_id: id.clone(),
                executor: job.job_id,
                before: diff.before,
                after: diff.after,
                diff: reference,
                evidence: vec![evidence],
                verifier: None,
                proof: None,
                execution_workspace: self.branch_workspace.clone(),
                compatibility: self.branch_compatibility.clone(),
                ontology_generation: self.branch_generation.clone(),
                // Set only by reconciliation: a result captured here has not
                // been published into canonical source yet, and a serial result
                // never will be.
                reconciled_from: None,
            };
            run.pending = Some(pending.clone());
            if self.persist_run {
                self.checkpoint(info, run, "TASK_EXECUTION_COMPLETED")?;
            } else {
                let ledger = run.context.get(&key).cloned();
                let authority = self.authority_info.as_ref().unwrap_or(info);
                merge_branch(self.store, authority, &id, pending, ledger)?;
            }
            // The durable captured result precedes AWAITING_VERIFICATION. A
            // restart can therefore recover every state at or beyond this
            // transition without inferring from provider success.
            self.store.transition_task(
                &info.repository_id,
                &id,
                TaskState::Executing,
                TaskState::AwaitingVerification,
                None,
                now_ms()?,
            )?;
            return Ok(());
        }
    }
    fn revalidate_branch_launch(&self, run: &RunRecord, task: &TaskId) -> Result<()> {
        let Some(authority) = &self.authority_info else {
            return Ok(());
        };
        let expected: SourceSnapshot = self.artifacts.decode(&run.expected)?;
        let current = source::capture_bound(
            &authority.root,
            &self.artifacts,
            &authority.repository_id,
            &authority.workspace_id,
        )?;
        require(
            current == expected,
            "STALE_CONCURRENCY_AUTHORITY: canonical source changed after batch claim and before executor launch",
        )?;
        require(
            concurrency::live_generation_id(self.store, authority)?
                == self
                    .branch_generation
                    .clone()
                    .ok_or_else(|| Error::Invalid("branch ontology authority missing".into()))?,
            "STALE_CONCURRENCY_AUTHORITY: ontology changed after batch claim and before executor launch",
        )?;
        require(
            self.store
                .task(&authority.repository_id, task)?
                .is_some_and(|stored| stored.state == TaskState::Executing),
            "STALE_CONCURRENCY_AUTHORITY: task claim changed before executor launch",
        )
    }
    /// Re-prove durable lifecycle authority after provider preflight, at the
    /// closest practical boundary before external process creation. SQLite and
    /// the filesystem cannot participate atomically in `adapter.launch`, so a
    /// cancellation or source mutation after this returns is the residual
    /// cooperative race that only OS sandbox enforcement eliminates.
    fn revalidate_launch_at_adapter(
        &mut self,
        info: &RepositoryInfo,
        plan: Option<&PlanId>,
        task: Option<&TaskId>,
        role: AgentRole,
        source: &SourceSnapshot,
    ) -> Result<()> {
        let Some(plan) = plan else {
            return Ok(());
        };
        require(
            self.capture(info)? == *source,
            "SOURCE_DRIFT: launch source changed before external process creation",
        )?;
        let authority = self.authority_info.clone().unwrap_or_else(|| info.clone());
        let tx = self
            .store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (record, cancelled, plan_state): (String, bool, String) = tx.query_row(
            "SELECT r.record_json,r.cancel_requested,e.state FROM runtime_runs r JOIN execution_plans e ON e.repo_id=r.repo_id AND e.plan_id=r.plan_id WHERE r.repo_id=?1 AND r.plan_id=?2 AND r.workspace_id=?3 AND e.workspace_id=r.workspace_id",
            params![
                authority.repository_id.as_str(),
                plan.as_str(),
                authority.workspace_id.as_str()
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let durable: RunRecord = serde_json::from_str(&record)?;
        require(
            durable.state == RunState::Running
                && plan_state == "ACTIVE"
                && !cancelled
                && durable.reconciliation.is_none(),
            "STALE_CONCURRENCY_AUTHORITY: lifecycle or reconciliation authority changed before launch",
        )?;
        let states = store::task_states(&tx, &authority.repository_id, plan)?;
        match (role, task) {
            (AgentRole::Executor, Some(task)) => require(
                states.get(task) == Some(&TaskState::Executing),
                "STALE_CONCURRENCY_AUTHORITY: task claim changed before executor launch",
            )?,
            (AgentRole::Verifier, Some(task)) => require(
                states.get(task) == Some(&TaskState::Verifying) && durable.pending.is_some(),
                "STALE_CONCURRENCY_AUTHORITY: task is not currently eligible for verification",
            )?,
            (AgentRole::Verifier, None) => require(
                states.values().all(|state| *state == TaskState::Verified)
                    && durable.accepted.len() == states.len()
                    && durable.pending.is_none()
                    && durable.branches.is_empty(),
                "STALE_CONCURRENCY_AUTHORITY: plan is not currently eligible for integration verification",
            )?,
            _ => {}
        }
        if self.authority_info.is_some() {
            let task =
                task.ok_or_else(|| Error::Invalid("branch task authority missing".into()))?;
            let batch = durable.batch_authority.as_ref().ok_or_else(|| {
                Error::Invalid(
                    "STALE_CONCURRENCY_AUTHORITY: durable batch authority missing".into(),
                )
            })?;
            require(
                batch.tasks.contains(task)
                    && batch.compatibility == self.branch_compatibility
                    && batch.ontology_generation
                        == self.branch_generation.clone().ok_or_else(|| {
                            Error::Invalid("branch ontology authority missing".into())
                        })?,
                "STALE_CONCURRENCY_AUTHORITY: launch no longer corresponds to the authorized batch branch",
            )?;
            let generation = graph::generation(&tx, &authority)?.ok_or_else(|| {
                Error::Invalid(
                    "STALE_CONCURRENCY_AUTHORITY: accepted ontology is unavailable".into(),
                )
            })?;
            let generation_id: String = tx.query_row(
                "SELECT generation_id FROM ontology_generations WHERE workspace_id=?1 AND sequence=?2 AND fingerprint=?3 ORDER BY ordinal DESC LIMIT 1",
                params![
                    authority.workspace_id.as_str(),
                    generation.sequence as i64,
                    generation.fingerprint
                ],
                |row| row.get(0),
            )?;
            require(
                generation_id == batch.ontology_generation,
                "STALE_CONCURRENCY_AUTHORITY: ontology changed before executor launch",
            )?;
            let expected: SourceSnapshot = self.artifacts.decode(&batch.source)?;
            let current = source::capture_bound(
                &authority.root,
                &self.artifacts,
                &authority.repository_id,
                &authority.workspace_id,
            )?;
            require(
                current == expected,
                "STALE_CONCURRENCY_AUTHORITY: canonical source changed before executor launch",
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Between context rounds the ontology generation and the captured source
    /// a base context was derived from must still hold, or the relay would
    /// issue a delta derived from stale assumptions.
    fn revalidate(
        &self,
        info: &RepositoryInfo,
        base: &context::BaseRef,
        current: &SourceSnapshot,
    ) -> Result<()> {
        // Freshness is deliberately not required here: a verifier is issued
        // context while the captured edits are not yet indexed. What must hold
        // is that the ontology generation and the captured source are the ones
        // the base was derived from; every issued fact is additionally bound to
        // its own content hash when it is resolved.
        require(
            graph::generation(&self.store.connection, info)? == base.graph_generation,
            "SOURCE_DRIFT: ontology generation changed between context rounds; replan required",
        )?;
        require(
            context::source_hash(current)? == base.source_hash,
            "SOURCE_DRIFT: workspace changed between context rounds; replan required",
        )
    }
    /// The approved deltas of a subject, each rechecked against the base it
    /// extends, in round order.
    fn issued_deltas(
        &self,
        ledger: &context::ContextLedger,
    ) -> Result<Vec<(context::ContextDelta, ArtifactRef)>> {
        let base = ledger
            .base
            .as_ref()
            .ok_or_else(|| Error::Invalid("context ledger has no issued base".into()))?;
        let mut deltas = vec![];
        for reference in ledger.deltas() {
            let delta: context::ContextDelta = self.artifacts.decode(&reference.artifact)?;
            require(
                delta.graph_generation == base.graph_generation
                    && delta.source_hash == base.source_hash,
                "SOURCE_DRIFT: an approved context delta was derived from other source/ontology state; replan required",
            )?;
            deltas.push((delta, reference.artifact.clone()));
        }
        Ok(deltas)
    }
    /// Persists a worker's context request, resolves it deterministically, and
    /// records the outcome. Nothing outside the envelope is ever granted here.
    #[allow(clippy::too_many_arguments)]
    fn relay(
        &mut self,
        info: &RepositoryInfo,
        run: &mut RunRecord,
        key: &str,
        envelope: &context::Envelope,
        job: &RuntimeJob,
        request: &ContextRequest,
        source: &SourceSnapshot,
        max_rounds: u32,
        max_escalations: u32,
    ) -> Result<Relay> {
        let ledger = run
            .context
            .get(key)
            .cloned()
            .ok_or_else(|| Error::Invalid("context relay ledger missing".into()))?;
        require(
            request.job_id == job.job_id && request.task_id == ledger.task_id,
            "context request does not match its issued job and task",
        )?;
        let plan = run.plan_id.clone();
        let request_ref = self.artifacts.json(request)?;
        let mut requester = job.clone();
        requester.context_request = Some(request_ref.clone());
        save_job(self.store, info, &requester, "CONTEXT_REQUESTED")?;
        let round = ledger.rounds_used();
        event(
            self.store,
            info,
            Some(&plan),
            Some(&job.job_id),
            "CONTEXT_REQUESTED",
            &json!({"subject":key,"round":round,"items":request.items.len(),"max_bytes":request.max_bytes,"request":request_ref.hash}).to_string(),
        )?;
        let limits = self.config.context;
        let budget = context::Budget {
            request_max_bytes: request.max_bytes as usize,
            round_limit: (request.max_bytes as usize).min(limits.max_round_bytes as usize),
            task_remaining: (limits.max_task_bytes as usize).saturating_sub(ledger.granted_bytes),
            rounds_used: round,
            max_rounds,
            escalations_used: ledger.escalations,
            max_escalations,
        };
        let generation = ledger
            .base
            .as_ref()
            .and_then(|b| b.graph_generation.clone());
        let authority = self.authority_info.as_ref().unwrap_or(info);
        let (resolution, items) = context::resolve(
            &*self.store,
            authority,
            &self.artifacts,
            source,
            key,
            generation.as_ref(),
            envelope,
            request,
            &request_ref.hash,
            budget,
        )?;
        let mut record = context::RoundRecord {
            round,
            job_id: job.job_id.clone(),
            request: request_ref,
            resolution: self.artifacts.json(&resolution)?,
            outcome: context::RoundOutcome::Denied,
            delta: None,
            decision: None,
        };
        let (relay, phase, detail) = match resolution.verdict {
            context::Verdict::Granted => {
                let delta = context::delta(
                    &plan,
                    ledger.task_id.as_ref(),
                    ledger.role,
                    &job.job_id,
                    round + 1,
                    &resolution,
                    context::Grant::Automatic,
                    items,
                )?;
                let detail = json!({"subject":key,"round":round+1,"delta":delta.delta_id,"bytes":delta.bytes,"items":delta.items.len(),"grant":"AUTOMATIC"});
                record.outcome = context::RoundOutcome::Granted;
                record.delta = Some(context::DeltaRef {
                    delta_id: delta.delta_id.clone(),
                    artifact: self.artifacts.json(&delta)?,
                    bytes: delta.bytes,
                });
                (Relay::Granted, "CONTEXT_DELTA_GRANTED", detail)
            }
            context::Verdict::Escalate => {
                let outside: BTreeSet<&String> = resolution
                    .items
                    .iter()
                    .filter(|i| i.outcome == context::ItemOutcome::OutsideEnvelope)
                    .flat_map(|i| &i.paths)
                    .collect();
                record.outcome = context::RoundOutcome::Escalated;
                (
                    Relay::Escalated,
                    "NEEDS_PLANNER_CONTEXT_APPROVAL",
                    json!({"subject":key,"round":round,"outside":outside,"bytes":resolution.bytes}),
                )
            }
            context::Verdict::Denied => {
                let code = resolution.code.clone().unwrap_or_else(|| "DENIED".into());
                // Name the first item that could not be issued, so the blocked
                // state explains itself without opening the artifact.
                let offender = resolution
                    .items
                    .iter()
                    .find(|i| i.outcome != context::ItemOutcome::Granted)
                    .map(|i| {
                        format!(
                            " (item {} {:?}{})",
                            i.index,
                            i.outcome,
                            i.detail
                                .as_deref()
                                .map(|d| format!(": {d}"))
                                .unwrap_or_default()
                        )
                    })
                    .unwrap_or_default();
                record.outcome = context::RoundOutcome::Denied;
                (
                    Relay::Denied(format!("{code}{offender}")),
                    "CONTEXT_REQUEST_DENIED",
                    json!({"subject":key,"round":round,"code":code,"bytes":resolution.bytes}),
                )
            }
        };
        let entry = run.context.get_mut(key).expect("issued ledger");
        if let Some(delta) = &record.delta {
            entry.granted_bytes += delta.bytes;
        }
        match record.outcome {
            context::RoundOutcome::Escalated => {
                entry.state = context::LedgerState::NeedsPlannerContextApproval;
                entry.escalations += 1;
            }
            context::RoundOutcome::Denied => entry.state = context::LedgerState::Denied,
            _ => {}
        }
        entry.rounds.push(record);
        self.checkpoint(info, run, phase)?;
        event(
            self.store,
            info,
            Some(&plan),
            Some(&job.job_id),
            phase,
            &detail.to_string(),
        )?;
        Ok(relay)
    }
    /// Projects the publication chain a concurrent result went through, in the
    /// same source-identity vocabulary the verifier already holds for its own
    /// issued source and for every evidence record. A serial result never left
    /// canonical source, so its diff and checks already share one identity and
    /// this is `null` rather than a claim it cannot check.
    ///
    /// Nothing here is a second copy of durable state: `canonical_after` is
    /// `pending.after`, `branch_result` is the captured diff's own after-image,
    /// and only `canonical_before` had to be retained on the pending result.
    /// The verifier can confirm the chain itself — `diff` is the artifact it
    /// was shown, and `canonical_after` must equal both its issued source and
    /// the `source_state` of every check evidence record.
    fn reconciliation_chain(&self, pending: &PendingTask, after: &SourceSnapshot) -> Result<Value> {
        let Some(from) = &pending.reconciled_from else {
            return Ok(Value::Null);
        };
        let before: SourceSnapshot = self.artifacts.decode(from)?;
        let captured: CapturedDiff = self.artifacts.decode(&pending.diff)?;
        let branch: SourceSnapshot = self.artifacts.decode(&captured.after)?;
        Ok(json!({
            "task_id": pending.task_id,
            "executor_job_id": pending.executor,
            "diff": pending.diff,
            "execution_workspace": pending.execution_workspace,
            "branch_result": branch.source_ref()?,
            "canonical_before": before.source_ref()?,
            "canonical_after": after.source_ref()?,
            "guarantee": "This executor ran in an isolated worktree, so `diff` was captured at `branch_result`. agentctl then published exactly `diff` onto `canonical_before`: every path was changed only from its recorded before-image, and publication refuses unless the resulting canonical source reaches `canonical_after`. `canonical_after` is therefore the exact result of this diff, and is the source this job was issued and the source every check evidence record is bound to.",
        }))
    }
    fn verify_pending(
        &mut self,
        info: &RepositoryInfo,
        plan: &planning::ExecutionPlan,
        run: &mut RunRecord,
        lease: &WorkspaceLease,
    ) -> Result<()> {
        let mut pending = run.pending.clone().expect("pending task");
        let after = self.expected(info, &pending.after)?;
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
        let current = if current.state == TaskState::Executing {
            self.store.transition_task(
                &info.repository_id,
                &pending.task_id,
                TaskState::Executing,
                TaskState::AwaitingVerification,
                None,
                now_ms()?,
            )?;
            self.store
                .task(&info.repository_id, &pending.task_id)?
                .ok_or_else(|| Error::Invalid("pending task disappeared".into()))?
        } else {
            current
        };
        if current.state == TaskState::Verified {
            require(
                pending.proof.is_some(),
                "verified task lacks runtime checkpoint",
            )?;
        } else {
            if pending.proof.is_none()
                && let Some((job, proof, input)) =
                    self.completed_verifier(info, &run.plan_id, Some(&pending.task_id), &after)?
            {
                require(
                    input.artifact["evidence"] == serde_json::to_value(&pending.evidence)?,
                    "recovery evidence mismatch",
                )?;
                pending.verifier = Some(job.job_id);
                pending.proof = Some(proof);
                run.pending = Some(pending.clone());
                self.checkpoint(info, run, "VERIFIER_OUTPUT_RECOVERED")?;
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
                self.checkpoint(info, run, "PACKET_CHECKS_COMPLETED")?;
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
                let records = self.evidence_input(info, &pending.evidence)?;
                let reconciliation = self.reconciliation_chain(&pending, &after)?;
                let (packet, contract, invariants) = (
                    task.clone(),
                    inspection.contract.clone(),
                    inspection.invariants.clone(),
                );
                let required =
                    required_refs(&task.verification.requirement_refs, &task.invariant_refs);
                let evidence = pending.evidence.clone();
                let build = move |diff: Value, deltas: Value, relay: Value| json!({"task":packet,"contract":contract,"invariants":invariants,"target":target,"evidence":evidence,"evidence_records":records,"diff":diff,"reconciliation":reconciliation,"deltas":deltas,"context_relay":relay,"required_refs":required,"verification_schema":schemars::schema_for!(VerificationPacket),"instruction":VERIFIER_INSTRUCTION});
                let (verifier, proof) = self.verify(
                    info,
                    run,
                    Verification {
                        task: Some(&pending.task_id),
                        diff: &pending.diff,
                        invariants: inspection.invariants.keys().cloned().collect(),
                        envelope: [task.read_scope.clone(), task.write_scope.clone()].concat(),
                        build: &build,
                    },
                    &after,
                    lease,
                )?;
                pending.verifier = Some(verifier);
                pending.proof = Some(proof);
                run.pending = Some(pending.clone());
                self.checkpoint(info, run, "VERIFIER_OUTPUT_CAPTURED")?;
            }
            self.expected(info, &pending.after)?;
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
            if next == TaskState::Rejected {
                // This plan can no longer complete, so its candidate never can
                // become accepted; say so durably.
                graph::close_for_plan(
                    &self.store.connection,
                    info,
                    &run.plan_id,
                    Some(&pending.task_id),
                    graph::DecisionReason::VerificationRejected,
                )?;
            }
            require(
                next == TaskState::Verified,
                "verifier rejected/blocked task; dependents remain locked; explicit correction plan required",
            )?;
        }
        run.expected = pending.after.clone();
        // Only here: the task just passed its independent verifier, so this is
        // the newest source a verifier has accepted.
        run.verified = Some(pending.after.clone());
        let origin = graph::GenerationOrigin::Runtime {
            plan_id: run.plan_id.clone(),
            task_id: Some(pending.task_id.clone()),
        };
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
        run.refused = None;
        self.checkpoint(info, run, "TASK_VERIFIED")?;
        // Task-verified work refreshes the working ontology as this plan's
        // candidate; it becomes accepted truth only at plan completion.
        require(
            self.store.index_observed(&info.root, &origin)?.failed == 0,
            "accepted source could not be indexed; planner review required",
        )?;
        Ok(())
    }
    /// Runs a fresh verifier over a captured transition, relaying bounded
    /// context on a budget of its own. A verifier request is derived only from
    /// verifier-visible material: it never inherits the executor's requests,
    /// reasons or transcript, and it is neither PASS nor REJECT.
    fn verify(
        &mut self,
        info: &RepositoryInfo,
        run: &mut RunRecord,
        verification: Verification<'_>,
        source: &SourceSnapshot,
        lease: &WorkspaceLease,
    ) -> Result<(JobId, VerificationPacket)> {
        let key = context::subject_key(AgentRole::Verifier, verification.task);
        let limits = self.config.context;
        loop {
            let ledger = run
                .context
                .entry(key.clone())
                .or_insert_with(|| {
                    context::ContextLedger::new(AgentRole::Verifier, verification.task.cloned())
                })
                .clone();
            require(
                ledger.state == context::LedgerState::Open,
                format!(
                    "verification context relay is {:?}; explicit planner decision required",
                    ledger.state
                ),
            )?;
            match &ledger.base {
                // The verifier's base is the captured transition it verifies.
                Some(base) => self.revalidate(info, base, source)?,
                None => {
                    graph::require_issuable(&self.store.connection, info, &run.plan_id)?;
                    let base = context::BaseRef {
                        artifact: verification.diff.clone(),
                        graph_generation: graph::generation(&self.store.connection, info)?,
                        source_hash: context::source_hash(source)?,
                    };
                    run.context.get_mut(&key).expect("issued ledger").base = Some(base);
                    self.checkpoint(info, run, "VERIFIER_CONTEXT_BASE_ISSUED")?;
                }
            }
            let ledger = run.context.get(&key).cloned().expect("issued ledger");
            let deltas = self.issued_deltas(&ledger)?;
            let round = ledger.rounds_used();
            let (diff, paths) = self.diff_input(verification.diff)?;
            let mut inventory = manifest::ContextInventory {
                paths,
                invariants: verification.invariants.clone(),
                visibility: Some(limits.visibility),
                ..Default::default()
            };
            context::inventory(&mut inventory, None, &deltas, round)?;
            let artifact = (verification.build)(
                diff,
                json!(deltas.iter().map(|(d, _)| d).collect::<Vec<_>>()),
                self.relay_state(&ledger, limits.verifier_max_rounds),
            );
            event(
                self.store,
                info,
                Some(&run.plan_id),
                None,
                "VERIFICATION_STARTED",
                "fresh verifier; no executor transcript or context-request history",
            )?;
            let (job, value) = self.invoke(
                info,
                Some(&run.plan_id),
                None,
                verification.task,
                AgentRole::Verifier,
                source,
                artifact,
                inventory,
                lease,
                &run.policy_hash,
            )?;
            let proof: VerificationPacket = serde_json::from_value(value)?;
            if let Some(request) = &proof.context_request {
                let envelope = context::Envelope {
                    scopes: verification.envelope.clone(),
                    memory: vec![],
                };
                // Verifier relay is automatic-only: a verifier cannot escalate
                // to the planner, so anything outside its envelope is denied.
                let outcome = self.relay(
                    info,
                    run,
                    &key,
                    &envelope,
                    &job,
                    request,
                    source,
                    limits.verifier_max_rounds,
                    0,
                )?;
                let code = match outcome {
                    Relay::Granted => continue,
                    Relay::Denied(code) => code,
                    Relay::Escalated => "ESCALATION_NOT_PERMITTED".into(),
                };
                return Err(Error::Invalid(format!(
                    "VERIFIER_CONTEXT_REQUEST_DENIED: {code}; verification reached no decision"
                )));
            }
            return Ok((job.job_id, proof));
        }
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
            return self.complete(info, run, current, &proof);
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
        let records = self.evidence_input(info, &evidence)?;
        // Structural facts are most useful at the acceptance boundary: give the
        // independent integration verifier a small read-only projection of the
        // exact accepted -> candidate ontology delta. No candidate means the
        // plan introduced no newly indexed generation.
        let structural_footprint = self
            .store
            .candidate_for_plan(&info.root, &run.plan_id)?
            .map(|candidate| {
                self.store.plan_footprint(
                    &info.root,
                    &run.plan_id,
                    &graph::FootprintRequest::Generation(candidate.generation_id),
                    graph::FootprintLimits {
                        files: 8,
                        entities: 16,
                        relations: 16,
                        signals: 5,
                        evidence_per_signal: 3,
                    },
                )
            })
            .transpose()?;
        let keys: Vec<String> = invariants.keys().cloned().collect();
        let envelope: Vec<ScopePath> = plan
            .packet
            .tasks
            .iter()
            .flat_map(|t| t.read_scope.iter().chain(&t.write_scope).cloned())
            .collect();
        let (packet, contract) = (plan.packet.clone(), plan.metadata.integration.clone());
        // Exactly what `validate_completion` demands: the plan's integration
        // checks and every task's critical invariants.
        let task_invariants: BTreeSet<String> = plan
            .packet
            .tasks
            .iter()
            .flat_map(|t| t.invariant_refs.iter().cloned())
            .collect();
        let required = required_refs(
            &plan.packet.integration_verification.requirement_refs,
            &task_invariants.into_iter().collect::<Vec<_>>(),
        );
        let issued = evidence.clone();
        let build = move |diff: Value, deltas: Value, relay: Value| json!({"plan":packet,"contract":contract,"invariants":invariants,"target":target,"evidence":issued,"evidence_records":records,"diff":diff,"structural_footprint":structural_footprint,"deltas":deltas,"context_relay":relay,"required_refs":required,"verification_schema":schemars::schema_for!(VerificationPacket),"instruction":VERIFIER_INSTRUCTION});
        let (_, proof) = self.verify(
            info,
            run,
            Verification {
                task: None,
                diff: &reference,
                invariants: keys,
                envelope,
                build: &build,
            },
            current,
            lease,
        )?;
        self.complete(info, run, current, &proof)
    }
    /// The acceptance boundary: plan completion and promotion of the plan's
    /// ontology candidate commit in one transaction, and only after an
    /// integration PASS over the unchanged final source.
    fn complete(
        &mut self,
        info: &RepositoryInfo,
        run: &mut RunRecord,
        current: &SourceSnapshot,
        proof: &VerificationPacket,
    ) -> Result<()> {
        self.expected(info, &run.expected)?;
        if proof.decision == VerificationDecision::Reject {
            graph::close_for_plan(
                &self.store.connection,
                info,
                &run.plan_id,
                None,
                graph::DecisionReason::IntegrationRejected,
            )?;
        }
        let accepted = self.store.complete_execution_plan_accepting(
            &info.root,
            &run.plan_id,
            proof,
            &current.source_ref()?,
        )?;
        run.state = RunState::Complete;
        self.checkpoint(info, run, "PLAN_RUNTIME_COMPLETED")?;
        self.reclaim_worktrees(info, run)?;
        if let Some(generation) = accepted {
            event(
                self.store,
                info,
                Some(&run.plan_id),
                None,
                "ONTOLOGY_GENERATION_ACCEPTED",
                &generation,
            )?;
        }
        Ok(())
    }
}

use super::contract::VERIFIER_INSTRUCTION;

fn provider_failure(class: OutcomeClass, detail: String) -> Error {
    Error::Provider {
        class,
        detail: detail.chars().take(900).collect(),
    }
}

/// The IDs `lifecycle` validation will require of a PASS, from the same
/// fields it checks.
fn required_refs(requirements: &[String], invariants: &[String]) -> Value {
    json!({"requirement_refs": requirements, "invariant_refs": invariants})
}
