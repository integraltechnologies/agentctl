#[allow(dead_code)]
mod common;
use agentctl::{
    local::{
        self,
        config::{MachineConfig, ProjectConfig, VerificationDefinition},
        paths::{MachinePaths, PathContext},
        planning::*,
        repository::RepositoryInfo,
        runtime::{process::*, provider::*, *},
        store::Store,
    },
    protocol::*,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

struct Fixture {
    temp: common::TempDir,
    root: PathBuf,
    paths: MachinePaths,
    config: RuntimeConfig,
}

struct AnalyticsFake {
    inner: Fake,
}
impl ProviderAdapter for AnalyticsFake {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn launch(
        &mut self,
        i: &JobInput,
        s: ProcessSpec,
        c: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        self.inner.launch(i, s, c)
    }
    fn collect(&self, o: &ProcessOutput) -> local::Result<Value> {
        self.inner.collect(o)
    }
    fn usage(&self, _: &ProcessOutput) -> local::Result<Usage> {
        Ok(Usage {
            provenance: TokenUsageProvenance::Exact,
            input: Some(10),
            output: Some(2),
            cached: Some(3),
        })
    }
}
#[test]
fn analytics_full_planner_reject_explicit_correction_fallback_and_guarded_completion() {
    let mut f = Fixture::new();
    let prepared = f.prepare();
    let p = artifact(&prepared);
    let inputs = seen();
    let mut store = f.store();
    Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(AnalyticsFake {
                inner: Fake {
                    mode: Mode::Planner(Box::new(p.clone())),
                    seen: inputs.clone(),
                },
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap();
    store
        .activate_execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    assert!(
        Runtime::new(
            &mut store,
            f.paths.clone(),
            f.config.clone(),
            BTreeMap::from([(
                "test".into(),
                Box::new(AnalyticsFake {
                    inner: Fake {
                        mode: Mode::Reject,
                        seen: inputs.clone()
                    }
                }) as Box<dyn ProviderAdapter>
            )])
        )
        .unwrap()
        .with_check_launcher(Box::new(Checks { fail: false }))
        .run(&f.root, &p.packet.plan_id)
        .is_err()
    );
    assert!(!store.execution_tasks(&f.root, &p.packet.plan_id).unwrap()[1].structurally_ready);
    git(&f.root, &["add", "src/api.rs"]);
    git(
        &f.root,
        &[
            "commit",
            "--quiet",
            "-m",
            "human-chosen correction baseline",
        ],
    );
    store.index_repository(&f.root).unwrap();
    let mut replacement = artifact(&f.prepare());
    replacement.packet.plan_id = PlanId::new("plan:analytics-correction").unwrap();
    for t in &mut replacement.packet.tasks {
        t.task_id = TaskId::new(format!("{}:r", t.task_id.as_str())).unwrap();
        for d in &mut t.dependencies {
            *d = TaskId::new(format!("{}:r", d.as_str())).unwrap();
        }
    }
    for (c, t) in replacement
        .metadata
        .contracts
        .iter_mut()
        .zip(&replacement.packet.tasks)
    {
        c.task_id = t.task_id.clone();
        c.task_packet_hash = hash(t).unwrap();
    }
    replacement.metadata.integration.plan_id = replacement.packet.plan_id.clone();
    replacement.metadata.integration.plan_packet_hash = hash(&replacement.packet).unwrap();
    replacement.metadata.replan = Some(ReplanReference {
        previous_plan_id: p.packet.plan_id.clone(),
        reason: "explicit correction".into(),
        previously_verified_tasks: vec![],
        replaced_tasks: p.packet.tasks.iter().map(|t| t.task_id.clone()).collect(),
    });
    store.import_execution_plan(&f.root, &replacement).unwrap();
    Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::new(),
    )
    .unwrap()
    .replace(&f.root, &p.packet.plan_id, &replacement.packet.plan_id)
    .unwrap();
    store
        .activate_execution_plan(&f.root, &replacement.packet.plan_id)
        .unwrap();
    configure_fallback(&mut f, 2);
    let adapters: BTreeMap<String, Box<dyn ProviderAdapter>> = BTreeMap::from([
        (
            "test".into(),
            Box::new(Fake {
                mode: Mode::Usage(TokenUsageProvenance::Exact),
                seen: inputs.clone(),
            }) as Box<dyn ProviderAdapter>,
        ),
        (
            "primary".into(),
            Box::new(RoutedFake {
                inner: Fake {
                    mode: Mode::Usage(TokenUsageProvenance::Exact),
                    seen: inputs.clone(),
                },
                unavailable_after_first: true,
                failure: Some(routing::FailureClass::ProviderUnavailable),
                calls: 0,
            }) as Box<dyn ProviderAdapter>,
        ),
        (
            "fallback".into(),
            Box::new(Fake {
                mode: Mode::Usage(TokenUsageProvenance::Estimated),
                seen: inputs.clone(),
            }) as Box<dyn ProviderAdapter>,
        ),
    ]);
    let run = Runtime::new(&mut store, f.paths.clone(), f.config.clone(), adapters)
        .unwrap()
        .with_check_launcher(Box::new(Checks { fail: false }))
        .run(&f.root, &replacement.packet.plan_id)
        .unwrap();
    assert_eq!(run.state, RunState::Complete);
    let info = RepositoryInfo::discover(&f.root).unwrap();
    let now = local::now_ms().unwrap();
    let mut q = local::analytics::Query::workspace(
        info.repository_id.as_str().into(),
        info.workspace_id.as_str().into(),
        now,
    );
    q.from_ms = 0;
    q.session = Some(run.engineering_session.unwrap().id);
    let s = store.analytics(q.clone(), now).unwrap();
    assert_eq!(s.summary.jobs, 18);
    assert_eq!(s.summary.tokens.total.exact, Some(168));
    assert_eq!(s.summary.tokens.total.estimated, Some(66));
    assert_eq!(s.summary.unknown_jobs, 6);
    assert_eq!(s.summary.token_quality, "PARTIAL");
    assert_eq!(s.summary.pass, 5);
    assert_eq!(s.summary.reject, 1);
    assert_eq!(s.summary.rejected_executor_attempts, 1);
    assert_eq!(s.summary.accepted_executor_attempts, 4);
    assert_eq!(s.summary.packet_rejected_usage.total.exact, Some(24));
    assert_eq!(s.summary.packet_accepted_usage.total.exact, Some(110));
    assert_eq!(s.summary.packet_accepted_usage.total.estimated, Some(66));
    assert_eq!(s.summary.fallback_executions, 3);
    assert_eq!(s.summary.availability_failures["PROVIDER_UNAVAILABLE"], 6);
    assert_eq!(s.summary.policy_skipped, 0);
    assert_eq!(s.integration.tokens.total.exact, Some(22));
    assert_eq!(s.unattributed.tokens.total.exact, Some(12));
    assert_eq!(s.sessions[0].correction_round, Some(1));
    assert_eq!(s.sessions[0].verified, 4);
    assert_eq!(s.sessions[0].current_tasks, 4);
    assert_eq!(s.sessions[0].lifecycle, "COMPLETE");
    assert_eq!(s.verified_tasks_with_complete_usage, 1);
    assert_eq!(s.verified_tasks_with_partial_usage, 3);
    assert_eq!(s.tokens_per_complete_verified_task, Some(44.0));
    assert!(s.jobs.iter().all(|j| j.prompt_bytes.is_some()
        && j.context_bytes.is_some()
        && j.context_budget.is_some()));
    assert!(s.jobs.iter().all(|j| j.active_execution_ms.is_none()));
    let before = serde_json::to_value(&s).unwrap();
    f.config.roles.get_mut("executor").unwrap().model = Some("new-current-model".into());
    fs::write(
        &f.paths.machine_config,
        toml::to_string(&MachineConfig {
            runtime: f.config.clone(),
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        before,
        serde_json::to_value(store.analytics(q.clone(), now).unwrap()).unwrap()
    );
    drop(store);
    let db = fs::read(&f.paths.database).unwrap();
    for args in [
        vec![
            "analytics",
            "session",
            q.session.as_deref().unwrap(),
            "--json",
        ],
        vec!["analytics", "roles"],
        vec!["analytics", "usage", "--json"],
        vec!["analytics", "routes", "--json"],
        vec!["analytics", "corrections"],
    ] {
        let o = cli(&f, &args);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let text = String::from_utf8(o.stdout).unwrap();
        assert!(!text.contains("EXECUTOR_PRIVATE_CANARY"));
        if args.last() == Some(&"--json") {
            serde_json::from_str::<Value>(&text).unwrap();
        }
    }
    assert_eq!(db, fs::read(&f.paths.database).unwrap());
}

struct LatePolicyMutation(&'static str);
impl ProviderAdapter for LatePolicyMutation {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            model: true,
            effort: true,
            fresh_session: true,
            structured_output: true,
            token_usage: false,
        }
    }
    fn launch(
        &mut self,
        _: &JobInput,
        spec: ProcessSpec,
        _: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        spec.recheck_policy()?;
        assert!(spec.writable && spec.network);
        assert!(spec.protected.is_empty());
        let path = spec.workspace.join(".agentctl/project.toml");
        match self.0 {
            "removed" => fs::remove_file(path).unwrap(),
            "malformed" => fs::write(path, "not = [valid").unwrap(),
            _ => {
                let mut p = ProjectConfig::load(&spec.workspace).unwrap();
                p.routing.read_only = true;
                p.routing.deny_network = true;
                fs::write(path, toml::to_string(&p).unwrap()).unwrap();
            }
        }
        // Native launch must reject before any spawn, even after the adapter
        // boundary check passed. No nested sandbox permission/model call needed.
        NativeProcess::launch(&spec).map(|p| Box::new(p) as Box<dyn RunningProcess>)
    }
    fn collect(&self, _: &ProcessOutput) -> local::Result<Value> {
        unreachable!()
    }
}
#[test]
fn native_spawn_rechecks_policy_after_adapter_preparation_without_spawning() {
    for mutation in ["changed", "removed", "malformed"] {
        let f = Fixture::new();
        let p = f.plan();
        let mut store = f.store();
        let result = Runtime::new(
            &mut store,
            f.paths.clone(),
            f.config.clone(),
            BTreeMap::from([(
                "test".into(),
                Box::new(LatePolicyMutation(mutation)) as Box<dyn ProviderAdapter>,
            )]),
        )
        .unwrap()
        .run(&f.root, &p.packet.plan_id);
        assert!(result.is_err());
        // Inspect durable failure without needing the now-invalid project file.
        let c = common::sql(&f.paths.database);
        let json: String = c
            .query_row("SELECT record_json FROM runtime_jobs", [], |r| r.get(0))
            .unwrap();
        let job: RuntimeJob = serde_json::from_str(&json).unwrap();
        assert!(job.pid.is_none());
        assert!(
            job.failure
                .unwrap()
                .contains("before process spawn; replan/revalidation")
        );
    }
}

#[test]
fn validated_policy_snapshot_blocks_deterministic_launch_boundary_mutations() {
    for boundary in ["before_snapshot", "validated", "prelaunch"] {
        let mut f = Fixture::new();
        configure_fallback(&mut f, 2);
        let p = f.plan();
        let inputs = seen();
        let mut store = f.store();
        let mut changed = ProjectConfig::load(&f.root).unwrap();
        changed.routing.deny_network = true;
        changed.routing.read_only = true;
        changed.routing.profiles.insert(
            "executor".into(),
            routing::RolePatch {
                provider: Some("fallback".into()),
                model: Some("UNVALIDATED_MODEL".into()),
                context_bytes: Some(100),
                fallbacks: Some(vec![]),
                instructions: Some(vec!["UNVALIDATED_INSTRUCTION".into()]),
                ..Default::default()
            },
        );
        let path = f.root.join(".agentctl/project.toml");
        let mut runtime = Runtime::new(
            &mut store,
            f.paths.clone(),
            f.config.clone(),
            BTreeMap::from([(
                "primary".into(),
                Box::new(Fake {
                    mode: Mode::Pass,
                    seen: inputs.clone(),
                }) as Box<dyn ProviderAdapter>,
            )]),
        )
        .unwrap()
        .with_policy_observer(move |phase| {
            if phase == boundary {
                fs::write(&path, toml::to_string(&changed).unwrap()).unwrap();
            }
        });
        assert!(runtime.run(&f.root, &p.packet.plan_id).is_err());
        assert!(
            inputs.lock().unwrap().is_empty(),
            "provider must never launch on policy drift"
        );
        let jobs = store.runtime_jobs(&f.root, None).unwrap();
        if boundary == "before_snapshot" {
            assert!(jobs.is_empty());
        } else {
            assert_eq!(jobs.len(), 1);
            let job = &jobs[0];
            assert_eq!(job.config.provider, "primary");
            assert_eq!(job.config.model.as_deref(), Some("opaque-executor"));
            assert_eq!(
                job.route.as_ref().unwrap().project_policy_hash,
                p.metadata.source.policy_hash
            );
            assert!(
                job.failure
                    .as_deref()
                    .unwrap()
                    .contains("replan/revalidation")
            );
        }
        let snapshot = store.observe(local::now_ms().unwrap()).unwrap();
        assert!(snapshot.events.iter().any(|e| e.phase == "POLICY_DRIFT"));
    }
}

#[test]
fn policy_filtered_primary_executes_allowed_backend_and_records_policy_not_failure() {
    let mut f = Fixture::new();
    configure_fallback(&mut f, 0);
    let mut policy = ProjectConfig::load(&f.root).unwrap();
    policy.routing.allowed_providers =
        Some(["test".into(), "fallback".into()].into_iter().collect());
    fs::write(
        f.root.join(".agentctl/project.toml"),
        toml::to_string(&policy).unwrap(),
    )
    .unwrap();
    git(&f.root, &["add", "."]);
    git(&f.root, &["commit", "--quiet", "-m", "fixture policy"]);
    f.store().index_repository(&f.root).unwrap();
    let p = f.plan();
    let inputs = seen();
    let mut store = f.store();
    let adapters = ["test", "fallback"]
        .into_iter()
        .map(|name| {
            (
                name.into(),
                Box::new(Fake {
                    mode: Mode::Pass,
                    seen: inputs.clone(),
                }) as Box<dyn ProviderAdapter>,
            )
        })
        .collect();
    let run = Runtime::new(&mut store, f.paths.clone(), f.config.clone(), adapters)
        .unwrap()
        .with_check_launcher(Box::new(Checks { fail: false }))
        .run(&f.root, &p.packet.plan_id)
        .unwrap();
    assert_eq!(run.state, RunState::Complete);
    for job in store
        .runtime_jobs(&f.root, None)
        .unwrap()
        .iter()
        .filter(|j| j.role == AgentRole::Executor)
    {
        let r = job.route.as_ref().unwrap();
        assert_eq!(r.primary.provider, "primary");
        assert_eq!(r.selected.provider, "fallback");
        assert_eq!(r.policy_skipped.len(), 2);
        assert!(r.failures.is_empty());
        assert_eq!(r.attempt, 0);
    }
    let snapshot = store.observe(local::now_ms().unwrap()).unwrap();
    assert!(
        snapshot
            .events
            .iter()
            .any(|e| e.phase == "ROUTE_POLICY_FILTERED")
    );
    assert!(
        snapshot
            .agents
            .iter()
            .any(
                |a| a.policy_skip_reason.as_deref() == Some("PROJECT_POLICY_RESTRICTION")
                    && a.fallback_reason.is_none()
            )
    );
}

#[test]
fn diagnostic_exit_codes_preserve_human_and_json_rows() {
    let f = Fixture::new();
    for json_mode in [false, true] {
        for (args, valid) in [
            (vec!["role", "show", "executor"], true),
            (vec!["role", "show", "recon"], false),
            (vec!["route", "check"], false),
            (vec!["route", "unknown"], false),
        ] {
            let mut args = args;
            if json_mode {
                args.push("--json");
            }
            let o = cli(&f, &args);
            assert_eq!(o.status.success(), valid);
            assert!(!o.stdout.is_empty());
            if json_mode {
                let _: Value = serde_json::from_slice(&o.stdout).unwrap();
            }
            if args[1] == "check" {
                assert!(String::from_utf8_lossy(&o.stdout).contains("opaque-executor"));
            }
        }
    }
}

#[test]
fn compact_override_rejects_ambiguous_empty_and_unknown_components() {
    let mut f = Fixture::new();
    f.config.profiles.insert(
        "custom".into(),
        routing::RolePatch {
            provider: Some("test".into()),
            ..Default::default()
        },
    );
    fs::write(
        &f.paths.machine_config,
        toml::to_string(&MachineConfig {
            runtime: f.config.clone(),
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    for bad in [
        "",
        "executor",
        "executor:",
        ":test",
        "executor::model",
        "executor:test:",
        "executor:test:model:extra",
        "executor:test:model:extra:more",
        "executor: :model",
        " :test",
        "executor:test: ",
        "unknown:test",
    ] {
        assert!(
            !cli(&f, &["route", "executor", "--override", bad, "--json"])
                .status
                .success(),
            "accepted {bad:?}"
        );
    }
    for good in ["executor:test", "executor:test:model"] {
        assert!(
            cli(&f, &["route", "executor", "--override", good, "--json"])
                .status
                .success()
        );
    }
    assert!(
        cli(
            &f,
            &[
                "route",
                "custom",
                "--override",
                "custom:test:model",
                "--json"
            ]
        )
        .status
        .success()
    );
}

#[test]
fn unused_or_forbidden_explicit_overrides_are_not_silently_ignored() {
    let mut f = Fixture::new();
    assert!(
        !cli(&f, &["route", "executor", "--override", "verifier:test"])
            .status
            .success()
    );
    f.config
        .providers
        .insert("forbidden".into(), f.config.providers["test"].clone());
    fs::write(
        &f.paths.machine_config,
        toml::to_string(&MachineConfig {
            runtime: f.config.clone(),
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    let mut p = ProjectConfig::load(&f.root).unwrap();
    p.routing.allowed_providers = Some(["test".into()].into_iter().collect());
    fs::write(
        f.root.join(".agentctl/project.toml"),
        toml::to_string(&p).unwrap(),
    )
    .unwrap();
    let o = cli(
        &f,
        &[
            "route",
            "executor",
            "--override",
            "executor:forbidden",
            "--json",
        ],
    );
    assert!(!o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("project forbids explicit")
    );
}

struct RoutedFake {
    inner: Fake,
    unavailable_after_first: bool,
    failure: Option<routing::FailureClass>,
    calls: usize,
}
impl ProviderAdapter for RoutedFake {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn launch(
        &mut self,
        input: &JobInput,
        spec: ProcessSpec,
        config: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        self.calls += 1;
        if let Some(reason) = self
            .failure
            .filter(|_| !self.unavailable_after_first || self.calls > 1)
        {
            return Err(local::Error::ProviderAvailability(reason));
        }
        assert!(
            input.compiled.is_some(),
            "all issued workers receive compiled instructions"
        );
        self.inner.launch(input, spec, config)
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        let mut value = self.inner.collect(output)?;
        if value.get("executor_job_id").is_some() {
            value["notes"] = json!("EXECUTOR_PRIVATE_CANARY");
        }
        Ok(value)
    }
    fn usage(&self, output: &ProcessOutput) -> local::Result<Usage> {
        self.inner.usage(output)
    }
}
fn configure_fallback(f: &mut Fixture, maximum: usize) {
    for name in ["primary", "fallback", "unavailable"] {
        f.config
            .providers
            .insert(name.into(), f.config.providers["test"].clone());
    }
    f.config.roles.get_mut("executor").unwrap().provider = "primary".into();
    f.config.profiles.insert(
        "executor".into(),
        routing::RolePatch {
            fallbacks: Some(vec![
                RoleConfig {
                    provider: "unavailable".into(),
                    model: Some("missing-model".into()),
                    effort: None,
                },
                RoleConfig {
                    provider: "fallback".into(),
                    model: Some("alternate-model".into()),
                    effort: None,
                },
            ]),
            max_fallback_attempts: Some(maximum),
            ..Default::default()
        },
    );
}

struct PermissionProbe(Arc<Mutex<Vec<(bool, bool)>>>);
impl ProviderAdapter for PermissionProbe {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            model: true,
            effort: true,
            fresh_session: true,
            structured_output: true,
            token_usage: false,
        }
    }
    fn launch(
        &mut self,
        _: &JobInput,
        spec: ProcessSpec,
        _: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        self.0.lock().unwrap().push((spec.writable, spec.network));
        Err(local::Error::ProviderAvailability(
            routing::FailureClass::StartupFailure,
        ))
    }
    fn collect(&self, _: &ProcessOutput) -> local::Result<Value> {
        unreachable!()
    }
}
#[test]
fn fallback_cannot_expand_project_permission_boundaries() {
    let mut f = Fixture::new();
    configure_fallback(&mut f, 2);
    let mut project = ProjectConfig::load(&f.root).unwrap();
    project.routing.read_only = true;
    project.routing.deny_network = true;
    fs::write(
        f.root.join(".agentctl/project.toml"),
        toml::to_string(&project).unwrap(),
    )
    .unwrap();
    git(&f.root, &["add", ".agentctl/project.toml"]);
    git(
        &f.root,
        &["commit", "--quiet", "-m", "fixture hard routing policy"],
    );
    f.store().index_repository(&f.root).unwrap();
    let p = f.plan();
    let probes = Arc::new(Mutex::new(vec![]));
    let adapters: BTreeMap<String, Box<dyn ProviderAdapter>> =
        ["primary", "unavailable", "fallback"]
            .into_iter()
            .map(|name| {
                (
                    name.into(),
                    Box::new(PermissionProbe(probes.clone())) as Box<dyn ProviderAdapter>,
                )
            })
            .collect();
    let mut store = f.store();
    assert!(
        Runtime::new(&mut store, f.paths.clone(), f.config.clone(), adapters)
            .unwrap()
            .with_check_launcher(Box::new(Checks { fail: false }))
            .run(&f.root, &p.packet.plan_id)
            .is_err()
    );
    assert_eq!(*probes.lock().unwrap(), vec![(false, false); 3]);
    assert_eq!(store.runtime_jobs(&f.root, None).unwrap().len(), 3);
}

#[test]
fn explicit_runtime_override_first_fallback_and_same_model_keep_distinct_workers() {
    let mut f = Fixture::new();
    configure_fallback(&mut f, 1);
    f.config.profiles.get_mut("executor").unwrap().fallbacks = Some(vec![RoleConfig {
        provider: "fallback".into(),
        model: Some("same-model".into()),
        effort: None,
    }]);
    let p = f.plan();
    let inputs = seen();
    let mut store = f.store();
    let mut runtime = Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "fallback".into(),
            Box::new(Fake {
                mode: Mode::Pass,
                seen: inputs.clone(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .with_role_overrides(BTreeMap::from([
        (
            "executor".into(),
            routing::RolePatch {
                provider: Some("unavailable".into()),
                ..Default::default()
            },
        ),
        (
            "verifier".into(),
            routing::RolePatch {
                provider: Some("fallback".into()),
                model: Some("same-model".into()),
                ..Default::default()
            },
        ),
    ]))
    .unwrap()
    .with_check_launcher(Box::new(Checks { fail: false }));
    assert_eq!(
        runtime.run(&f.root, &p.packet.plan_id).unwrap().state,
        RunState::Complete
    );
    let jobs = store.runtime_jobs(&f.root, None).unwrap();
    for job in jobs
        .iter()
        .filter(|j| j.state == RuntimeJobState::Succeeded)
    {
        assert_eq!(job.config.provider, "fallback");
        assert_eq!(job.config.model.as_deref(), Some("same-model"));
        assert_eq!(
            job.route.as_ref().unwrap().sources["provider"],
            "explicit user"
        );
        assert_eq!(
            job.route.as_ref().unwrap().attempt,
            usize::from(job.role == AgentRole::Executor)
        );
    }
    let inputs = inputs.lock().unwrap();
    assert_eq!(inputs.len(), 9);
    let sessions: std::collections::BTreeSet<_> = inputs.iter().map(|i| &i.session_id).collect();
    assert_eq!(sessions.len(), 9);
    let owners: std::collections::BTreeSet<_> = inputs
        .iter()
        .map(|i| &i.ownership.agent_instance_id)
        .collect();
    assert_eq!(owners.len(), 9);
}

#[test]
fn routed_planner_dag_fallback_keeps_provenance_ownership_and_verification() {
    let mut f = Fixture::new();
    configure_fallback(&mut f, 2);
    let prepared = f.prepare();
    let p = artifact(&prepared);
    let inputs = seen();
    let mut store = f.store();
    Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p.clone())),
                seen: inputs.clone(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap();
    store
        .activate_execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    let tasks = store.execution_tasks(&f.root, &p.packet.plan_id).unwrap();
    assert!(tasks[0].structurally_ready);
    assert!(!tasks[1].structurally_ready);
    let adapters: BTreeMap<String, Box<dyn ProviderAdapter>> = BTreeMap::from([
        (
            "test".into(),
            Box::new(Fake {
                mode: Mode::Pass,
                seen: inputs.clone(),
            }) as Box<dyn ProviderAdapter>,
        ),
        (
            "primary".into(),
            Box::new(RoutedFake {
                inner: Fake {
                    mode: Mode::Pass,
                    seen: inputs.clone(),
                },
                unavailable_after_first: true,
                failure: Some(routing::FailureClass::ProviderUnavailable),
                calls: 0,
            }) as Box<dyn ProviderAdapter>,
        ),
        (
            "fallback".into(),
            Box::new(Fake {
                mode: Mode::Usage(TokenUsageProvenance::Exact),
                seen: inputs.clone(),
            }) as Box<dyn ProviderAdapter>,
        ),
    ]);
    let run = Runtime::new(&mut store, f.paths.clone(), f.config.clone(), adapters)
        .unwrap()
        .with_check_launcher(Box::new(Checks { fail: false }))
        .run(&f.root, &p.packet.plan_id)
        .unwrap();
    assert_eq!(run.state, RunState::Complete);
    let session = run.engineering_session.unwrap();
    let jobs = store.runtime_jobs(&f.root, None).unwrap();
    let fallback_jobs: Vec<_> = jobs
        .iter()
        .filter(|j| j.config.provider == "fallback")
        .collect();
    assert_eq!(fallback_jobs.len(), 3);
    for job in &jobs {
        let route = job.route.as_ref().unwrap();
        assert_eq!(route.selected, job.config);
        assert_eq!(route.requested_role, routing::role_name(job.role));
        assert_eq!(
            job.ownership.as_ref().unwrap().engineering_session_id,
            session.id
        );
        let provenance = job.prompt.as_ref().unwrap();
        assert!(provenance.bytes < 262144);
        assert_eq!(provenance.profile_hash, route.profile_hash);
    }
    for job in &fallback_jobs {
        let r = job.route.as_ref().unwrap();
        assert_eq!(r.attempt, 2);
        assert_eq!(r.failures.len(), 2);
        assert_eq!(r.primary.provider, "primary");
        assert_eq!(
            r.failures[0].reason,
            routing::FailureClass::ProviderUnavailable
        );
    }
    let successful: Vec<_> = jobs
        .iter()
        .filter(|j| j.state == RuntimeJobState::Succeeded)
        .collect();
    assert_eq!(successful.len(), 10);
    let conversations: std::collections::BTreeSet<_> = jobs.iter().map(|j| &j.session_id).collect();
    assert_eq!(conversations.len(), jobs.len());
    for input in inputs.lock().unwrap().iter() {
        let profile = routing::resolve(
            &f.config,
            &Default::default(),
            routing::role_name(input.role),
            None,
        )
        .unwrap();
        let a = prompt::compile(&profile.profile, &profile.project_policy_hash, input).unwrap();
        let b = prompt::compile(&profile.profile, &profile.project_policy_hash, input).unwrap();
        assert_eq!(a.bytes, b.bytes);
        assert_eq!(a.bytes, input.compiled.as_ref().unwrap().bytes);
        let text = String::from_utf8(a.bytes).unwrap();
        eprintln!("compiled fixture {:?}: {} bytes", input.role, text.len());
        assert!(text.len() < 128 * 1024, "fixture prompt growth regression");
        for absent in [
            "original giant conversation",
            "executor chain-of-thought",
            "EXECUTOR_PRIVATE_CANARY",
            "unrelated-secret-memory",
            "authentication\":",
            "machine_config",
        ] {
            assert!(!text.contains(absent));
        }
        if input.role == AgentRole::Verifier {
            assert!(text.contains("diff"));
            assert!(text.contains("evidence"));
            assert!(text.contains("VerificationPacket"));
            assert!(input.artifact.get("result").is_none());
        }
        if input.role == AgentRole::Executor {
            assert!(text.contains("ResultPacket"));
            assert!(input.artifact.get("planner_packet").is_none());
        }
        let tiny = routing::RoleProfile {
            context_bytes: 100,
            ..profile.profile
        };
        assert!(prompt::compile(&tiny, "test", input).is_err());
    }
    drop(store);
    // Historical records do not consult edited machine policy.
    f.config.roles.get_mut("executor").unwrap().provider = "test".into();
    fs::write(
        &f.paths.machine_config,
        toml::to_string(&MachineConfig {
            runtime: f.config.clone(),
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    let snapshot = Store::read_only(&f.paths.database, 5000)
        .unwrap()
        .observe(local::now_ms().unwrap())
        .unwrap();
    assert!(
        snapshot
            .agents
            .iter()
            .any(|a| a.provider.as_deref() == Some("fallback")
                && a.route_attempt == Some(2)
                && a.requested_role.as_deref() == Some("executor"))
    );
    assert!(snapshot.events.iter().any(|e| e.phase == "ROUTE_FALLBACK"));
    assert!(
        snapshot
            .usage
            .iter()
            .any(|u| u.provider.as_deref() == Some("fallback"))
    );
    assert!(
        snapshot
            .agents
            .iter()
            .all(|a| a.liveness == local::observe::Liveness::Unknown)
    );
    assert!(
        Store::read_only(&f.paths.database, 5000)
            .unwrap()
            .runtime_jobs(&f.root, None)
            .unwrap()
            .iter()
            .any(|j| j.config.provider == "fallback")
    );
}

#[test]
fn fallback_auth_startup_capability_and_attempt_limits_are_mechanical() {
    for (reason, limit) in [
        (routing::FailureClass::AuthUnavailable, 2),
        (routing::FailureClass::StartupFailure, 2),
        (routing::FailureClass::CapabilityUnsupported, 2),
        (routing::FailureClass::ProviderUnavailable, 1),
        (routing::FailureClass::ProviderUnavailable, 0),
    ] {
        let mut f = Fixture::new();
        configure_fallback(&mut f, limit);
        let p = f.plan();
        let inputs = seen();
        let mut store = f.store();
        let adapters: BTreeMap<String, Box<dyn ProviderAdapter>> = BTreeMap::from([
            (
                "test".into(),
                Box::new(Fake {
                    mode: Mode::Pass,
                    seen: inputs.clone(),
                }) as Box<dyn ProviderAdapter>,
            ),
            (
                "primary".into(),
                Box::new(RoutedFake {
                    inner: Fake {
                        mode: Mode::Pass,
                        seen: inputs.clone(),
                    },
                    unavailable_after_first: false,
                    failure: Some(reason),
                    calls: 0,
                }) as Box<dyn ProviderAdapter>,
            ),
            (
                "fallback".into(),
                Box::new(Fake {
                    mode: Mode::Pass,
                    seen: inputs.clone(),
                }) as Box<dyn ProviderAdapter>,
            ),
        ]);
        let run = Runtime::new(&mut store, f.paths.clone(), f.config.clone(), adapters)
            .unwrap()
            .with_check_launcher(Box::new(Checks { fail: false }))
            .run(&f.root, &p.packet.plan_id);
        if limit == 2 {
            assert_eq!(run.unwrap().state, RunState::Complete);
        } else {
            assert!(run.is_err());
            assert_eq!(
                store
                    .runtime_status(&f.root, &p.packet.plan_id)
                    .unwrap()
                    .unwrap()
                    .state,
                RunState::Blocked
            );
        }
        let jobs = store.runtime_jobs(&f.root, None).unwrap();
        assert!(
            jobs.iter()
                .all(|j| j.route.as_ref().unwrap().attempt <= limit)
        );
        if limit == 2 {
            assert!(jobs.iter().any(|j| {
                j.route
                    .as_ref()
                    .unwrap()
                    .failures
                    .first()
                    .is_some_and(|f| f.reason == reason)
            }));
        }
    }
}

#[test]
fn reject_and_unknown_failure_never_select_fallback() {
    for mode in [Mode::Reject, Mode::Crash] {
        let mut f = Fixture::new();
        configure_fallback(&mut f, 2);
        f.config.roles.get_mut("executor").unwrap().provider = "test".into();
        f.config.profiles.insert(
            "verifier".into(),
            routing::RolePatch {
                fallbacks: Some(vec![RoleConfig {
                    provider: "fallback".into(),
                    model: None,
                    effort: None,
                }]),
                ..Default::default()
            },
        );
        let p = f.plan();
        assert!(f.run(&p, mode, seen(), false).is_err());
        assert_eq!(
            f.store()
                .runtime_status(&f.root, &p.packet.plan_id)
                .unwrap()
                .unwrap()
                .state,
            RunState::Blocked
        );
        assert!(
            f.store()
                .runtime_jobs(&f.root, None)
                .unwrap()
                .iter()
                .all(|j| j.route.as_ref().unwrap().attempt == 0)
        );
    }
}

#[test]
fn route_cli_is_read_only_and_explicit_override_is_inspectable() {
    let mut f = Fixture::new();
    for name in ["recon", "reviewer"] {
        f.config
            .roles
            .insert(name.into(), f.config.roles["verifier"].clone());
    }
    fs::write(
        &f.paths.machine_config,
        toml::to_string(&MachineConfig {
            runtime: f.config.clone(),
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    let before = fs::read(&f.paths.database).unwrap();
    for args in [
        vec!["roles", "--json"],
        vec!["role", "show", "recon", "--json"],
        vec!["route", "check", "--json"],
        vec![
            "route",
            "executor",
            "--override",
            "executor:test:user-model",
            "--json",
        ],
    ] {
        let output = cli(&f, &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        if args.contains(&"--override") {
            assert_eq!(value["resolved"]["primary"]["model"], "user-model");
            assert_eq!(value["resolved"]["sources"]["model"], "explicit user");
        }
    }
    assert_eq!(before, fs::read(&f.paths.database).unwrap());
    assert!(
        !cli(
            &f,
            &[
                "route",
                "executor",
                "--override",
                "executor:test:a",
                "--override",
                "executor:test:b"
            ]
        )
        .status
        .success()
    );
}
impl Fixture {
    fn new() -> Self {
        let temp = common::TempDir::new();
        let root = temp.0.join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        for file in ["api", "graph", "cli", "regression"] {
            fs::write(
                root.join(format!("src/{file}.rs")),
                format!("pub fn cache_{file}() {{}}\n"),
            )
            .unwrap();
        }
        let mut policy = ProjectConfig::initialize(&root).unwrap();
        policy.commands.insert(
            "unit".into(),
            CommandSpec {
                program: "/usr/bin/true".into(),
                args: vec![],
                cwd: ".".into(),
            },
        );
        for name in ["unit", "integration"] {
            policy.verification.insert(
                name.into(),
                VerificationDefinition {
                    description: format!("{name} tests"),
                    command_refs: vec!["unit".into()],
                },
            );
        }
        fs::write(
            root.join(".agentctl/project.toml"),
            toml::to_string(&policy).unwrap(),
        )
        .unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "baseline"]);
        let paths = MachinePaths::resolve(&PathContext {
            home: Some(temp.0.join("home")),
            ..Default::default()
        })
        .unwrap();
        paths.create_directories().unwrap();
        let mut config = RuntimeConfig::default();
        config.providers.insert(
            "test".into(),
            ProviderConfig {
                authentication: Default::default(),
                adapter: "codex".into(),
                executable: "/usr/bin/true".into(),
            },
        );
        for role in ["planner", "executor", "verifier"] {
            config.roles.insert(
                role.into(),
                RoleConfig {
                    provider: "test".into(),
                    model: Some(format!("opaque-{role}")),
                    effort: None,
                },
            );
        }
        let machine = MachineConfig {
            runtime: config.clone(),
            ..Default::default()
        };
        fs::write(&paths.machine_config, toml::to_string(&machine).unwrap()).unwrap();
        let mut s = Store::open(&paths.database, 5000).unwrap();
        s.register_repository(RepositoryInfo::discover(&root).unwrap())
            .unwrap();
        s.index_repository(&root).unwrap();
        Self {
            temp,
            root,
            paths,
            config,
        }
    }
    fn store(&self) -> Store {
        Store::open(&self.paths.database, 5000).unwrap()
    }
    fn prepare(&self) -> PlannerPacket {
        self.store()
            .prepare_plan(
                &self.root,
                RequestDraft {
                    objective: "Implement cache persistence graph CLI regression support".into(),
                    query: Some("cache".into()),
                    scope: vec![ScopePath::Directory { path: "src".into() }],
                    constraints: vec!["No Git history mutation".into()],
                    definition_of_done: vec!["Cache API graph CLI regression checks pass".into()],
                    verification: Some(requirements("integration")),
                    invariant_refs: vec![],
                    provenance: PlanningProvenance {
                        actor: "human".into(),
                        source_refs: vec!["objective".into()],
                        provider: None,
                    },
                },
                PlanningLimits::default(),
            )
            .unwrap()
    }
    fn plan(&self) -> ExecutionPlan {
        let p = artifact(&self.prepare());
        self.store().import_execution_plan(&self.root, &p).unwrap();
        self.store()
            .activate_execution_plan(&self.root, &p.packet.plan_id)
            .unwrap();
        p
    }
    fn run(
        &self,
        p: &ExecutionPlan,
        mode: Mode,
        seen: Arc<Mutex<Vec<JobInput>>>,
        fail_check: bool,
    ) -> local::Result<RunRecord> {
        let mut s = self.store();
        Runtime::new(
            &mut s,
            self.paths.clone(),
            self.config.clone(),
            BTreeMap::from([(
                "test".into(),
                Box::new(Fake { mode, seen }) as Box<dyn ProviderAdapter>,
            )]),
        )
        .unwrap()
        .with_check_launcher(Box::new(Checks { fail: fail_check }))
        .run(&self.root, &p.packet.plan_id)
    }
}
fn git(root: &Path, args: &[&str]) {
    let o = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}
fn requirements(name: &str) -> VerificationRequirements {
    VerificationRequirements {
        requirement_refs: vec![name.into()],
        evidence_required: true,
    }
}
fn artifact(prepared: &PlannerPacket) -> ExecutionPlan {
    let tasks: Vec<_> = ["api", "graph", "cli", "regression"]
        .iter()
        .enumerate()
        .map(|(i, name)| TaskPacket {
            version: ProtocolVersion::V1,
            task_id: TaskId::new(format!("task:{i}")).unwrap(),
            objective: format!("Implement cache {name}"),
            read_scope: vec![ScopePath::Directory { path: "src".into() }],
            write_scope: vec![ScopePath::File {
                path: format!("src/{name}.rs"),
            }],
            graph_entities: vec![prepared.context.graph.primary[0].entity.id.clone()],
            invariant_refs: prepared.request.intent.invariant_refs.clone(),
            dependencies: if i == 0 {
                vec![]
            } else {
                vec![TaskId::new(format!("task:{}", if i == 1 { 0 } else { 1 })).unwrap()]
            },
            definition_of_done: vec![format!("{name} works")],
            verification: requirements("unit"),
        })
        .collect();
    let packet = PlanPacket {
        version: ProtocolVersion::V1,
        plan_id: PlanId::new("plan:runtime").unwrap(),
        objective: prepared.request.intent.objective.clone(),
        tasks,
        integration_verification: requirements("integration"),
    };
    ExecutionPlan {
        metadata: PlanMetadata {
            version: ProtocolVersion::V1,
            request_id: prepared.request.request_id.clone(),
            source: prepared.request.source.clone(),
            created_at_ms: local::now_ms().unwrap(),
            provenance: PlanningProvenance {
                actor: "fixture-planner".into(),
                source_refs: vec![prepared.request.request_id.as_str().into()],
                provider: None,
            },
            contracts: packet
                .tasks
                .iter()
                .map(|t| VerificationContract {
                    task_id: t.task_id.clone(),
                    task_packet_hash: hash(t).unwrap(),
                    independent_verifier: true,
                    input: VerifierInput::PacketDiffAndEvidence,
                    memory_refs: vec![],
                    exclusions: vec![],
                    non_goals: vec![],
                })
                .collect(),
            integration: IntegrationVerificationContract {
                plan_id: packet.plan_id.clone(),
                plan_packet_hash: hash(&packet).unwrap(),
                independent_verifier: true,
                require_all_task_verifications: true,
                require_final_diff_and_evidence: true,
                expectations: prepared.request.intent.definition_of_done.clone(),
            },
            replan: None,
        },
        packet,
    }
}
#[derive(Clone)]
enum Mode {
    Panic,
    Usage(TokenUsageProvenance),
    Pass,
    Reject,
    Malformed,
    Crash,
    Timeout,
    WrongJob,
    WrongEvidence,
    Scope,
    /// Performs the in-scope edit, then, outside the write scope, plants a
    /// self-ignoring `.gitignore` that turns a new directory into one Git (and
    /// `git status`) never enters, and hides a file inside it.
    PlantGitignore,
    /// Performs the in-scope edit, then moves the ignore boundary from outside
    /// the worktree: appends a rule to the repository-local `info/exclude` (the
    /// file Git itself resolves) that makes a new directory one Git never enters,
    /// and hides a file inside it.
    HideWithInfoExclude,
    VerifierDrift,
    IntegrationDrift,
    Planner(Box<ExecutionPlan>),
}
struct Fake {
    mode: Mode,
    seen: Arc<Mutex<Vec<JobInput>>>,
}
impl ProviderAdapter for Fake {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            model: true,
            effort: true,
            fresh_session: true,
            structured_output: true,
            token_usage: false,
        }
    }
    fn launch(
        &mut self,
        input: &JobInput,
        process: ProcessSpec,
        _: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        self.seen.lock().unwrap().push(input.clone());
        if matches!(self.mode, Mode::Panic) {
            panic!("simulated abrupt controller loss");
        }
        if matches!(self.mode, Mode::Timeout) {
            return Ok(Box::new(Hang { cancelled: false }));
        }
        if matches!(self.mode, Mode::Crash) {
            return Ok(Box::new(Immediate(Some(ProcessOutput {
                exit: Some(9),
                stdout: vec![],
                stderr: b"fixture crash".to_vec(),
                failure: None,
            }))));
        }
        if matches!(self.mode, Mode::Malformed) {
            return Ok(Box::new(Immediate(Some(success(b"not JSON".to_vec())))));
        }
        let value = match input.role {
            AgentRole::Planner => match &self.mode {
                Mode::Planner(p) => serde_json::to_value(p).unwrap(),
                _ => json!({}),
            },
            AgentRole::Executor => {
                let task: TaskPacket =
                    serde_json::from_value(input.artifact["task"].clone()).unwrap();
                let path = if matches!(self.mode, Mode::Scope) {
                    "outside.txt"
                } else {
                    task.write_scope[0].path()
                };
                let prior = fs::read_to_string(process.workspace.join(path)).unwrap_or_default();
                fs::write(
                    process.workspace.join(path),
                    format!(
                        "{prior}// accepted fixture change for {}\n",
                        task.task_id.as_str()
                    ),
                )
                .unwrap();
                if matches!(self.mode, Mode::PlantGitignore) {
                    let tests = process.workspace.join("tests");
                    fs::create_dir_all(tests.join("unit")).unwrap();
                    fs::write(tests.join(".gitignore"), ".gitignore\nunit/\n").unwrap();
                    fs::write(tests.join("unit/conftest.py"), "import builtins\n").unwrap();
                }
                if matches!(self.mode, Mode::HideWithInfoExclude) {
                    let o = git_output(
                        &process.workspace,
                        &[
                            "rev-parse",
                            "--path-format=absolute",
                            "--git-path",
                            "info/exclude",
                        ],
                    );
                    let exclude = PathBuf::from(String::from_utf8(o.stdout).unwrap().trim_end());
                    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
                    let prior = fs::read_to_string(&exclude).unwrap_or_default();
                    fs::write(&exclude, format!("{prior}/hidden/\n")).unwrap();
                    let hidden = process.workspace.join("hidden");
                    fs::create_dir_all(&hidden).unwrap();
                    fs::write(hidden.join("conftest.py"), "import builtins\n").unwrap();
                }
                serde_json::to_value(ResultPacket {
                    version: ProtocolVersion::V1,
                    task_id: task.task_id,
                    executor_job_id: if matches!(self.mode, Mode::WrongJob) {
                        JobId::new("spoofed:executor").unwrap()
                    } else {
                        input.job_id.clone()
                    },
                    status: ResultStatus::Succeeded,
                    changed_paths: vec![path.into()],
                    changed_entities: vec![],
                    evidence: vec![],
                    notes: None,
                    failure: None,
                })
                .unwrap()
            }
            AgentRole::Verifier => {
                if matches!(self.mode, Mode::VerifierDrift)
                    || matches!(self.mode, Mode::IntegrationDrift) && input.task_id.is_none()
                {
                    fs::write(process.workspace.join("drift.txt"), "external edit").unwrap();
                }
                let reject = matches!(self.mode, Mode::Reject);
                let target: VerificationTarget =
                    serde_json::from_value(input.artifact["target"].clone()).unwrap();
                let requirements = if input.task_id.is_some() {
                    vec!["unit".into()]
                } else {
                    vec!["integration".into()]
                };
                serde_json::to_value(VerificationPacket {
                    version: ProtocolVersion::V1,
                    verification_id: VerificationId::new(format!(
                        "verification:{}",
                        input.job_id.as_str()
                    ))
                    .unwrap(),
                    target,
                    verifier_job_id: input.job_id.clone(),
                    decision: if reject {
                        VerificationDecision::Reject
                    } else {
                        VerificationDecision::Pass
                    },
                    findings: if reject {
                        vec![VerificationFinding {
                            severity: FindingSeverity::Error,
                            requirement_refs: requirements.clone(),
                            invariant_refs: vec![],
                            location: None,
                            problem: "fixture rejection".into(),
                        }]
                    } else {
                        vec![]
                    },
                    evidence: if matches!(self.mode, Mode::WrongEvidence) {
                        vec![EvidenceRef(EvidenceId::new("evidence:spoofed").unwrap())]
                    } else {
                        serde_json::from_value(input.artifact["evidence"].clone()).unwrap()
                    },
                    requirement_refs: requirements,
                    invariant_refs: vec![],
                    notes: None,
                })
                .unwrap()
            }
        };
        Ok(Box::new(Immediate(Some(success(
            serde_json::to_vec(&value).unwrap(),
        )))))
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        Ok(serde_json::from_slice(&output.stdout)?)
    }
    fn usage(&self, _: &ProcessOutput) -> local::Result<Usage> {
        Ok(match self.mode {
            Mode::Usage(provenance) if provenance != TokenUsageProvenance::Unknown => Usage {
                provenance,
                input: Some(17),
                output: Some(5),
                cached: Some(3),
            },
            _ => Usage::default(),
        })
    }
}
fn success(stdout: Vec<u8>) -> ProcessOutput {
    ProcessOutput {
        exit: Some(0),
        stdout,
        stderr: vec![],
        failure: None,
    }
}
struct Immediate(Option<ProcessOutput>);
impl RunningProcess for Immediate {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        Ok(self.0.take())
    }
    fn cancel(&mut self) -> local::Result<CancellationOutcome> {
        self.0 = Some(ProcessOutput {
            exit: None,
            stdout: vec![],
            stderr: vec![],
            failure: Some("cancelled".into()),
        });
        Ok(CancellationOutcome::Applied)
    }
}
struct Hang {
    cancelled: bool,
}
impl RunningProcess for Hang {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        Ok(self.cancelled.then(|| ProcessOutput {
            exit: None,
            stdout: vec![],
            stderr: vec![],
            failure: Some("cancelled".into()),
        }))
    }
    fn cancel(&mut self) -> local::Result<CancellationOutcome> {
        self.cancelled = true;
        Ok(CancellationOutcome::Applied)
    }
}
struct Checks {
    fail: bool,
}
impl CheckLauncher for Checks {
    fn provenance(&self) -> &'static str {
        "DETERMINISTIC_TEST_FIXTURE"
    }
    fn launch(&mut self, spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        assert!(!spec.writable);
        assert!(!spec.network);
        assert!(spec.credential_env.is_empty());
        Ok(Box::new(Immediate(Some(ProcessOutput {
            exit: Some(if self.fail { 1 } else { 0 }),
            stdout: b"captured check output".to_vec(),
            stderr: vec![],
            failure: None,
        }))))
    }
}
fn seen() -> Arc<Mutex<Vec<JobInput>>> {
    Arc::new(Mutex::new(vec![]))
}

#[test]
fn full_diamond_runtime_captures_verifies_refreshes_and_completes() {
    let f = Fixture::new();
    let p = f.plan();
    let inputs = seen();
    let result = f.run(&p, Mode::Pass, inputs.clone(), false).unwrap();
    assert_eq!(result.state, RunState::Complete);
    assert_eq!(result.accepted.len(), 4);
    let inputs = inputs.lock().unwrap();
    assert_eq!(inputs.len(), 9);
    let sessions: std::collections::BTreeSet<_> = inputs.iter().map(|i| &i.session_id).collect();
    assert_eq!(sessions.len(), 9);
    for (index, input) in inputs.iter().enumerate() {
        assert_eq!(
            input.role,
            if index < 8 && index % 2 == 0 {
                AgentRole::Executor
            } else {
                AgentRole::Verifier
            }
        );
        assert_eq!(input.workspace_id, result.workspace_id);
    }
    assert!(
        inputs[2].artifact["files"]
            .to_string()
            .contains("accepted fixture change for task:0")
    );
    assert!(inputs[1].artifact.get("notes").is_none());
    assert!(
        inputs[1].artifact["diff"]["changes"]
            .to_string()
            .contains("accepted fixture change")
    );
    assert_eq!(
        f.store()
            .execution_plan(&f.root, &p.packet.plan_id)
            .unwrap()
            .state,
        PlanState::Complete
    );
    drop(inputs);
    let before = seen();
    assert_eq!(
        f.run(&p, Mode::Pass, before.clone(), false).unwrap().state,
        RunState::Complete
    );
    assert!(before.lock().unwrap().is_empty());
}
#[test]
fn verifier_rejection_stops_a_b_c_d_cascade() {
    let f = Fixture::new();
    let p = f.plan();
    let inputs = seen();
    assert!(f.run(&p, Mode::Reject, inputs.clone(), false).is_err());
    assert_eq!(inputs.lock().unwrap().len(), 2);
    let tasks = f
        .store()
        .tasks(
            &RepositoryInfo::discover(&f.root).unwrap().repository_id,
            Some(&p.packet.plan_id),
        )
        .unwrap();
    assert_eq!(tasks[0].state, TaskState::Rejected);
    assert!(tasks[1..].iter().all(|t| t.state == TaskState::Planned));
}
#[test]
fn executor_failures_and_spoofing_never_launch_dependents() {
    for mode in [Mode::Malformed, Mode::Crash, Mode::WrongJob, Mode::Scope] {
        let f = Fixture::new();
        let p = f.plan();
        let inputs = seen();
        assert!(f.run(&p, mode, inputs.clone(), false).is_err());
        assert_eq!(inputs.lock().unwrap().len(), 1);
        assert_eq!(
            f.store()
                .runtime_status(&f.root, &p.packet.plan_id)
                .unwrap()
                .unwrap()
                .state,
            RunState::Blocked
        );
    }
}
#[test]
fn forged_evidence_and_source_drift_cannot_verify() {
    for mode in [
        Mode::WrongEvidence,
        Mode::VerifierDrift,
        Mode::IntegrationDrift,
    ] {
        let f = Fixture::new();
        let p = f.plan();
        assert!(f.run(&p, mode, seen(), false).is_err());
        assert_eq!(
            f.store()
                .execution_plan(&f.root, &p.packet.plan_id)
                .unwrap()
                .state,
            PlanState::Active
        );
    }
}
#[test]
fn source_changes_after_activation_block_before_launch() {
    let f = Fixture::new();
    let p = f.plan();
    fs::write(f.root.join("src/api.rs"), "pub fn changed(){}\n").unwrap();
    let inputs = seen();
    assert!(f.run(&p, Mode::Pass, inputs.clone(), false).is_err());
    assert!(inputs.lock().unwrap().is_empty());
    let run = f
        .store()
        .runtime_status(&f.root, &p.packet.plan_id)
        .unwrap()
        .unwrap();
    assert_eq!(run.state, RunState::Blocked);
    assert!(run.reason.unwrap().contains("SOURCE_DRIFT"));
}
#[test]
fn deterministic_check_failure_cannot_be_overridden_by_model() {
    let f = Fixture::new();
    let p = f.plan();
    let inputs = seen();
    assert!(f.run(&p, Mode::Pass, inputs.clone(), true).is_err());
    assert_eq!(inputs.lock().unwrap().len(), 1);
}
#[test]
fn provider_timeout_is_terminal_and_resume_does_not_retry() {
    let mut f = Fixture::new();
    f.config.timeout_ms = 10;
    let p = f.plan();
    assert!(f.run(&p, Mode::Timeout, seen(), false).is_err());
    let inputs = seen();
    assert!(f.run(&p, Mode::Pass, inputs.clone(), false).is_err());
    assert!(inputs.lock().unwrap().is_empty());
}
#[test]
fn planner_output_reuses_stage4_import_and_never_activates_partially() {
    for valid in [true, false] {
        let f = Fixture::new();
        let prepared = f.prepare();
        let p = artifact(&prepared);
        let mut store = f.store();
        let input = seen();
        let mode = if valid {
            Mode::Planner(Box::new(p.clone()))
        } else {
            Mode::Malformed
        };
        let mut runtime = Runtime::new(
            &mut store,
            f.paths.clone(),
            f.config.clone(),
            BTreeMap::from([(
                "test".into(),
                Box::new(Fake {
                    mode,
                    seen: input.clone(),
                }) as Box<dyn ProviderAdapter>,
            )]),
        )
        .unwrap();
        let result = runtime.plan(&f.root, &prepared.request.request_id);
        assert_eq!(result.is_ok(), valid);
        if let Ok(v) = result {
            assert_eq!(v.state, PlanState::Validated);
        }
        assert_eq!(input.lock().unwrap()[0].role, AgentRole::Planner);
    }
}
/// Issue #1 through the runtime: a literal framework path is captured, written by
/// the executor, reported, diffed and verified as exactly that path, and a
/// sibling a pattern reading of `[slug]` would match is never touched.
#[test]
fn runtime_captures_executes_and_verifies_literal_framework_paths() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("src/app/[slug]")).unwrap();
    fs::create_dir_all(f.root.join("src/app/s")).unwrap();
    fs::write(
        f.root.join("src/app/[slug]/page.ts"),
        "export function cacheSlugPage(): number { return 1; }\n",
    )
    .unwrap();
    fs::write(
        f.root.join("src/app/s/page.ts"),
        "export function cacheDecoyPage(): number { return 2; }\n",
    )
    .unwrap();
    git(&f.root, &["add", "."]);
    git(&f.root, &["commit", "--quiet", "-m", "literal routes"]);
    assert_eq!(f.store().index_repository(&f.root).unwrap().failed, 0);
    let prepared = f.prepare();
    let mut p = artifact(&prepared);
    p.packet.tasks.truncate(1);
    p.packet.tasks[0].write_scope = vec![ScopePath::File {
        path: "src/app/[slug]/page.ts".into(),
    }];
    p.metadata.contracts.truncate(1);
    p.metadata.contracts[0].task_packet_hash = hash(&p.packet.tasks[0]).unwrap();
    p.metadata.integration.plan_packet_hash = hash(&p.packet).unwrap();
    f.store().import_execution_plan(&f.root, &p).unwrap();
    f.store()
        .activate_execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    let inputs = seen();
    let run = f.run(&p, Mode::Pass, inputs.clone(), false).unwrap();
    assert_eq!(run.state, RunState::Complete);
    let inputs = inputs.lock().unwrap();
    let verifier = inputs
        .iter()
        .find(|i| i.role == AgentRole::Verifier && i.task_id.is_some())
        .unwrap();
    let changes = verifier.artifact["diff"]["changes"].as_array().unwrap();
    let paths: Vec<&str> = changes
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["src/app/[slug]/page.ts"]);
    assert!(
        fs::read_to_string(f.root.join("src/app/[slug]/page.ts"))
            .unwrap()
            .contains("accepted fixture change for task:0")
    );
    assert_eq!(
        fs::read_to_string(f.root.join("src/app/s/page.ts")).unwrap(),
        "export function cacheDecoyPage(): number { return 2; }\n"
    );
    // The accepted change re-indexes under its literal path.
    let found = f
        .store()
        .graph(&f.root)
        .unwrap()
        .entities_in_file("src/app/[slug]/page.ts", 10)
        .unwrap()
        .data;
    assert!(found.iter().any(|e| e.name == "cacheSlugPage"));
}
/// Every provider job records a deterministic manifest of what agentctl issued:
/// its byte accounting reproduces the compiled prompt exactly, each category is
/// the size of that part of the issued input, and no repository content is
/// copied into it.
#[test]
fn provider_jobs_record_exact_content_free_context_manifests() {
    let f = Fixture::new();
    fs::write(
        f.root.join("src/api.rs"),
        "pub fn cache_api() { let _ = \"MANIFEST_SOURCE_CANARY\"; }\n",
    )
    .unwrap();
    git(&f.root, &["add", "."]);
    git(&f.root, &["commit", "--quiet", "-m", "canary"]);
    f.store().index_repository(&f.root).unwrap();
    let prepared = f.prepare();
    let p = artifact(&prepared);
    let inputs = seen();
    {
        let mut store = f.store();
        Runtime::new(
            &mut store,
            f.paths.clone(),
            f.config.clone(),
            BTreeMap::from([(
                "test".into(),
                Box::new(Fake {
                    mode: Mode::Planner(Box::new(p.clone())),
                    seen: inputs.clone(),
                }) as Box<dyn ProviderAdapter>,
            )]),
        )
        .unwrap()
        .plan(&f.root, &prepared.request.request_id)
        .unwrap();
    }
    f.store()
        .activate_execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    assert_eq!(
        f.run(&p, Mode::Pass, inputs.clone(), false).unwrap().state,
        RunState::Complete
    );
    let inputs = inputs.lock().unwrap();
    let jobs = f.store().runtime_jobs(&f.root, None).unwrap();
    let mut roles = std::collections::BTreeSet::new();
    for job in &jobs {
        let m = job.context_manifest.as_ref().expect("issued job manifest");
        let prompt = job.prompt.as_ref().unwrap();
        let input = inputs.iter().find(|i| i.job_id == job.job_id).unwrap();
        roles.insert(format!("{:?}", job.role));
        assert_eq!(m.version, manifest::MANIFEST_VERSION);
        assert_eq!(
            (
                m.role,
                m.job_id.as_ref(),
                m.plan_id.as_ref(),
                m.task_id.as_ref(),
                m.request_id.as_ref()
            ),
            (
                job.role,
                Some(&job.job_id),
                job.plan_id.as_ref(),
                job.task_id.as_ref(),
                job.request_id.as_ref()
            )
        );
        let compiled = &input.compiled.as_ref().unwrap().bytes;
        assert_eq!(
            (m.bytes.total, prompt.bytes),
            (compiled.len(), compiled.len())
        );
        assert_eq!(
            m.bytes.categories.iter().map(|c| c.bytes).sum::<usize>(),
            compiled.len()
        );
        assert_eq!(
            m.bytes.provider_hidden,
            manifest::HiddenContext::NotObserved
        );
        let value = serde_json::to_value(input).unwrap();
        for c in m
            .bytes
            .categories
            .iter()
            .filter(|c| c.category != "framing" && c.category != "instructions")
        {
            let part = c.category.split('.').fold(&value, |v, key| &v[key]);
            assert_eq!(
                serde_json::to_vec(part).unwrap().len(),
                c.bytes,
                "{}",
                c.category
            );
        }
        assert!(
            !serde_json::to_string(m)
                .unwrap()
                .contains("MANIFEST_SOURCE_CANARY")
        );
        match job.role {
            AgentRole::Planner => {
                assert_eq!(m.graph_generation, prepared.request.source.graph_generation);
                assert!(m.graph_generation.is_some());
                assert!(
                    m.bytes
                        .categories
                        .iter()
                        .any(|c| c.category == "artifact.planner_packet.context.graph.relations")
                );
                assert!(
                    m.paths
                        .iter()
                        .any(|s| s.kind == manifest::SuppliedKind::GraphFacts)
                );
                assert_eq!(
                    m,
                    &manifest::for_job(
                        input,
                        job.request_id.as_ref(),
                        prompt,
                        manifest::ContextInventory::planner(&prepared)
                    )
                    .unwrap()
                );
            }
            AgentRole::Executor => {
                assert!(String::from_utf8_lossy(compiled).contains("MANIFEST_SOURCE_CANARY"));
                assert!(m.paths.iter().any(|s| s.path == "src/api.rs"
                    && s.kind == manifest::SuppliedKind::File
                    && s.content_hash.is_some()));
                assert!(m.graph_generation.is_some());
                assert!(
                    m.bytes
                        .categories
                        .iter()
                        .any(|c| c.category == "artifact.files")
                );
            }
            AgentRole::Verifier => {
                assert!(!m.paths.is_empty());
                assert!(
                    m.paths
                        .iter()
                        .all(|s| s.kind == manifest::SuppliedKind::Diff)
                );
                assert!(
                    m.bytes
                        .categories
                        .iter()
                        .any(|c| c.category.starts_with("artifact.diff."))
                );
            }
        }
    }
    assert_eq!(roles.len(), 3);
    // Jobs recorded before manifests existed remain readable.
    let mut legacy = serde_json::to_value(&jobs[0]).unwrap();
    legacy.as_object_mut().unwrap().remove("context_manifest");
    let job: RuntimeJob = serde_json::from_value(legacy).unwrap();
    assert!(job.context_manifest.is_none());
}
/// `--bytes` only bounds the frozen `PlannerPacket` (selected graph/memory/excerpt
/// content). `run planner` separately calls `source::capture`, which walks and reads
/// *every* file in the workspace tree (for provenance/journaling). `source::capture`
/// bounds capture by the real invariant -- 64 MiB / 20000 files in aggregate -- rather
/// than an arbitrary fixed per-file ceiling, so an irrelevant file larger than the old
/// 2 MiB cap no longer fails a capture that comfortably fits the aggregate budget.
/// Regression for the producer/runtime mismatch in agentctl issue #2 (Stage 2B).
#[test]
fn run_planner_succeeds_with_irrelevant_oversized_workspace_file_in_budget() {
    let f = Fixture::new();
    // Unrecognized extension: invisible to graph indexing/freshness (Language::for_path
    // returns None), yet source::capture still observes it for full workspace identity.
    fs::write(
        f.root.join("src/oversized.bin"),
        vec![0u8; 2 * 1024 * 1024 + 1],
    )
    .unwrap();
    let prepared = f.prepare();
    assert!(
        prepared.serialized_bytes < PlanningLimits::default().bytes,
        "frozen packet stays well under the 32 KiB budget: {}",
        prepared.serialized_bytes
    );
    let p = artifact(&prepared);
    let mut store = f.store();
    Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p)),
                seen: seen(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap();
}
/// Capture must stay bounded: a single file that alone exceeds the 64 MiB aggregate
/// workspace budget still fails closed. Regression for agentctl issue #2 (Stage 2C):
/// this used to surface as the same ambiguous "runtime file/artifact exceeds size
/// limit" message shared with git-index reads and CAS artifact readback, which was
/// impossible to distinguish from a `PlannerPacket`/artifact size problem. The
/// diagnostic must now name workspace/source capture specifically and report the
/// governing byte limit, the offending path, and the offending file's actual size.
#[test]
fn run_planner_rejects_workspace_exceeding_aggregate_capture_budget() {
    let f = Fixture::new();
    fs::write(f.root.join("src/huge.bin"), vec![0u8; 64 * 1024 * 1024 + 1]).unwrap();
    let prepared = f.prepare();
    let p = artifact(&prepared);
    let mut store = f.store();
    let err = Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p)),
                seen: seen(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("runtime workspace capture") && message.contains("67108864-byte limit"),
        "diagnostic must name workspace/source capture and the 64 MiB governing limit, not a generic artifact/size message: {message}"
    );
    assert!(
        message.contains("src/huge.bin"),
        "diagnostic must name the offending workspace-relative path: {message}"
    );
    assert!(
        message.contains("file=67108865"),
        "diagnostic must report the offending file's actual size: {message}"
    );
}
/// The workspace file-count ceiling is a distinct failure mode from the aggregate
/// byte budget above: same subsystem, different dimension. Its diagnostic must name
/// workspace capture, the governing 20000-file limit, and the observed count -- not
/// collapse into the byte-budget message or the generic artifact/size message.
#[test]
fn run_planner_rejects_workspace_exceeding_file_count_limit() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("src/many")).unwrap();
    for i in 0..=20_000 {
        fs::write(f.root.join(format!("src/many/f{i}.txt")), b"x").unwrap();
    }
    let prepared = f.prepare();
    let p = artifact(&prepared);
    let mut store = f.store();
    let err = Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p)),
                seen: seen(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("runtime workspace capture") && message.contains("20000-file limit"),
        "diagnostic must name workspace capture and the governing file-count limit, not a generic message: {message}"
    );
    assert!(
        message.contains("count=20000"),
        "diagnostic must report the observed file count: {message}"
    );
}
/// Mirrors the byte/file totals `source::collect` would observe for a
/// workspace (excluding the top-level `.git` directory), so boundary fixtures
/// below can land exactly on a limit rather than guessing. Not a
/// reimplementation of capture's scope/symlink/protected-path checks --
/// fixtures here are plain files with no symlinks or protected paths, and no
/// Git-ignored files, so every file on disk outside `.git` is observed source.
fn workspace_totals(root: &Path) -> (usize, u64) {
    let mut count = 0usize;
    let mut bytes = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.parent() == Some(root) && path.file_name().unwrap() == ".git" {
                continue;
            }
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                pending.push(path);
            } else {
                count += 1;
                bytes += meta.len();
            }
        }
    }
    (count, bytes)
}
/// Parses the first run of ASCII digits following `prefix` in a diagnostic
/// message, e.g. `field(msg, "captured=")` for `"...(captured=123, file=456)"`.
fn field(message: &str, prefix: &str) -> u64 {
    let start = message.find(prefix).unwrap_or_else(|| {
        panic!("diagnostic is missing {prefix:?}: {message}");
    }) + prefix.len();
    message[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap()
}
/// Grows `.git/index` past `target_bytes` by feeding `git update-index
/// --index-info` synthetic entries that reuse the repository's empty-blob
/// object and name paths that are never created on disk. Git accepts and
/// stages these without ever touching the working tree, so repository
/// discovery (`rev-parse`, `status`) keeps succeeding right up to the read
/// that must fail -- reaching the GIT_INDEX_BYTES diagnostic deterministically
/// and in well under a second, unlike staging hundreds of thousands of real
/// files or replacing `.git/index` with bytes Git itself would refuse to
/// parse. Paths stay at a single 250-byte component (under the 255-byte
/// filesystem name limit) so `status` never has to stat a name the OS would
/// reject as too long.
fn inflate_git_index_past(root: &Path, target_bytes: u64) {
    use std::io::Write;
    let blob = Command::new("git")
        .current_dir(root)
        .args(["hash-object", "-w", "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        blob.status.success(),
        "{}",
        String::from_utf8_lossy(&blob.stderr)
    );
    let blob = String::from_utf8(blob.stdout).unwrap();
    let blob = blob.trim_end();
    const PATH_LEN: usize = 250;
    // Empirically, a 250-byte path yields a ~320-byte index entry (a 62-byte
    // fixed header plus the NUL-terminated path, padded to a multiple of 8).
    // The +1000-entry margin absorbs that estimate comfortably past the target.
    const ENTRY_BYTES: u64 = 320;
    let entries = target_bytes / ENTRY_BYTES + 1000;
    let mut feed = String::new();
    for i in 0..entries {
        let prefix = format!("p{i:07}");
        feed.push_str("100644 ");
        feed.push_str(blob);
        feed.push('\t');
        feed.push_str(&prefix);
        feed.push_str(&"x".repeat(PATH_LEN - prefix.len()));
        feed.push('\n');
    }
    let mut child = Command::new("git")
        .current_dir(root)
        .args(["update-index", "--index-info"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(feed.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}
/// The `.git/index` read during workspace capture shares `read_file`'s bounded
/// reader with ordinary source files, but its size ceiling is a distinct
/// invariant from the aggregate workspace-content budget (Stage 2C). This
/// grows the on-disk index past its 16 MiB ceiling with synthetic entries that
/// are never realized as real files, proving the diagnostic path is reachable
/// without replacing `.git/index` with something Git itself would refuse to
/// open (which would surface as a generic Git discovery failure long before
/// `source::capture` ever inspects the index's size).
#[test]
fn run_planner_reports_git_index_diagnostic_for_oversized_index() {
    let f = Fixture::new();
    let prepared = f.prepare();
    let p = artifact(&prepared);
    inflate_git_index_past(&f.root, 16 * 1024 * 1024);
    let index_bytes = fs::metadata(f.root.join(".git/index")).unwrap().len();
    assert!(index_bytes > 16 * 1024 * 1024);
    let mut store = f.store();
    let err = Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p)),
                seen: seen(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("runtime git index read") && message.contains("16777216-byte limit"),
        "diagnostic must name the git index read and the governing 16 MiB limit, not a generic or workspace-capture message: {message}"
    );
    assert!(
        message.contains(&format!("size={index_bytes}")),
        "diagnostic must report the observed index size: {message}"
    );
}
/// Aggregation, not any single file, drives the workspace-capture budget:
/// three files that would each comfortably fit alone -- all well past the
/// obsolete 2 MiB per-file ceiling Stage 2B removed -- must still fail closed
/// once their combined content crosses the real 64 MiB aggregate.
/// `fs::read_dir` order is unspecified, so this asserts only the arithmetic
/// invariant the check enforces (captured-so-far plus the offending file's
/// size is what crossed the budget) and that the offending file is one of the
/// three created here, rather than assuming a specific traversal order.
#[test]
fn run_planner_rejects_workspace_where_several_valid_files_exceed_aggregate_in_sum() {
    let f = Fixture::new();
    const MIB: u64 = 1024 * 1024;
    let sizes = [30 * MIB, 30 * MIB, 10 * MIB]; // 70 MiB combined > 64 MiB aggregate
    for (i, size) in sizes.iter().enumerate() {
        fs::write(
            f.root.join(format!("src/big{i}.bin")),
            vec![0u8; *size as usize],
        )
        .unwrap();
    }
    let prepared = f.prepare();
    let p = artifact(&prepared);
    let mut store = f.store();
    let err = Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p)),
                seen: seen(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("runtime workspace capture") && message.contains("67108864-byte limit"),
        "diagnostic must name workspace capture and the 64 MiB aggregate limit: {message}"
    );
    let captured = field(&message, "captured=");
    let file = field(&message, "file=");
    assert!(
        sizes.contains(&file),
        "offending file size must be one of the created files: file={file} sizes={sizes:?}"
    );
    assert!(
        captured <= 64 * MIB,
        "captured-so-far must itself stay within budget: captured={captured}"
    );
    assert!(
        captured + file > 64 * MIB,
        "aggregation invariant violated: captured-so-far plus the offending file must be what crossed the budget: captured={captured} file={file}"
    );
}
/// The aggregate byte budget is a `<=` boundary: exactly `WORKSPACE_CAPTURE_BYTES`
/// of real content must be accepted. The fixture's own baseline files already
/// consume part of that budget, so the filler file here is sized to land the
/// *workspace total* exactly on the boundary, not just the filler file alone.
#[test]
fn run_planner_accepts_workspace_at_exact_aggregate_byte_boundary() {
    let f = Fixture::new();
    const TARGET: u64 = 64 * 1024 * 1024;
    let (_, existing) = workspace_totals(&f.root);
    assert!(existing < TARGET);
    fs::write(
        f.root.join("src/filler.bin"),
        vec![0u8; (TARGET - existing) as usize],
    )
    .unwrap();
    let (_, total) = workspace_totals(&f.root);
    assert_eq!(
        total, TARGET,
        "fixture must land exactly on the 64 MiB boundary"
    );
    let prepared = f.prepare();
    let p = artifact(&prepared);
    let mut store = f.store();
    Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p)),
                seen: seen(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap();
}
/// One byte past the same boundary must fail closed. Because the workspace's
/// total content is exactly `WORKSPACE_CAPTURE_BYTES + 1` regardless of
/// `read_dir` order, the check's own running-total arithmetic guarantees
/// `captured + file` always equals that same total -- proof sketch: if the
/// first `k` files processed already summed past the budget while any earlier
/// prefix did not, and the full workspace exceeds the budget by exactly one
/// byte, the unprocessed remainder (if any) would have to contribute a
/// negative amount to close that gap, which is impossible, so the failing
/// file is always the traversal's last one and the two fields always sum to
/// budget+1. This asserts that arithmetic invariant rather than a specific
/// offending filename.
#[test]
fn run_planner_rejects_workspace_one_byte_past_aggregate_byte_boundary() {
    let f = Fixture::new();
    const TARGET: u64 = 64 * 1024 * 1024;
    let (_, existing) = workspace_totals(&f.root);
    assert!(existing < TARGET);
    fs::write(
        f.root.join("src/filler.bin"),
        vec![0u8; (TARGET - existing + 1) as usize],
    )
    .unwrap();
    let (_, total) = workspace_totals(&f.root);
    assert_eq!(total, TARGET + 1);
    let prepared = f.prepare();
    let p = artifact(&prepared);
    let mut store = f.store();
    let err = Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p)),
                seen: seen(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("runtime workspace capture") && message.contains("67108864-byte limit"),
        "{message}"
    );
    let captured = field(&message, "captured=");
    let file = field(&message, "file=");
    assert_eq!(
        captured + file,
        TARGET + 1,
        "regardless of traversal order, captured+file must equal the fixture's exact one-byte overage: {message}"
    );
}
/// The file-count ceiling is also a boundary the runtime must accept exactly
/// at, not just reject one past (the existing regression above already proves
/// the reject side: 20001 files observes `count=20000` and fails). This
/// constructs a workspace with exactly 20000 files in total, baseline files
/// included, and confirms capture -- and the whole planner job-creation path
/// -- succeeds.
#[test]
fn run_planner_accepts_workspace_at_exact_file_count_boundary() {
    let f = Fixture::new();
    const TARGET: usize = 20_000;
    let (existing, _) = workspace_totals(&f.root);
    assert!(existing < TARGET);
    fs::create_dir_all(f.root.join("src/many")).unwrap();
    for i in 0..(TARGET - existing) {
        fs::write(f.root.join(format!("src/many/f{i}.txt")), b"x").unwrap();
    }
    let (count, _) = workspace_totals(&f.root);
    assert_eq!(
        count, TARGET,
        "fixture must land exactly on the 20000-file boundary"
    );
    let prepared = f.prepare();
    let p = artifact(&prepared);
    let mut store = f.store();
    Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p)),
                seen: seen(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap();
}
/// Combined Stage 2E acceptance for Issue #2's originally confusing symptoms:
/// a `PlannerPacket` sized close to its configured planning byte budget (not
/// trivially small, as in the Stage 2B regression above) must still be
/// produced, frozen, and consumed by the runtime -- reaching planner job
/// creation -- even though the workspace separately contains an unrelated
/// file well past the old, now-removed 2 MiB per-file ceiling but comfortably
/// inside the real 64 MiB aggregate.
#[test]
fn run_planner_reaches_job_creation_with_near_limit_packet_and_in_budget_oversized_file() {
    let f = Fixture::new();
    let natural = f.prepare().serialized_bytes;
    let limits = PlanningLimits {
        bytes: (natural + 32).clamp(4096, 131072),
        ..PlanningLimits::default()
    };
    // Unrelated to packet sizing: an in-budget file past the old 2 MiB ceiling
    // must not affect workspace capture during the same `run planner` call.
    fs::write(
        f.root.join("src/oversized.bin"),
        vec![0u8; 2 * 1024 * 1024 + 1],
    )
    .unwrap();
    let prepared = f
        .store()
        .prepare_plan(
            &f.root,
            RequestDraft {
                objective: "Implement cache persistence graph CLI regression support".into(),
                query: Some("cache".into()),
                scope: vec![ScopePath::Directory { path: "src".into() }],
                constraints: vec!["No Git history mutation".into()],
                definition_of_done: vec!["Cache API graph CLI regression checks pass".into()],
                verification: Some(requirements("integration")),
                invariant_refs: vec![],
                provenance: PlanningProvenance {
                    actor: "human".into(),
                    source_refs: vec!["objective".into()],
                    provider: None,
                },
            },
            limits,
        )
        .unwrap();
    assert!(prepared.serialized_bytes <= limits.bytes);
    assert!(
        prepared.serialized_bytes as f64 >= limits.bytes as f64 * 0.9,
        "packet should sit near its configured budget, not far under it: {} of {}",
        prepared.serialized_bytes,
        limits.bytes
    );
    let p = artifact(&prepared);
    let mut store = f.store();
    let view = Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p)),
                seen: seen(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap();
    assert_eq!(view.state, PlanState::Validated);
}
/// Content-addressed artifact readback shares `read_regular_file` with workspace
/// capture and git-index reads, but its size ceiling is a distinct, unrelated
/// invariant (the CAS's own bound on a single stored blob, keyed by the size
/// recorded at write time). A corrupted/oversized `ArtifactRef` must fail with a
/// diagnostic naming artifact readback -- not the shared generic read error, and
/// not the workspace-capture or `PlannerPacket` messages -- and it must fail before
/// ever opening the (possibly nonexistent) backing file, since the recorded size
/// alone is enough to know the read is refused.
#[test]
fn artifact_readback_reports_its_own_limit_without_reading_content() {
    let f = Fixture::new();
    let artifacts = Artifacts::new(&f.paths.data_root.join("test-artifacts")).unwrap();
    let oversized = ArtifactRef {
        hash: format!("blake3:{}", "a".repeat(64)),
        bytes: 64 * 1024 * 1024 + 1,
    };
    let message = artifacts.get(&oversized).unwrap_err().to_string();
    assert!(
        message.contains("runtime artifact readback") && message.contains("67108864-byte limit"),
        "diagnostic must name artifact readback and the governing 64 MiB limit: {message}"
    );
    assert!(
        message.contains("recorded=67108865"),
        "diagnostic must report the recorded/expected size: {message}"
    );
}
/// A large-but-in-budget file is captured with its real content identity (not
/// excluded, not represented as bounded metadata only) -- so a later change to it is
/// still caught as source drift, exactly like any other tracked file.
#[test]
fn oversized_workspace_file_changes_are_still_detected_as_source_drift() {
    let f = Fixture::new();
    fs::write(
        f.root.join("src/oversized.bin"),
        vec![0u8; 2 * 1024 * 1024 + 1],
    )
    .unwrap();
    git(&f.root, &["add", "src/oversized.bin"]);
    git(&f.root, &["commit", "--quiet", "-m", "add oversized file"]);
    f.store().index_repository(&f.root).unwrap();
    let p = f.plan();
    fs::write(
        f.root.join("src/oversized.bin"),
        vec![1u8; 2 * 1024 * 1024 + 1],
    )
    .unwrap();
    let inputs = seen();
    assert!(f.run(&p, Mode::Pass, inputs.clone(), false).is_err());
    assert!(inputs.lock().unwrap().is_empty());
    let run = f
        .store()
        .runtime_status(&f.root, &p.packet.plan_id)
        .unwrap()
        .unwrap();
    assert_eq!(run.state, RunState::Blocked);
    assert!(run.reason.unwrap().contains("SOURCE_DRIFT"));
}
/// Creates a sparse file: it has the given apparent size (which is what workspace
/// capture budgets) without costing disk space or I/O to create.
fn sparse(path: &Path, bytes: u64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::File::create(path).unwrap().set_len(bytes).unwrap();
}
/// Plain Git with host configuration (including any `core.excludesFile`) disabled.
fn git_output(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(root)
        .args(["-c", "core.excludesFile=/dev/null"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
}
/// Git's own verdict, independent of agentctl, on whether a path is ignored.
fn git_ignored(root: &Path, path: &str) -> bool {
    git_output(root, &["check-ignore", "--quiet", "--", path])
        .status
        .success()
}
/// Total size of the files Git itself lists as tracked, plus untracked and not
/// ignored: an oracle independent of agentctl's capture. It is exact for
/// fixtures with no individually ignored files, whose only ignored entries are
/// whole directories.
fn git_source_bytes(root: &Path) -> u64 {
    let o = git_output(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    o.stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| {
            fs::metadata(root.join(std::str::from_utf8(p).unwrap()))
                .unwrap()
                .len()
        })
        .sum()
}
/// `agentctl run planner` for the fixture's standard request through the real
/// runtime: workspace capture, planner job creation, invocation, and plan import.
fn run_planner(f: &Fixture) -> local::Result<ExecutionPlanView> {
    let prepared = f.prepare();
    let p = artifact(&prepared);
    let mut store = f.store();
    Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p)),
                seen: seen(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
}
/// Regression for agentctl issue #3. While dogfooding, a ~124 MiB Git-ignored
/// `target/debug/libagentctl.rlib` exhausted the 64 MiB workspace capture budget
/// and blocked `agentctl run planner`, even though the frozen planner context was
/// valid. Ignored content now never costs capture budget, however large. That
/// holds both inside a directory the repository ignores as a whole (never walked)
/// and for an individually ignored file (observed by metadata only). Both rules
/// live in a nested, committed `.gitignore`, under names that look nothing like
/// build output. The ignored directory's other entries mimic the rest of a real
/// `target/`: a hardlinked binary and a symlink, which capture refuses wherever
/// it reads content.
#[test]
fn run_planner_ignores_git_ignored_artifact_larger_than_capture_budget() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("data")).unwrap();
    fs::write(f.root.join("data/.gitignore"), "/snapshot/\n*.pack\n").unwrap();
    sparse(&f.root.join("data/local.pack"), 128 * 1024 * 1024);
    assert!(git_ignored(&f.root, "data/local.pack"));
    git(&f.root, &["add", "data/.gitignore"]);
    git(
        &f.root,
        &["commit", "--quiet", "-m", "ignore local snapshots"],
    );
    f.store().index_repository(&f.root).unwrap();
    sparse(
        &f.root.join("data/snapshot/archive.pack"),
        128 * 1024 * 1024,
    );
    #[cfg(unix)]
    {
        let snapshot = f.root.join("data/snapshot");
        fs::write(snapshot.join("tool"), "x").unwrap();
        fs::hard_link(snapshot.join("tool"), snapshot.join("tool-3f2a")).unwrap();
        std::os::unix::fs::symlink("/", snapshot.join("root")).unwrap();
    }
    assert!(
        git_ignored(&f.root, "data/snapshot/archive.pack"),
        "Git itself must classify the artifact as ignored"
    );
    assert_eq!(run_planner(&f).unwrap().state, PlanState::Validated);
}
/// Control for the test above: the identical artifact at the identical path,
/// without the ignore rule, is included source and still fails closed on the
/// unchanged aggregate budget. So it is the repository's ignore rule, not the
/// artifact's name or location, that removes it from capture.
#[test]
fn run_planner_still_rejects_the_same_artifact_when_it_is_not_ignored() {
    let f = Fixture::new();
    sparse(
        &f.root.join("data/snapshot/archive.pack"),
        128 * 1024 * 1024,
    );
    assert!(!git_ignored(&f.root, "data/snapshot/archive.pack"));
    let message = run_planner(&f).unwrap_err().to_string();
    assert!(
        message.contains("runtime workspace capture") && message.contains("67108864-byte limit"),
        "{message}"
    );
    assert!(
        message.contains("data/snapshot/archive.pack") && message.contains("file=134217728"),
        "{message}"
    );
}
/// Excluding ignored files does not relax the budget for included ones. Included
/// files that together exceed 64 MiB still fail closed, and the diagnostic's
/// running total counts only included content: the 128 MiB ignored artifact
/// alongside contributes nothing. Capture proceeds in sorted path order, so the
/// offending file (the last included path) is deterministic.
#[test]
fn run_planner_rejects_included_files_exceeding_budget_alongside_ignored_artifacts() {
    let f = Fixture::new();
    const MIB: u64 = 1024 * 1024;
    fs::write(f.root.join(".gitignore"), "/cache/\n").unwrap();
    sparse(&f.root.join("cache/blob.bin"), 128 * MIB);
    sparse(&f.root.join("src/zz-a.bin"), 40 * MIB);
    sparse(&f.root.join("src/zz-b.bin"), 30 * MIB);
    assert!(git_ignored(&f.root, "cache/blob.bin"));
    let included = git_source_bytes(&f.root);
    assert!(included > 64 * MIB && included < 128 * MIB);
    let message = run_planner(&f).unwrap_err().to_string();
    assert!(
        message.contains("runtime workspace capture") && message.contains("67108864-byte limit"),
        "{message}"
    );
    assert!(message.contains("while reading src/zz-b.bin"), "{message}");
    assert_eq!(field(&message, "file="), 30 * MIB, "{message}");
    assert_eq!(
        field(&message, "captured=") + 30 * MIB,
        included,
        "the running total must count exactly the included files: {message}"
    );
}
/// The whole executor, check, and verifier pipeline completes with a huge ignored
/// build artifact in the checkout (the dogfood shape), and drift in a captured
/// file still blocks verification.
#[test]
fn runtime_completes_with_ignored_artifacts_and_still_blocks_captured_drift() {
    for (mode, complete) in [(Mode::Pass, true), (Mode::VerifierDrift, false)] {
        let f = Fixture::new();
        fs::write(f.root.join(".gitignore"), "/target/\n").unwrap();
        git(&f.root, &["add", ".gitignore"]);
        git(&f.root, &["commit", "--quiet", "-m", "ignore build output"]);
        f.store().index_repository(&f.root).unwrap();
        sparse(
            &f.root.join("target/debug/libagentctl.rlib"),
            124 * 1024 * 1024,
        );
        let p = f.plan();
        let result = f.run(&p, mode, seen(), false);
        if complete {
            assert_eq!(result.unwrap().state, RunState::Complete);
        } else {
            let message = result.unwrap_err().to_string();
            assert!(message.contains("SOURCE_DRIFT"), "{message}");
            assert_eq!(
                f.store()
                    .execution_plan(&f.root, &p.packet.plan_id)
                    .unwrap()
                    .state,
                PlanState::Active
            );
        }
    }
}
/// An executor that hides a new file inside a directory it has just made opaque,
/// using a self-ignoring `.gitignore` outside its write scope, is still caught.
/// The hidden file is never walked, but the planted rule file is, so the captured
/// diff carries exactly that scope violation even though `git status` shows
/// nothing. Skipping ignored directories must not open a blind spot the executor
/// can create for itself.
#[test]
fn executor_planting_a_self_ignoring_gitignore_is_a_scope_violation() {
    let f = Fixture::new();
    let p = f.plan();
    let message = f
        .run(&p, Mode::PlantGitignore, seen(), false)
        .unwrap_err()
        .to_string();
    assert!(message.contains("outside allowed scope"), "{message}");
    assert!(
        git_ignored(&f.root, "tests/.gitignore") && git_ignored(&f.root, "tests/unit/conftest.py"),
        "Git itself must no longer report the planted files"
    );
    let status = git_output(&f.root, &["status", "--porcelain", "--untracked-files=all"]);
    assert!(
        !String::from_utf8_lossy(&status.stdout).contains("tests/"),
        "git status must not report the plant"
    );
    let run = f
        .store()
        .runtime_status(&f.root, &p.packet.plan_id)
        .unwrap()
        .unwrap();
    assert_eq!(run.state, RunState::Blocked);
    let hash = f
        .store()
        .events(None, None, None, 1000)
        .unwrap()
        .iter()
        .find_map(|e| match &e.entry {
            local::store::JournalEntry::Runtime { phase, detail, .. }
                if phase == "DIFF_CAPTURED" =>
            {
                Some(detail.clone())
            }
            _ => None,
        })
        .unwrap();
    let diff: CapturedDiff = serde_json::from_slice(
        &fs::read(
            f.paths
                .data_root
                .join("runtime/blobs")
                .join(hash.strip_prefix("blake3:").unwrap()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(diff.scope_violations, vec!["tests/.gitignore"]);
}
/// Regression for the Issue #3 re-verification finding. `.git/info/exclude`
/// lives outside the worktree, so capture never walks it, yet it moves the
/// ignore boundary. The executor appends a rule there that makes a new directory
/// one Git never enters, and hides a file in it outside its write scope. Neither
/// the rule nor the hidden file is worktree content, and Git (including
/// `git status`) no longer reports either. The snapshot records a hash of the
/// exclude rules Git resolves, so the captured diff fails closed as
/// `SOURCE_DRIFT` instead of accepting the in-scope edit alone.
#[test]
fn executor_hiding_content_by_editing_info_exclude_fails_closed() {
    let f = Fixture::new();
    let p = f.plan();
    let message = f
        .run(&p, Mode::HideWithInfoExclude, seen(), false)
        .unwrap_err()
        .to_string();
    assert!(message.contains("SOURCE_DRIFT"), "{message}");
    assert!(
        git_ignored(&f.root, "hidden/conftest.py"),
        "Git itself must no longer report the hidden file"
    );
    let status = git_output(&f.root, &["status", "--porcelain", "--untracked-files=all"]);
    assert!(!String::from_utf8_lossy(&status.stdout).contains("hidden/"));
    let run = f
        .store()
        .runtime_status(&f.root, &p.packet.plan_id)
        .unwrap()
        .unwrap();
    assert_eq!(run.state, RunState::Blocked);
    assert!(run.reason.unwrap().contains("SOURCE_DRIFT"));
}
#[test]
fn runtime_owned_plan_rejects_legacy_api_verifier_and_completion_spoofs() {
    let f = Fixture::new();
    let p = f.plan();
    assert!(f.run(&p, Mode::Reject, seen(), false).is_err());
    let info = RepositoryInfo::discover(&f.root).unwrap();
    let mut s = f.store();
    let job = AgentJob {
        version: ProtocolVersion::V1,
        job_id: JobId::new("spoof:job").unwrap(),
        agent_id: AgentId::new("spoof:agent").unwrap(),
        role: AgentRole::Verifier,
        plan_id: p.packet.plan_id.clone(),
        task_id: Some(p.packet.tasks[0].task_id.clone()),
        state: JobState::Queued,
        provider: None,
        created_at_ms: local::now_ms().unwrap(),
        started_at_ms: None,
        finished_at_ms: None,
    };
    assert!(
        s.register_job_in_workspace(&info.repository_id, &info.workspace_id, &job)
            .is_err()
    );
    let c = common::sql(&f.paths.database);
    assert!(
        c.execute("UPDATE tasks SET state_json='\"VERIFIED\"'", [])
            .is_err()
    );
    assert!(c.execute("UPDATE execution_plans SET state='COMPLETE',integration_json='{}',final_source_json='{}'",[]).is_err());
}
#[test]
fn sibling_workspace_cannot_run_or_cancel_another_plan() {
    let f = Fixture::new();
    let p = f.plan();
    let linked = f.temp.0.join("linked");
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    f.store()
        .register_repository(RepositoryInfo::discover(&linked).unwrap())
        .unwrap();
    let mut s = f.store();
    let mut runtime =
        Runtime::new(&mut s, f.paths.clone(), f.config.clone(), BTreeMap::new()).unwrap();
    assert!(runtime.run(&linked, &p.packet.plan_id).is_err());
    assert!(
        f.store()
            .runtime_cancel(&linked, &p.packet.plan_id)
            .is_err()
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires macOS sandbox capability outside a nested host sandbox"]
fn native_sandboxed_checks_complete_the_fake_provider_diamond() {
    let f = Fixture::new();
    let mut policy = ProjectConfig::load(&f.root).unwrap();
    policy.commands.insert(
        "unit".into(),
        CommandSpec {
            program: "/usr/bin/grep".into(),
            args: vec![
                "-q".into(),
                "accepted fixture change".into(),
                "src/api.rs".into(),
            ],
            cwd: ".".into(),
        },
    );
    fs::write(
        f.root.join(".agentctl/project.toml"),
        toml::to_string(&policy).unwrap(),
    )
    .unwrap();
    git(&f.root, &["add", ".agentctl/project.toml"]);
    git(
        &f.root,
        &["commit", "--quiet", "-m", "real source assertion"],
    );
    f.store().index_repository(&f.root).unwrap();
    let p = f.plan();
    let mut s = f.store();
    let mut runtime = Runtime::new(
        &mut s,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Pass,
                seen: seen(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap();
    assert_eq!(
        runtime.run(&f.root, &p.packet.plan_id).unwrap().state,
        RunState::Complete
    );
}

struct CrashCheck {
    count: usize,
    at: usize,
}
impl CheckLauncher for CrashCheck {
    fn provenance(&self) -> &'static str {
        "CRASH_TEST_FIXTURE"
    }
    fn launch(&mut self, spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        self.count += 1;
        assert_ne!(self.count, self.at, "controller loss at durable checkpoint");
        Checks { fail: false }.launch(spec)
    }
}

#[test]
fn reopen_resumes_pending_verification_and_integration_without_reexecuting_verified_tasks() {
    for at in [1, 2, 5] {
        let f = Fixture::new();
        let p = f.plan();
        let inputs = seen();
        let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut s = f.store();
            Runtime::new(
                &mut s,
                f.paths.clone(),
                f.config.clone(),
                BTreeMap::from([(
                    "test".into(),
                    Box::new(Fake {
                        mode: Mode::Pass,
                        seen: inputs.clone(),
                    }) as Box<dyn ProviderAdapter>,
                )]),
            )
            .unwrap()
            .with_check_launcher(Box::new(CrashCheck { count: 0, at }))
            .run(&f.root, &p.packet.plan_id)
            .unwrap();
        }));
        assert!(crash.is_err());
        assert_eq!(
            f.store()
                .runtime_status(&f.root, &p.packet.plan_id)
                .unwrap()
                .unwrap()
                .state,
            RunState::Running
        );
        assert_eq!(
            f.run(&p, Mode::Pass, inputs.clone(), false).unwrap().state,
            RunState::Complete
        );
        let inputs = inputs.lock().unwrap();
        assert_eq!(inputs.len(), 9);
        for task in &p.packet.tasks {
            assert_eq!(
                inputs
                    .iter()
                    .filter(|i| i.role == AgentRole::Executor
                        && i.task_id.as_ref() == Some(&task.task_id))
                    .count(),
                1
            );
        }
    }
}

#[test]
fn interrupted_provider_is_persisted_and_never_relaunched_automatically() {
    let f = Fixture::new();
    let p = f.plan();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.run(
            &p,
            Mode::Panic,
            seen(),
            false
        )))
        .is_err()
    );
    let inputs = seen();
    assert!(f.run(&p, Mode::Pass, inputs.clone(), false).is_err());
    assert!(inputs.lock().unwrap().is_empty());
    let jobs = f
        .store()
        .runtime_jobs(&f.root, Some(&p.packet.plan_id))
        .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, RuntimeJobState::Interrupted);
    assert_eq!(
        f.store()
            .job(
                &RepositoryInfo::discover(&f.root).unwrap().repository_id,
                &jobs[0].job_id
            )
            .unwrap()
            .unwrap()
            .state,
        JobState::Failed
    );
}

#[test]
fn token_observations_preserve_exact_estimated_and_unknown_without_fabrication() {
    use local::store::JournalEntry;
    for provenance in [
        TokenUsageProvenance::Exact,
        TokenUsageProvenance::Estimated,
        TokenUsageProvenance::Unknown,
    ] {
        let f = Fixture::new();
        let p = f.plan();
        f.run(&p, Mode::Usage(provenance), seen(), false).unwrap();
        let events = f.store().events(None, None, None, 1000).unwrap();
        let usages: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.entry {
                JournalEntry::Agent { event } => match &event.event {
                    AgentEventKind::TokenUsageObserved { usage } => Some(usage),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(usages.len(), 9);
        for u in usages {
            assert_eq!(u.provenance, provenance);
            assert_eq!(
                u.input_tokens,
                (provenance != TokenUsageProvenance::Unknown).then_some(17)
            );
            assert!(u.total_tokens.is_none());
        }
    }
}

#[test]
fn captured_artifacts_and_command_metadata_are_hash_bound_and_tamper_evident() {
    let f = Fixture::new();
    let p = f.plan();
    let inputs = seen();
    let run = f.run(&p, Mode::Pass, inputs.clone(), false).unwrap();
    let artifacts = Artifacts::new(&f.paths.data_root.join("runtime/blobs")).unwrap();
    let before: SourceSnapshot = artifacts.decode(&run.baseline).unwrap();
    let after: SourceSnapshot = artifacts.decode(&run.expected).unwrap();
    assert_eq!(before.head, after.head);
    assert_ne!(before.files, after.files);
    for accepted in run.accepted.values() {
        let diff: CapturedDiff = artifacts.decode(&accepted.diff).unwrap();
        assert_eq!(diff.workspace_id, run.workspace_id);
        assert_eq!(diff.plan_id, p.packet.plan_id);
        assert_eq!(diff.executor_job_id.as_ref(), Some(&accepted.executor));
        assert_eq!(diff.changes.len(), 1);
        assert!(diff.scope_violations.is_empty());
    }
    let inputs = inputs.lock().unwrap();
    let evidence = &inputs[1].artifact["evidence_records"];
    assert_eq!(evidence[1]["exit_status"], 0);
    assert_eq!(evidence[1]["command"]["program"], "/usr/bin/true");
    assert!(
        evidence[1]["stdout_hash"]
            .as_str()
            .unwrap()
            .starts_with("blake3:")
    );
    let path = artifacts.path(&run.baseline).unwrap();
    fs::write(path, b"tampered").unwrap();
    assert!(artifacts.get(&run.baseline).is_err());
}

#[test]
fn v6_runtime_migration_is_additive_atomic_and_missing_guards_fail_closed() {
    let f = Fixture::new();
    let p = f.plan();
    let c = common::sql(&f.paths.database);
    let old: String = c
        .query_row("SELECT metadata_json FROM execution_plans", [], |r| {
            r.get(0)
        })
        .unwrap();
    common::strip_runtime(&c);
    c.pragma_update(None, "user_version", 6).unwrap();
    c.execute_batch("CREATE TABLE runtime_jobs(block_upgrade TEXT)")
        .unwrap();
    drop(c);
    assert!(Store::open(&f.paths.database, 5000).is_err());
    let c = common::sql(&f.paths.database);
    assert_eq!(
        c.query_row::<u32, _, _>("PRAGMA user_version", [], |r| r.get(0))
            .unwrap(),
        6
    );
    assert_eq!(
        c.query_row::<u32, _, _>(
            "SELECT count(*) FROM sqlite_master WHERE name='runtime_runs'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
    c.execute_batch("DROP TABLE runtime_jobs").unwrap();
    drop(c);
    assert_eq!(f.store().status().unwrap().schema_version, 12);
    assert_eq!(
        f.store()
            .execution_plan(&f.root, &p.packet.plan_id)
            .unwrap()
            .state,
        PlanState::Active
    );
    let c = common::sql(&f.paths.database);
    assert_eq!(
        c.query_row::<String, _, _>("SELECT metadata_json FROM execution_plans", [], |r| r
            .get(0))
            .unwrap(),
        old
    );
    c.execute_batch("DROP TRIGGER runtime_task_gate").unwrap();
    drop(c);
    assert!(Store::open(&f.paths.database, 5000).is_err());
}

fn cli(f: &Fixture, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .current_dir(&f.root)
        .args(args)
        .env("HOME", f.temp.0.join("home"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME")
        .output()
        .unwrap()
}
#[test]
fn cli_dry_run_and_provider_inspection_are_isolated_and_nonexecuting() {
    let f = Fixture::new();
    let p = f.plan();
    for args in [
        vec!["provider", "list", "--json"],
        vec!["provider", "doctor", "--json"],
        vec![
            "run",
            "plan",
            p.packet.plan_id.as_str(),
            "--dry-run",
            "--json",
        ],
    ] {
        let output = cli(&f, &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap();
    }
    assert!(f.store().runtime_jobs(&f.root, None).unwrap().is_empty());
    assert!(!f.paths.data_root.join("runtime").exists());
    assert!(!RepositoryInfo::discover(&f.root).unwrap().source.dirty);
}

struct Controlled {
    paths: MachinePaths,
    config: RuntimeConfig,
    cancel: bool,
    seen: Arc<Mutex<Vec<JobInput>>>,
}
impl ProviderAdapter for Controlled {
    fn capabilities(&self) -> Capabilities {
        Fake {
            mode: Mode::Pass,
            seen: self.seen.clone(),
        }
        .capabilities()
    }
    fn launch(
        &mut self,
        input: &JobInput,
        spec: ProcessSpec,
        role: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        let mut store = Store::open(&self.paths.database, 5000)?;
        let err = Runtime::new(
            &mut store,
            self.paths.clone(),
            self.config.clone(),
            BTreeMap::new(),
        )?
        .run(&spec.workspace, input.plan_id.as_ref().unwrap())
        .unwrap_err();
        assert!(err.to_string().contains("lease"), "{err}");
        if self.cancel {
            store.runtime_cancel(&spec.workspace, input.plan_id.as_ref().unwrap())?;
            self.seen.lock().unwrap().push(input.clone());
            return Ok(Box::new(Hang { cancelled: false }));
        }
        Fake {
            mode: Mode::Pass,
            seen: self.seen.clone(),
        }
        .launch(input, spec, role)
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}

#[test]
fn workspace_lease_prevents_concurrent_launch_and_cancellation_stops_dependents() {
    for cancel in [false, true] {
        let f = Fixture::new();
        let p = f.plan();
        let inputs = seen();
        let mut s = f.store();
        let result = Runtime::new(
            &mut s,
            f.paths.clone(),
            f.config.clone(),
            BTreeMap::from([(
                "test".into(),
                Box::new(Controlled {
                    paths: f.paths.clone(),
                    config: f.config.clone(),
                    cancel,
                    seen: inputs.clone(),
                }) as Box<dyn ProviderAdapter>,
            )]),
        )
        .unwrap()
        .with_check_launcher(Box::new(Checks { fail: false }))
        .run(&f.root, &p.packet.plan_id);
        assert_eq!(result.is_err(), cancel);
        assert_eq!(inputs.lock().unwrap().len(), if cancel { 1 } else { 9 });
        if cancel {
            assert_eq!(
                s.runtime_status(&f.root, &p.packet.plan_id)
                    .unwrap()
                    .unwrap()
                    .state,
                RunState::Cancelled
            );
            assert_eq!(
                s.runtime_jobs(&f.root, Some(&p.packet.plan_id)).unwrap()[0].state,
                RuntimeJobState::Cancelled
            );
        }
    }
}

#[test]
fn ignored_files_are_not_an_escape_from_actual_diff_scope_checks() {
    let f = Fixture::new();
    fs::write(f.root.join(".gitignore"), "outside.txt\n").unwrap();
    git(&f.root, &["add", ".gitignore"]);
    git(&f.root, &["commit", "--quiet", "-m", "ignore fixture"]);
    f.store().index_repository(&f.root).unwrap();
    let p = f.plan();
    assert!(f.run(&p, Mode::Scope, seen(), false).is_err());
    assert!(!RepositoryInfo::discover(&f.root).unwrap().source.dirty);
    let events = f.store().events(None, None, None, 1000).unwrap();
    let hash = events
        .iter()
        .find_map(|e| match &e.entry {
            local::store::JournalEntry::Runtime { phase, detail, .. }
                if phase == "DIFF_CAPTURED" =>
            {
                Some(detail.clone())
            }
            _ => None,
        })
        .unwrap();
    let bytes = fs::read(
        f.paths
            .data_root
            .join("runtime/blobs")
            .join(hash.strip_prefix("blake3:").unwrap()),
    )
    .unwrap();
    let diff: CapturedDiff = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(diff.scope_violations, vec!["outside.txt"]);
    assert_eq!(diff.changes[0].path, "outside.txt");
    assert!(diff.changes[0].before.is_none());
}

#[test]
fn full_fake_planner_to_integration_flow_and_hash_helper_use_canonical_contracts() {
    use std::io::Write;
    let f = Fixture::new();
    let prepared = f.prepare();
    let p = artifact(&prepared);
    let inputs = seen();
    let mut s = f.store();
    let result = Runtime::new(
        &mut s,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p.clone())),
                seen: inputs.clone(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap();
    assert_eq!(result.state, PlanState::Validated);
    s.activate_execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    assert_eq!(
        f.run(&p, Mode::Pass, inputs.clone(), false).unwrap().state,
        RunState::Complete
    );
    assert_eq!(inputs.lock().unwrap().len(), 10);
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .args(["run", "packet-hashes"])
        .env("HOME", f.temp.0.join("nonexistent-home"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec_pretty(&p.packet).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let hashes: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(hashes["plan_packet_hash"], hash(&p.packet).unwrap());
    assert!(!f.temp.0.join("nonexistent-home").exists());
}

#[test]
fn correction_requires_explicit_replacement_and_stops_at_configured_bound() {
    let mut f = Fixture::new();
    f.config.max_correction_rounds = 1;
    let p = f.plan();
    assert!(f.run(&p, Mode::Reject, seen(), false).is_err());
    // A human chooses to preserve the rejected edit as the next committed
    // baseline. agentctl neither resets nor commits the user's checkout.
    git(&f.root, &["add", "src/api.rs"]);
    git(
        &f.root,
        &["commit", "--quiet", "-m", "human replan baseline"],
    );
    f.store().index_repository(&f.root).unwrap();
    let mut replacement = artifact(&f.prepare());
    replacement.packet.plan_id = PlanId::new("plan:correction").unwrap();
    for task in &mut replacement.packet.tasks {
        task.task_id = TaskId::new(format!("{}:r", task.task_id.as_str())).unwrap();
        for dep in &mut task.dependencies {
            *dep = TaskId::new(format!("{}:r", dep.as_str())).unwrap();
        }
    }
    for (contract, task) in replacement
        .metadata
        .contracts
        .iter_mut()
        .zip(&replacement.packet.tasks)
    {
        contract.task_id = task.task_id.clone();
        contract.task_packet_hash = hash(task).unwrap();
    }
    replacement.metadata.integration.plan_id = replacement.packet.plan_id.clone();
    replacement.metadata.integration.plan_packet_hash = hash(&replacement.packet).unwrap();
    replacement.metadata.replan = Some(ReplanReference {
        previous_plan_id: p.packet.plan_id.clone(),
        reason: "explicit corrective decomposition".into(),
        previously_verified_tasks: vec![],
        replaced_tasks: p.packet.tasks.iter().map(|t| t.task_id.clone()).collect(),
    });
    let mut s = f.store();
    s.import_execution_plan(&f.root, &replacement).unwrap();
    assert!(
        s.supersede_execution_plan(&f.root, &p.packet.plan_id, &replacement.packet.plan_id)
            .is_err()
    );
    Runtime::new(&mut s, f.paths.clone(), f.config.clone(), BTreeMap::new())
        .unwrap()
        .replace(&f.root, &p.packet.plan_id, &replacement.packet.plan_id)
        .unwrap();
    s.activate_execution_plan(&f.root, &replacement.packet.plan_id)
        .unwrap();
    assert!(f.run(&replacement, Mode::Reject, seen(), false).is_err());
    assert_eq!(
        s.runtime_status(&f.root, &replacement.packet.plan_id)
            .unwrap()
            .unwrap()
            .correction_round,
        1
    );
    let error = Runtime::new(&mut s, f.paths.clone(), f.config.clone(), BTreeMap::new())
        .unwrap()
        .replace(
            &f.root,
            &replacement.packet.plan_id,
            &PlanId::new("plan:another").unwrap(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("correction limit"));
}

#[test]
fn shared_artifact_publication_is_atomic_across_concurrent_workspace_controllers() {
    let temp = common::TempDir::new();
    let root = temp.0.join("blobs");
    let artifacts = Arc::new(Artifacts::new(&root).unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(12));
    let threads: Vec<_> = (0..12)
        .map(|_| {
            let a = artifacts.clone();
            let b = barrier.clone();
            std::thread::spawn(move || {
                b.wait();
                let bytes = vec![42; 1024 * 1024];
                let r = a.put(&bytes).unwrap();
                assert_eq!(a.get(&r).unwrap(), bytes);
                r
            })
        })
        .collect();
    let refs: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(refs.iter().all(|r| r == &refs[0]));
    assert_eq!(fs::read_dir(root).unwrap().count(), 1);
}

#[test]
fn planner_default_topology_is_session_native_with_fresh_instances_and_parentage() {
    let f = Fixture::new();
    let prepared = f.prepare();
    let p = artifact(&prepared);
    let inputs = seen();
    let mut s = f.store();
    Runtime::new(
        &mut s,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p.clone())),
                seen: inputs.clone(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap();
    s.activate_execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    let run = f.run(&p, Mode::Pass, inputs.clone(), false).unwrap();
    let session = run.engineering_session.unwrap();
    let inputs = inputs.lock().unwrap();
    assert_eq!(inputs.len(), 10);
    let planner = &inputs[0];
    let instances: std::collections::BTreeSet<_> = inputs
        .iter()
        .map(|i| &i.ownership.agent_instance_id)
        .collect();
    assert_eq!(instances.len(), 10);
    for input in inputs.iter() {
        assert_eq!(input.ownership.engineering_session_id, session.id);
        assert_eq!(input.ownership.lifetime, AgentLifetime::SessionNative);
    }
    for pair in inputs[1..9].as_chunks::<2>().0 {
        assert_eq!(pair[0].role, AgentRole::Executor);
        assert_eq!(pair[1].role, AgentRole::Verifier);
        assert_eq!(
            pair[0].ownership.parent_agent_instance_id.as_ref(),
            Some(&planner.ownership.agent_instance_id)
        );
        assert_eq!(
            pair[1].ownership.parent_agent_instance_id.as_ref(),
            Some(&pair[0].ownership.agent_instance_id)
        );
        assert_ne!(pair[0].session_id, pair[1].session_id);
        assert_ne!(pair[0].job_id, pair[1].job_id);
    }
    let integration = &inputs[9];
    assert_eq!(integration.role, AgentRole::Verifier);
    assert!(integration.task_id.is_none());
    assert_eq!(
        integration.ownership.parent_agent_instance_id.as_ref(),
        Some(&planner.ownership.agent_instance_id)
    );
    let helper = inputs[1]
        .ownership
        .child(AgentId::new("agent:reserved-helper").unwrap());
    let grandchild = helper.child(AgentId::new("agent:reserved-grandchild").unwrap());
    assert_eq!(grandchild.engineering_session_id, session.id);
    assert_eq!(grandchild.lifetime, AgentLifetime::SessionNative);
    assert_eq!(s.runtime_jobs(&f.root, None).unwrap().len(), 10); // metadata construction never launches a helper
    assert_eq!(
        fs::read_dir(f.paths.data_root.join("runtime/scratch"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn same_provider_policy_never_reuses_another_engineering_sessions_workers() {
    let a = Fixture::new();
    let b = Fixture::new();
    assert_eq!(a.config, b.config);
    let pa = a.plan();
    let pb = b.plan();
    let ia = seen();
    let ib = seen();
    let ra = a.run(&pa, Mode::Pass, ia.clone(), false).unwrap();
    let rb = b.run(&pb, Mode::Pass, ib.clone(), false).unwrap();
    assert_ne!(ra.engineering_session, rb.engineering_session);
    let ia = ia.lock().unwrap();
    let ib = ib.lock().unwrap();
    for x in ia.iter() {
        for y in ib.iter() {
            assert_ne!(x.session_id, y.session_id);
            assert_ne!(x.ownership.agent_instance_id, y.ownership.agent_instance_id);
        }
    }
    // A packet produced in B cannot be applied to A through normal interfaces.
    let b_job = b
        .store()
        .runtime_jobs(&b.root, Some(&pb.packet.plan_id))
        .unwrap()
        .into_iter()
        .find(|j| j.role == AgentRole::Verifier)
        .unwrap();
    let proof: VerificationPacket = Artifacts::new(&b.paths.data_root.join("runtime/blobs"))
        .unwrap()
        .decode(&b_job.output.unwrap())
        .unwrap();
    assert!(
        a.store()
            .complete_execution_plan(&a.root, &pa.packet.plan_id, &proof, &ia[0].source)
            .is_err()
    );
}

#[test]
fn old_v7_runtime_metadata_remains_inspectable_without_silent_ownership_backfill() {
    let f = Fixture::new();
    let p = f.plan();
    assert!(f.run(&p, Mode::Reject, seen(), false).is_err());
    let c = common::sql(&f.paths.database);
    // Simulate an already-applied unaccepted v7 database, without changing its
    // schema or inventing historical ownership. This is privileged fixture setup.
    c.create_scalar_function(
        "agentctl_runtime_authorized",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS,
        |_| Ok(true),
    )
    .unwrap();
    c.execute(
        "UPDATE runtime_runs SET record_json=json_remove(record_json,'$.engineering_session')",
        [],
    )
    .unwrap();
    c.execute(
        "UPDATE runtime_jobs SET record_json=json_remove(record_json,'$.ownership','$.task_id','$.route','$.prompt')",
        [],
    )
    .unwrap();
    let before: String = c
        .query_row("SELECT record_json FROM runtime_runs", [], |r| r.get(0))
        .unwrap();
    drop(c);
    assert!(
        f.store()
            .runtime_status(&f.root, &p.packet.plan_id)
            .unwrap()
            .unwrap()
            .engineering_session
            .is_none()
    );
    let error = f.run(&p, Mode::Pass, seen(), false).unwrap_err();
    assert!(error.to_string().contains("ownership"));
    let c = common::sql(&f.paths.database);
    assert_eq!(
        before,
        c.query_row::<String, _, _>("SELECT record_json FROM runtime_runs", [], |r| r.get(0))
            .unwrap()
    );
    assert_eq!(f.store().status().unwrap().schema_version, 12);
    assert!(
        f.store()
            .runtime_jobs(&f.root, None)
            .unwrap()
            .iter()
            .all(|j| j.route.is_none() && j.prompt.is_none())
    );
}

struct ObservedProcess {
    inner: Box<dyn RunningProcess>,
    database: PathBuf,
    snapshots: Arc<Mutex<Vec<local::observe::Snapshot>>>,
}

// A real owned child waits on an open stdin pipe; no provider/model is invoked.
struct LiveFixtureProcess {
    child: std::process::Child,
    gate: Arc<std::sync::atomic::AtomicU8>,
    inner: Box<dyn RunningProcess>,
    confirmed: bool,
}
impl RunningProcess for LiveFixtureProcess {
    fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }
    fn liveness_confirmed(&self) -> bool {
        self.confirmed
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        use std::sync::atomic::Ordering;
        self.confirmed = false;
        match self.gate.load(Ordering::Acquire) {
            2 => panic!("fixture controller crash while provider child is attached"),
            1 => {
                let _ = self.child.kill();
                self.child.wait()?;
                self.inner.poll()
            }
            _ => {
                assert!(self.child.try_wait()?.is_none());
                self.confirmed = true;
                Ok(None)
            }
        }
    }
    fn cancel(&mut self) -> local::Result<CancellationOutcome> {
        self.confirmed = false;
        self.gate.store(1, std::sync::atomic::Ordering::Release);
        self.child.kill()?;
        Ok(CancellationOutcome::Applied)
    }
}
impl Drop for LiveFixtureProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct LiveFixtureProvider {
    fake: Fake,
    gate: Arc<std::sync::atomic::AtomicU8>,
    launched: bool,
}
impl ProviderAdapter for LiveFixtureProvider {
    fn capabilities(&self) -> Capabilities {
        self.fake.capabilities()
    }
    fn launch(
        &mut self,
        input: &JobInput,
        spec: ProcessSpec,
        config: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        let inner = self.fake.launch(input, spec, config)?;
        if self.launched {
            return Ok(inner);
        }
        self.launched = true;
        let child = Command::new("/bin/cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(Box::new(LiveFixtureProcess {
            child,
            inner,
            gate: self.gate.clone(),
            confirmed: false,
        }))
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        self.fake.collect(output)
    }
}
struct LiveRun {
    gate: Arc<std::sync::atomic::AtomicU8>,
    thread: Option<std::thread::JoinHandle<local::Result<RunRecord>>>,
}
impl LiveRun {
    fn start(f: &Fixture, p: &ExecutionPlan) -> Self {
        let gate = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let child_gate = gate.clone();
        let paths = f.paths.clone();
        let root = f.root.clone();
        let config = f.config.clone();
        let plan = p.packet.plan_id.clone();
        let thread = std::thread::spawn(move || {
            let mut store = Store::open(&paths.database, 100).unwrap();
            Runtime::new(
                &mut store,
                paths,
                config,
                BTreeMap::from([(
                    "test".into(),
                    Box::new(LiveFixtureProvider {
                        fake: Fake {
                            mode: Mode::Pass,
                            seen: seen(),
                        },
                        gate: child_gate,
                        launched: false,
                    }) as Box<dyn ProviderAdapter>,
                )]),
            )
            .unwrap()
            .with_check_launcher(Box::new(Checks { fail: false }))
            .run(&root, &plan)
        });
        Self {
            gate,
            thread: Some(thread),
        }
    }
    fn join(mut self, outcome: u8) -> std::thread::Result<local::Result<RunRecord>> {
        self.gate
            .store(outcome, std::sync::atomic::Ordering::Release);
        self.thread.take().unwrap().join()
    }
}
impl Drop for LiveRun {
    fn drop(&mut self) {
        self.gate.store(1, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn wait_for_live(f: &Fixture) -> local::observe::Snapshot {
    let store = Store::read_only(&f.paths.database, 100).unwrap();
    let start = std::time::Instant::now();
    loop {
        let snapshot = store.observe(local::now_ms().unwrap()).unwrap();
        if snapshot
            .agents
            .iter()
            .any(|a| a.liveness == local::observe::Liveness::Live)
        {
            return snapshot;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "current child did not become observable"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn observe_current_owned_child_is_live_but_separate_process_and_terminal_jobs_are_unknown() {
    use local::observe::Liveness;
    let f = Fixture::new();
    let p = f.plan();
    let run = LiveRun::start(&f, &p);
    let snapshot = wait_for_live(&f);
    let agent = snapshot
        .agents
        .iter()
        .find(|a| a.liveness == Liveness::Live)
        .unwrap();
    assert_eq!(agent.state, "RUNNING");
    assert_eq!(agent.activity, "PROVIDER_EXECUTION");
    let cli_result = cli(&f, &["observe", "agent", &agent.id, "--json"]);
    assert!(cli_result.status.success());
    let value: Value = serde_json::from_slice(&cli_result.stdout).unwrap();
    assert_eq!(value["state"], "RUNNING");
    assert_eq!(value["liveness"], "UNKNOWN");
    assert_eq!(value["activity"], "PROVIDER_EXECUTION");
    let mut app = local::agenttop::App::new(snapshot.clone());
    assert!(
        local::agenttop::render_text(&app, 120, 40)
            .unwrap()
            .contains("RUNNING/LIVE")
    );
    app.inspect = true;
    assert!(
        local::agenttop::render_text(&app, 120, 40)
            .unwrap()
            .contains("Liveness LIVE")
    );
    run.join(1).unwrap().unwrap();
    let completed = f.store().observe(local::now_ms().unwrap()).unwrap();
    assert!(
        completed
            .agents
            .iter()
            .all(|a| a.state == "SUCCEEDED" && a.liveness == Liveness::Unknown)
    );
    let c = common::sql(&f.paths.database);
    assert_eq!(
        c.query_row::<i64, _, _>(
            "SELECT count(*) FROM runtime_jobs WHERE record_json LIKE '%liveness%'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn observe_crash_reopen_ignores_pid_and_recent_events_alongside_another_live_session() {
    use local::observe::Liveness;
    let old = Fixture::new();
    let plan = old.plan();
    let crashed = LiveRun::start(&old, &plan);
    let snapshot = wait_for_live(&old);
    let stale = snapshot
        .agents
        .iter()
        .find(|a| a.liveness == Liveness::Live)
        .unwrap()
        .clone();
    assert!(crashed.join(2).is_err());
    // Privileged fixture: even a PID known to be alive cannot establish identity.
    let c = common::sql(&old.paths.database);
    c.create_scalar_function(
        "agentctl_runtime_authorized",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS,
        |_| Ok(true),
    )
    .unwrap();
    c.execute(
        "UPDATE runtime_jobs SET record_json=json_set(record_json,'$.pid',?1) WHERE job_id=?2",
        rusqlite::params![std::process::id(), &stale.job_id],
    )
    .unwrap();
    drop(c);
    let store = Store::read_only(&old.paths.database, 100).unwrap();
    let at = local::now_ms().unwrap();
    let reopened = store.observe(at).unwrap();
    let agent = reopened.agents.iter().find(|a| a.id == stale.id).unwrap();
    assert_eq!(agent.state, "RUNNING");
    assert_eq!(agent.liveness, Liveness::Unknown);
    assert_eq!(agent.activity, "PROVIDER_EXECUTION");
    assert!(at.saturating_sub(agent.last_event.as_ref().unwrap().at_ms) < 10_000);
    let before = fs::read(&old.paths.database).unwrap();
    let result = cli(&old, &["observe", "agent", &stale.id, "--json"]);
    assert!(result.status.success());
    let value: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(value["liveness"], "UNKNOWN");
    assert_eq!(value["state"], "RUNNING");
    let mut app = local::agenttop::App::new(reopened);
    assert!(
        local::agenttop::render_text(&app, 120, 40)
            .unwrap()
            .contains("RUNNING/UNKNOWN")
    );
    app.inspect = true;
    let text = local::agenttop::render_text(&app, 120, 40).unwrap();
    assert!(text.contains("Liveness UNKNOWN"));
    assert!(!text.contains("PROVIDER_RUNNING"));
    assert_eq!(before, fs::read(&old.paths.database).unwrap());
    let mut current = Fixture::new();
    current.paths = old.paths.clone();
    current
        .store()
        .register_repository(RepositoryInfo::discover(&current.root).unwrap())
        .unwrap();
    current.store().index_repository(&current.root).unwrap();
    let plan = current.plan();
    let active = LiveRun::start(&current, &plan);
    let mixed = wait_for_live(&current);
    assert_eq!(mixed.sessions.len(), 2);
    assert_eq!(
        mixed
            .agents
            .iter()
            .find(|a| a.id == stale.id)
            .unwrap()
            .liveness,
        Liveness::Unknown
    );
    let live = mixed
        .agents
        .iter()
        .find(|a| a.liveness == Liveness::Live)
        .unwrap();
    assert_ne!(live.session_id, stale.session_id);
    active.join(1).unwrap().unwrap();
}
impl RunningProcess for ObservedProcess {
    fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }
    fn cancel(&mut self) -> local::Result<CancellationOutcome> {
        self.inner.cancel()
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        // A second, read-only connection sees the legitimate live transition.
        let snapshot = Store::read_only(&self.database, 100)?.observe(local::now_ms()?)?;
        self.snapshots.lock().unwrap().push(snapshot);
        self.inner.poll()
    }
}
struct ObservedProvider {
    fake: Fake,
    database: PathBuf,
    snapshots: Arc<Mutex<Vec<local::observe::Snapshot>>>,
}
impl ProviderAdapter for ObservedProvider {
    fn capabilities(&self) -> Capabilities {
        self.fake.capabilities()
    }
    fn launch(
        &mut self,
        input: &JobInput,
        spec: ProcessSpec,
        config: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        Ok(Box::new(ObservedProcess {
            inner: self.fake.launch(input, spec, config)?,
            database: self.database.clone(),
            snapshots: self.snapshots.clone(),
        }))
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        self.fake.collect(output)
    }
    fn usage(&self, _: &ProcessOutput) -> local::Result<Usage> {
        Ok(Usage {
            provenance: TokenUsageProvenance::Exact,
            input: Some(17),
            output: Some(5),
            cached: None,
        })
    }
}
struct ObservedChecks {
    database: PathBuf,
    snapshots: Arc<Mutex<Vec<local::observe::Snapshot>>>,
}
impl CheckLauncher for ObservedChecks {
    fn provenance(&self) -> &'static str {
        "DETERMINISTIC_TEST_FIXTURE"
    }
    fn launch(&mut self, spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        Ok(Box::new(ObservedProcess {
            inner: Checks { fail: false }.launch(spec)?,
            database: self.database.clone(),
            snapshots: self.snapshots.clone(),
        }))
    }
}

#[test]
fn observe_live_diamond_planner_tree_checks_usage_and_guarded_completion() {
    use local::observe::usage::{Scope, series};
    let f = Fixture::new();
    let prepared = f.prepare();
    let mut p = artifact(&prepared);
    p.packet.tasks[2].dependencies = vec![p.packet.tasks[0].task_id.clone()];
    p.packet.tasks[3].dependencies = vec![
        p.packet.tasks[1].task_id.clone(),
        p.packet.tasks[2].task_id.clone(),
    ];
    for (contract, task) in p.metadata.contracts.iter_mut().zip(&p.packet.tasks) {
        contract.task_packet_hash = hash(task).unwrap();
    }
    p.metadata.integration.plan_packet_hash = hash(&p.packet).unwrap();
    let snapshots = Arc::new(Mutex::new(vec![]));
    let inputs = seen();
    let mut store = f.store();
    Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(ObservedProvider {
                fake: Fake {
                    mode: Mode::Planner(Box::new(p.clone())),
                    seen: inputs.clone(),
                },
                database: f.paths.database.clone(),
                snapshots: snapshots.clone(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap();
    store
        .activate_execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    let run = Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(ObservedProvider {
                fake: Fake {
                    mode: Mode::Pass,
                    seen: inputs.clone(),
                },
                database: f.paths.database.clone(),
                snapshots: snapshots.clone(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .with_check_launcher(Box::new(ObservedChecks {
        database: f.paths.database.clone(),
        snapshots: snapshots.clone(),
    }))
    .run(&f.root, &p.packet.plan_id)
    .unwrap();
    assert_eq!(run.state, RunState::Complete);
    let snapshots = snapshots.lock().unwrap();
    let live = snapshots
        .iter()
        .find(|s| {
            s.agents.iter().any(|a| {
                a.task_id.as_deref() == Some("task:1")
                    && a.role == "EXECUTOR"
                    && a.state == "RUNNING"
            })
        })
        .unwrap();
    assert_eq!(live.sessions.len(), 1);
    let session = &live.sessions[0];
    assert_eq!(session.verified, 1);
    assert_eq!(session.task_count, 4);
    assert!(session.progress_complete);
    let states: Vec<_> = live.tasks.iter().map(|t| t.presentation.as_str()).collect();
    assert_eq!(states, vec!["VERIFIED", "EXECUTING", "READY", "BLOCKED"]);
    assert_eq!(
        live.tasks[3].blocker.as_ref().unwrap().dependencies,
        vec!["task:1", "task:2"]
    );
    assert!(live.tasks[1].verifier_job.is_none());
    let planner = live.agents.iter().find(|a| a.role == "PLANNER").unwrap();
    let executor = live
        .agents
        .iter()
        .find(|a| a.task_id.as_deref() == Some("task:1"))
        .unwrap();
    assert_eq!(executor.parent_id.as_ref(), Some(&planner.id));
    assert_eq!(executor.activity, "PROVIDER_EXECUTION");
    let va = live.agents.iter().find(|a| a.role == "VERIFIER").unwrap();
    assert_eq!(va.verification.as_deref(), Some("PASS"));
    let ea = live
        .agents
        .iter()
        .find(|a| a.role == "EXECUTOR" && a.task_id.as_deref() == Some("task:0"))
        .unwrap();
    assert_eq!(va.parent_id.as_ref(), Some(&ea.id));
    assert_ne!(va.id, ea.id);
    assert!(live.agents.iter().all(|a| !a.ownership_uncertain));
    assert_eq!(
        series(live, Scope::Aggregate, None).total_observed,
        Some(44)
    );
    assert!(
        live.usage
            .iter()
            .all(|u| u.job_id.as_deref() != Some(&executor.job_id))
    ); // running usage stays unknown
    for scope in [
        Scope::Provider("test".into()),
        Scope::Role("VERIFIER".into()),
        Scope::Task("task:0".into()),
    ] {
        assert!(series(live, scope, None).total_observed.is_some());
    }
    assert!(
        live.events
            .windows(2)
            .all(|w| w[0].sequence < w[1].sequence)
    );
    assert!(snapshots.iter().any(|s| {
        s.sessions.iter().any(|s| {
            s.activity == "VERIFICATION_CHECK_STARTED" && s.check.as_deref() == Some("unit")
        })
    }));
    let final_snapshot = store.observe(local::now_ms().unwrap()).unwrap();
    assert_eq!(final_snapshot.sessions[0].state, "COMPLETE");
    assert_eq!(final_snapshot.sessions[0].verified, 4);
    let integration = final_snapshot
        .agents
        .iter()
        .find(|a| a.role == "INTEGRATION_VERIFIER")
        .unwrap();
    assert_eq!(integration.verification.as_deref(), Some("PASS"));
    let text =
        local::agenttop::render_text(&local::agenttop::App::new(live.clone()), 120, 40).unwrap();
    assert!(text.contains("1/4 VERIFIED"));
    assert!(text.contains("BLOCKED"));
    assert!(!text.contains('%'));
}

#[test]
fn observe_cli_and_agenttop_are_read_only_and_never_invoke_provider() {
    let f = Fixture::new();
    let p = f.plan();
    f.run(&p, Mode::Usage(TokenUsageProvenance::Exact), seen(), false)
        .unwrap();
    let store = Store::read_only(&f.paths.database, 100).unwrap();
    let at = local::now_ms().unwrap();
    let before = store.observe(at).unwrap();
    fn fingerprint(root: &Path) -> BTreeMap<PathBuf, String> {
        fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, String>) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(root, &path, files);
                } else {
                    files.insert(
                        path.strip_prefix(root).unwrap().to_owned(),
                        blake3::hash(&fs::read(&path).unwrap()).to_string(),
                    );
                }
            }
        }
        let mut files = BTreeMap::new();
        visit(root, root, &mut files);
        files
    }
    let all_files = fingerprint(&f.root);
    let bytes = fs::read(&f.paths.database).unwrap();
    let wal_path = f.paths.database.with_extension("sqlite3-wal");
    let wal = fs::read(&wal_path).ok();
    let repo = Command::new("git")
        .current_dir(&f.root)
        .args(["diff", "--binary"])
        .output()
        .unwrap()
        .stdout;
    for args in [
        vec!["observe", "snapshot", "--json"],
        vec!["observe", "sessions", "--json"],
        vec!["observe", "agents", "--json"],
        vec!["observe", "tasks", "--json"],
        vec!["observe", "usage", "--json"],
        vec!["observe", "events", "--json"],
        vec!["observe", "usage", "provider", "test", "--json"],
        vec!["observe", "task", "task:0", "--json"],
        vec![
            "observe",
            "session",
            before.sessions[0].id.as_str(),
            "--json",
        ],
        vec!["observe", "agent", before.agents[0].id.as_str(), "--json"],
    ] {
        let result = cli(&f, &args);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        serde_json::from_slice::<Value>(&result.stdout).unwrap();
    }
    assert!(
        !cli(&f, &["observe", "task", "missing", "--json"])
            .status
            .success()
    );
    let result = Command::new(env!("CARGO_BIN_EXE_agenttop"))
        .args(["--once", "--width", "80", "--height", "24"])
        .current_dir(&f.root)
        .env("HOME", &f.temp.0)
        .env("XDG_CONFIG_HOME", f.paths.config_root.parent().unwrap())
        .env("XDG_DATA_HOME", f.paths.data_root.parent().unwrap())
        .env("XDG_CACHE_HOME", f.paths.cache_root.parent().unwrap())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("TOKENS/min"));
    assert_eq!(bytes, fs::read(&f.paths.database).unwrap());
    assert_eq!(wal, fs::read(&wal_path).ok());
    assert_eq!(all_files, fingerprint(&f.root));
    assert_eq!(
        repo,
        Command::new("git")
            .current_dir(&f.root)
            .args(["diff", "--binary"])
            .output()
            .unwrap()
            .stdout
    );
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(store.observe(at).unwrap()).unwrap()
    );
}

#[test]
fn observe_recent_query_is_bounded_redacted_and_does_not_load_provider_logs() {
    let f = Fixture::new();
    let p = f.plan();
    f.run(&p, Mode::Pass, seen(), false).unwrap();
    let info = RepositoryInfo::discover(&f.root).unwrap();
    let mut c = common::sql(&f.paths.database);
    let tx = c.transaction().unwrap();
    let payload = serde_json::to_string(&local::store::JournalEntry::Runtime {
        job_id: None,
        phase: "TEST_OBSERVATION".into(),
        detail: "sk-credential-canary-never-show private provider transcript".into(),
    })
    .unwrap();
    for i in 0..2100 {
        tx.execute("INSERT INTO events(repo_id,workspace_id,plan_id,timestamp_ms,entry_json) VALUES (?1,?2,?3,?4,?5)",rusqlite::params![info.repository_id.as_str(),info.workspace_id.as_str(),p.packet.plan_id.as_str(),i,&payload]).unwrap();
    }
    tx.commit().unwrap();
    let store = Store::read_only(&f.paths.database, 100).unwrap();
    let start = std::time::Instant::now();
    for _ in 0..10 {
        let snapshot = store.observe(local::now_ms().unwrap()).unwrap();
        assert!(snapshot.truncated);
        assert_eq!(snapshot.events.len(), 2048);
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("credential-canary"));
        assert!(!json.contains("private provider transcript"));
    }
    eprintln!("10 capped observation queries: {:?}", start.elapsed());
}

#[test]
fn observe_multiple_sessions_same_provider_and_malformed_legacy_metadata() {
    let first = Fixture::new();
    let p = first.plan();
    first.run(&p, Mode::Pass, seen(), false).unwrap();
    let mut second = Fixture::new();
    second.paths = first.paths.clone();
    second
        .store()
        .register_repository(RepositoryInfo::discover(&second.root).unwrap())
        .unwrap();
    second.store().index_repository(&second.root).unwrap();
    let p2 = second.plan();
    assert!(second.run(&p2, Mode::Reject, seen(), false).is_err());
    let before = first.store().observe(local::now_ms().unwrap()).unwrap();
    assert_eq!(before.sessions.len(), 2);
    assert_ne!(before.sessions[0].id, before.sessions[1].id);
    assert!(
        before
            .agents
            .iter()
            .all(|a| a.provider.as_deref() == Some("test"))
    );
    assert!(before.sessions.iter().any(|s| s.state == "BLOCKED"));
    assert!(before.sessions.iter().any(|s| s.state == "COMPLETE"));
    for session in &before.sessions {
        let tree = local::observe::tree(&before, Some(&session.id));
        assert!(!tree.is_empty());
        assert!(
            tree.iter()
                .all(|(i, _)| before.agents[*i].workspace_id == session.workspace_id)
        );
    }
    let c = common::sql(&first.paths.database);
    c.create_scalar_function(
        "agentctl_runtime_authorized",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS,
        |_| Ok(true),
    )
    .unwrap();
    c.execute(
        "UPDATE runtime_jobs SET record_json='not-json' WHERE job_id=?1",
        [&before.agents[0].job_id],
    )
    .unwrap();
    c.execute(
        "UPDATE runtime_runs SET record_json='{}' WHERE repo_id=?1",
        [&before.sessions[0].repository_id],
    )
    .unwrap();
    let snapshot = first.store().observe(local::now_ms().unwrap()).unwrap();
    assert!(snapshot.agents.iter().any(|a| a.ownership_uncertain
        && a.state == "UNKNOWN"
        && a.liveness == local::observe::Liveness::Unknown));
    assert!(snapshot.sessions.iter().any(|s| {
        s.blocker
            .as_ref()
            .is_some_and(|b| b.kind == "MALFORMED_HISTORY")
    }));
    assert!(local::agenttop::render_text(&local::agenttop::App::new(snapshot), 35, 12).is_ok());
}

// Executes a `#!/bin/sh` provider fixture with Unix permission bits.
#[cfg(unix)]
#[test]
fn native_auth_preflight_prefers_provider_login_without_importing_api_environment() {
    use local::runtime::credentials::*;
    use std::os::unix::fs::PermissionsExt;
    let temp = common::TempDir::new();
    let executable = temp.0.join("provider");
    fs::write(&executable,"#!/bin/sh\n[ -z \"${ANTHROPIC_API_KEY-}\" ] || exit 8\n[ -z \"${CODEX_API_KEY-}\" ] || exit 9\nprintf '%s' '{\"loggedIn\":true}'\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    for provider in ["codex", "claude"] {
        let native = NativeAuth {
            provider: provider.into(),
            home: temp.0.clone(),
            provider_home: temp.0.join("auth"),
            config_override: true,
        };
        for mode in [AuthMode::Auto, AuthMode::Native] {
            let auth = Authentication {
                mode,
                api_key_env: Some("PATH".into()),
            };
            let status = auth.preflight(&executable, &native).unwrap();
            assert!(status.authenticated);
            assert_eq!(status.method, "NATIVE");
            assert!(!status.api_key_required);
        }
    }
    assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 1); // no auth files copied/created
}

#[test]
fn optional_api_key_auth_is_explicit_and_missing_auth_is_actionable() {
    use local::runtime::credentials::*;
    let temp = common::TempDir::new();
    for provider in ["codex", "claude"] {
        let native = NativeAuth {
            provider: provider.into(),
            home: temp.0.clone(),
            provider_home: temp.0.join("auth"),
            config_override: false,
        };
        let unavailable = Authentication::default()
            .preflight(Path::new("/usr/bin/false"), &native)
            .unwrap();
        assert!(!unavailable.authenticated);
        assert!(unavailable.guidance.contains("login"));
        for mode in [AuthMode::Auto, AuthMode::ApiKey] {
            // PATH is a harmless fixture value, not a live provider key. No model is called.
            let auth = Authentication {
                mode,
                api_key_env: Some("PATH".into()),
            };
            let status = auth
                .preflight(Path::new("/usr/bin/false"), &native)
                .unwrap();
            assert_eq!(status.method, "API_KEY");
            assert!(status.authenticated);
        }
        let forced = Authentication {
            mode: AuthMode::Native,
            api_key_env: Some("PATH".into()),
        };
        assert!(
            !forced
                .preflight(Path::new("/usr/bin/false"), &native)
                .unwrap()
                .authenticated
        );
        assert!(
            Authentication {
                mode: AuthMode::ApiKey,
                api_key_env: None
            }
            .validate()
            .is_err()
        );
        assert!(
            Authentication {
                mode: AuthMode::ApiKey,
                api_key_env: Some("sk-secret-value".into())
            }
            .validate()
            .is_err()
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires macOS sandbox capability outside a nested host sandbox"]
fn native_auth_same_login_fresh_sessions_and_optional_key_cli_dag_never_persist_credentials() {
    use local::runtime::credentials::*;
    use std::os::unix::fs::PermissionsExt;
    const CANARY: &str = "sk-ant-agentctl-fixture-secret-never-persist-123456789";
    for provider in ["codex", "claude"] {
        for key_mode in [false, true] {
            let mut f = Fixture::new();
            let home = f.temp.0.join("home");
            let native = home.join(if provider == "codex" {
                ".codex"
            } else {
                ".claude"
            });
            fs::create_dir_all(&native).unwrap();
            let credential = native.join(if provider == "codex" {
                "auth.json"
            } else {
                ".credentials.json"
            });
            fs::write(&credential, CANARY).unwrap();
            fs::write(
                native.join("history.jsonl"),
                "UNRELATED_CONVERSATION_CANARY",
            )
            .unwrap();
            let executable = f.temp.0.join("mock-provider");
            let script = r#"#!/usr/bin/python3
import sys,os,json,pathlib
if '--version' in sys.argv: print('fixture provider');sys.exit(0)
if 'status' in sys.argv:
    assert not os.environ.get('CODEX_API_KEY') and not os.environ.get('ANTHROPIC_API_KEY')
    print(json.dumps({'loggedIn':True}));sys.exit(0)
i=json.loads(sys.stdin.read().split('\n',1)[1]); a=i['artifact']
is_claude='--safe-mode' in sys.argv
assert '--bare' not in sys.argv and 'resume' not in sys.argv and '--continue' not in sys.argv
assert ('--no-session-persistence' if is_claude else '--ephemeral') in sys.argv
key=os.environ.get('ANTHROPIC_API_KEY' if is_claude else 'CODEX_API_KEY')
if key is not None: assert key=='sk-ant-agentctl-fixture-secret-never-persist-123456789'
native=pathlib.Path(os.environ.get('CLAUDE_CONFIG_DIR',str(pathlib.Path.home()/'.claude'))) if is_claude else pathlib.Path(os.environ['CODEX_HOME'])
if key is None: assert (native/('.credentials.json' if is_claude else 'auth.json')).read_text()=='sk-ant-agentctl-fixture-secret-never-persist-123456789'
try:
    (native/'history.jsonl').read_text()
    raise AssertionError('unrelated provider history readable')
except (PermissionError,FileNotFoundError): pass
sys.stderr.write('sk-ant-agentctl-fixture-secret-never-persist-123456789')
if i['role']=='EXECUTOR':
    p=a['task']['write_scope'][0]['path'];pathlib.Path(p).write_text(pathlib.Path(p).read_text()+'// native fixture edit\n')
    value=dict(version='1',task_id=i['task_id'],executor_job_id=i['job_id'],status='SUCCEEDED',changed_paths=[p],changed_entities=[],evidence=[],notes=None,failure=None)
else:
    value=dict(version='1',verification_id='verification:'+i['job_id'],target=a['target'],verifier_job_id=i['job_id'],decision='PASS',findings=[],evidence=a['evidence'],requirement_refs=['unit' if i['task_id'] else 'integration'],invariant_refs=[],notes=None)
print(json.dumps({'result':json.dumps(value)}) if is_claude else json.dumps(value))
"#;
            fs::write(&executable, script).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            f.config.providers.insert(
                "test".into(),
                ProviderConfig {
                    adapter: provider.into(),
                    executable,
                    authentication: Authentication {
                        mode: if key_mode {
                            AuthMode::ApiKey
                        } else {
                            AuthMode::Auto
                        },
                        api_key_env: key_mode.then(|| "AGENTCTL_FIXTURE_KEY".into()),
                    },
                },
            );
            fs::write(
                &f.paths.machine_config,
                toml::to_string(&MachineConfig {
                    runtime: f.config.clone(),
                    ..Default::default()
                })
                .unwrap(),
            )
            .unwrap();
            let p = f.plan();
            let invoke = |args: &[&str]| {
                Command::new(env!("CARGO_BIN_EXE_agentctl"))
                    .current_dir(&f.root)
                    .args(args)
                    .env("HOME", &home)
                    .env("CODEX_HOME", home.join(".codex"))
                    .env_remove("CLAUDE_CONFIG_DIR")
                    .env_remove("XDG_CONFIG_HOME")
                    .env_remove("XDG_DATA_HOME")
                    .env_remove("XDG_CACHE_HOME")
                    .env_remove("ANTHROPIC_API_KEY")
                    .env_remove("CODEX_API_KEY")
                    .env_remove("OPENAI_API_KEY")
                    .env("AGENTCTL_FIXTURE_KEY", CANARY)
                    .output()
                    .unwrap()
            };
            let doctor = invoke(&["provider", "doctor", "--json"]);
            assert!(
                doctor.status.success(),
                "{}",
                String::from_utf8_lossy(&doctor.stderr)
            );
            let status: Value = serde_json::from_slice(&doctor.stdout).unwrap();
            assert_eq!(status[0]["authentication"]["authenticated"], true);
            assert_eq!(
                status[0]["authentication"]["method"],
                if key_mode { "API_KEY" } else { "NATIVE" }
            );
            let output = invoke(&["run", "plan", p.packet.plan_id.as_str(), "--json"]);
            if !output.status.success() {
                let a = Artifacts::new(&f.paths.data_root.join("runtime/blobs")).unwrap();
                let errors: Vec<_> = f
                    .store()
                    .runtime_jobs(&f.root, Some(&p.packet.plan_id))
                    .unwrap()
                    .iter()
                    .filter_map(|j| j.stderr.as_ref())
                    .map(|r| String::from_utf8_lossy(&a.get(r).unwrap()).to_string())
                    .collect();
                panic!("fixture provider={provider} api_key={key_mode}: {errors:?}");
            }
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let jobs = f
                .store()
                .runtime_jobs(&f.root, Some(&p.packet.plan_id))
                .unwrap();
            assert_eq!(jobs.len(), 9);
            assert_eq!(
                jobs.iter()
                    .map(|j| &j.session_id)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                9
            );
            let mut pending = vec![f.paths.data_root.clone(), f.paths.config_root.clone()];
            while let Some(path) = pending.pop() {
                for e in fs::read_dir(path).unwrap() {
                    let e = e.unwrap();
                    if e.file_type().unwrap().is_dir() {
                        pending.push(e.path());
                    } else {
                        let bytes = fs::read(e.path()).unwrap();
                        assert!(
                            !String::from_utf8_lossy(&bytes).contains(CANARY),
                            "credential leaked into {}",
                            e.path().display()
                        );
                        assert!(
                            !String::from_utf8_lossy(&bytes)
                                .contains("UNRELATED_CONVERSATION_CANARY")
                        );
                    }
                }
            }
            assert_eq!(fs::read_to_string(credential).unwrap(), CANARY);
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "checks installed native login status and requires Keychain access; no model call"]
fn installed_native_auth_preflight_without_api_keys() {
    use local::runtime::credentials::*;
    for provider in ["codex", "claude"] {
        let path = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join(provider))
            .find(|p| p.is_file())
            .expect("install provider CLI for this opt-in native login test");
        let status = Authentication {
            mode: AuthMode::Native,
            api_key_env: None,
        }
        .preflight(&path, &NativeAuth::discover(provider).unwrap())
        .unwrap();
        assert!(status.authenticated, "{provider}: {}", status.guidance);
        assert!(!status.api_key_required);
    }
}
