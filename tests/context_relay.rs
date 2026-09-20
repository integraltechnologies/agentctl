//! Stage 2 planner-mediated context relay: planner-authored initial context,
//! the typed request/resolution/delta lifecycle, fresh re-issue, machine-owned
//! budgets, planner escalation, verifier independence and diff compaction.
#[allow(dead_code)]
mod common;
use agentctl::{
    local::{
        self,
        config::{MachineConfig, ProjectConfig, VerificationDefinition},
        memory::{MemoryDraft, MemoryKind},
        paths::{MachinePaths, PathContext},
        planning::*,
        repository::RepositoryInfo,
        runtime::{context, manifest, process::*, provider::*, *},
        store::Store,
    },
    protocol::*,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

/// A file large enough that duplicating it would blow the verifier budget.
const LARGE_FILE_LINES: usize = 4200;

/// What a scripted worker does when it is launched.
#[derive(Clone, Debug)]
enum Reply {
    /// Ask for the definition of the first in-envelope callee of the issued
    /// symbol, using only information the base context actually carries.
    RequestCallee,
    /// Ask for a specific item list (identity fields are filled in from the job).
    Request(Vec<ContextRequestItem>, u32),
    /// A request whose identity deliberately names another job/task.
    RequestForeignIdentity,
    /// Edit in scope, then also ask for context (must fail closed).
    EditThenRequest,
    /// Perform the in-scope edit and report success.
    Edit,
    /// Exit nonzero.
    Crash,
    Pass,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Flavor {
    Codex,
    Claude,
}

struct Script {
    executor: Vec<Reply>,
    verifier: Vec<Reply>,
}

/// A provider double that answers from the issued context only, and returns its
/// JSON exactly the way the real adapter for `flavor` expects it, so the relay
/// state machine is exercised through both providers' output paths.
type Confinement = Vec<(AgentRole, Option<local::security::IssuedVisibility>)>;

struct Scripted {
    script: Arc<Mutex<Script>>,
    seen: Arc<Mutex<Vec<JobInput>>>,
    /// The repository confinement each job was launched with.
    issued: Arc<Mutex<Confinement>>,
    flavor: Flavor,
}

impl Scripted {
    fn next(&self, role: AgentRole) -> Reply {
        let mut script = self.script.lock().unwrap();
        let queue = match role {
            AgentRole::Executor => &mut script.executor,
            _ => &mut script.verifier,
        };
        if queue.is_empty() {
            return if role == AgentRole::Executor {
                Reply::Edit
            } else {
                Reply::Pass
            };
        }
        queue.remove(0)
    }
    fn adapter(&self) -> Box<dyn ProviderAdapter> {
        match self.flavor {
            Flavor::Codex => Box::new(CodexAdapter {
                executable: "/usr/bin/true".into(),
                authentication: Default::default(),
            }),
            Flavor::Claude => Box::new(ClaudeAdapter {
                executable: "/usr/bin/true".into(),
                authentication: Default::default(),
            }),
        }
    }
}

/// The first callee the base context offers for the planner-selected symbol.
fn issued_callee(input: &JobInput) -> GraphEntityId {
    let id = input.artifact["context"]["symbols"][0]["callees"][0]["id"]
        .as_str()
        .expect("base context offers an in-envelope callee stub");
    GraphEntityId::new(id).unwrap()
}

fn request(
    input: &JobInput,
    reason: &str,
    items: Vec<ContextRequestItem>,
    max: u32,
) -> ContextRequest {
    ContextRequest {
        version: ProtocolVersion::V1,
        task_id: input.task_id.clone(),
        job_id: input.job_id.clone(),
        reason: reason.into(),
        items,
        max_bytes: max,
    }
}

fn blocked(input: &JobInput, request: ContextRequest) -> Value {
    serde_json::to_value(ResultPacket {
        version: ProtocolVersion::V1,
        task_id: input.task_id.clone().unwrap(),
        executor_job_id: input.job_id.clone(),
        status: ResultStatus::Blocked,
        changed_paths: vec![],
        changed_entities: vec![],
        evidence: vec![],
        notes: None,
        failure: Some(FailureInfo {
            code: CONTEXT_REQUIRED.into(),
            summary: "issued context is insufficient".into(),
        }),
        context_request: Some(request),
    })
    .unwrap()
}

fn edit(process: &ProcessSpec, task: &TaskPacket) -> String {
    let path = task.write_scope[0].path().to_string();
    let prior = fs::read_to_string(process.workspace.join(&path)).unwrap_or_default();
    fs::write(
        process.workspace.join(&path),
        format!(
            "{prior}// accepted fixture change for {}\n",
            task.task_id.as_str()
        ),
    )
    .unwrap();
    path
}

impl ProviderAdapter for Scripted {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            model: true,
            effort: true,
            fresh_session: true,
            structured_output: true,
            token_usage: self.flavor == Flavor::Claude,
        }
    }
    fn launch(
        &mut self,
        input: &JobInput,
        process: ProcessSpec,
        _: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        self.seen.lock().unwrap().push(input.clone());
        self.issued
            .lock()
            .unwrap()
            .push((input.role, process.issued.clone()));
        let reply = self.next(input.role);
        if matches!(reply, Reply::Crash) {
            return Ok(Box::new(Done(Some(ProcessOutput {
                exit: Some(9),
                stdout: vec![],
                stderr: b"fixture crash in re-issued round".to_vec(),
                failure: None,
            }))));
        }
        let value = match input.role {
            AgentRole::Planner => unreachable!("fixture imports plans directly"),
            AgentRole::Executor => {
                let task: TaskPacket =
                    serde_json::from_value(input.artifact["task"].clone()).unwrap();
                match reply {
                    Reply::RequestCallee => blocked(
                        input,
                        request(
                            input,
                            "the callee this task must change is not issued",
                            vec![ContextRequestItem::SymbolDefinition {
                                entity_id: issued_callee(input),
                            }],
                            8192,
                        ),
                    ),
                    Reply::Request(items, max) => {
                        blocked(input, request(input, "scripted request", items, max))
                    }
                    Reply::RequestForeignIdentity => {
                        let mut forged = request(
                            input,
                            "forged identity",
                            vec![ContextRequestItem::SymbolDefinition {
                                entity_id: issued_callee(input),
                            }],
                            4096,
                        );
                        forged.job_id = JobId::new("runtime:someone-else").unwrap();
                        let mut value = blocked(input, forged);
                        // Keep the envelope's own identity fields intact so only
                        // the request's identity is wrong.
                        value["executor_job_id"] = json!(input.job_id);
                        value
                    }
                    Reply::EditThenRequest => {
                        edit(&process, &task);
                        blocked(
                            input,
                            request(
                                input,
                                "asking after editing",
                                vec![ContextRequestItem::SymbolDefinition {
                                    entity_id: issued_callee(input),
                                }],
                                4096,
                            ),
                        )
                    }
                    _ => {
                        let path = edit(&process, &task);
                        serde_json::to_value(ResultPacket {
                            version: ProtocolVersion::V1,
                            task_id: task.task_id,
                            executor_job_id: input.job_id.clone(),
                            status: ResultStatus::Succeeded,
                            changed_paths: vec![path],
                            changed_entities: vec![],
                            evidence: vec![],
                            notes: None,
                            failure: None,
                            context_request: None,
                        })
                        .unwrap()
                    }
                }
            }
            AgentRole::Verifier => {
                let target: VerificationTarget =
                    serde_json::from_value(input.artifact["target"].clone()).unwrap();
                let requirements = if input.task_id.is_some() {
                    vec!["unit".to_string()]
                } else {
                    vec!["integration".to_string()]
                };
                let context_request = match reply {
                    Reply::RequestCallee => Some(request(
                        input,
                        "the changed call target is not in the diff",
                        vec![ContextRequestItem::SymbolByName {
                            name: "store_read".into(),
                        }],
                        8192,
                    )),
                    Reply::Request(items, max) => {
                        Some(request(input, "scripted verifier request", items, max))
                    }
                    _ => None,
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
                    decision: if context_request.is_some() {
                        VerificationDecision::Blocked
                    } else {
                        VerificationDecision::Pass
                    },
                    findings: vec![],
                    evidence: serde_json::from_value(input.artifact["evidence"].clone()).unwrap(),
                    requirement_refs: requirements,
                    invariant_refs: vec![],
                    notes: None,
                    context_request,
                })
                .unwrap()
            }
        };
        let bytes = serde_json::to_vec(&value).unwrap();
        let stdout = match self.flavor {
            Flavor::Codex => bytes,
            // The Claude adapter reads its JSON out of a text result field.
            Flavor::Claude => serde_json::to_vec(&json!({
                "result": String::from_utf8(bytes).unwrap(),
                "usage": {"input_tokens": 11, "output_tokens": 3},
            }))
            .unwrap(),
        };
        Ok(Box::new(Done(Some(ProcessOutput {
            exit: Some(0),
            stdout,
            stderr: vec![],
            failure: None,
        }))))
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        self.adapter().collect(output)
    }
    fn usage(&self, output: &ProcessOutput) -> local::Result<Usage> {
        self.adapter().usage(output)
    }
}

struct Done(Option<ProcessOutput>);
impl RunningProcess for Done {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        Ok(self.0.take())
    }
    fn cancel(&mut self) -> local::Result<CancellationOutcome> {
        Ok(CancellationOutcome::AlreadyExited)
    }
}

struct Checks;
impl CheckLauncher for Checks {
    fn provenance(&self) -> &'static str {
        "DETERMINISTIC_TEST_FIXTURE"
    }
    fn launch(&mut self, _: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        Ok(Box::new(Done(Some(ProcessOutput {
            exit: Some(0),
            stdout: b"captured check output".to_vec(),
            stderr: vec![],
            failure: None,
        }))))
    }
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
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
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(root: &Path, path: &str, text: &str) {
    let full = root.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, text).unwrap();
}

struct Fixture {
    _temp: common::TempDir,
    root: PathBuf,
    paths: MachinePaths,
    config: RuntimeConfig,
    memory: BTreeMap<&'static str, String>,
    /// What every launched job was confined to, in launch order.
    issued: Arc<Mutex<Confinement>>,
}

impl Fixture {
    /// A cache crate whose read scope holds many files, one planner-selected
    /// symbol with a cross-file callee, tests outside the read scope, and a
    /// deliberately large file for diff compaction.
    fn new() -> Self {
        let temp = common::TempDir::new();
        let root = temp.0.join("repo");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        write(&root, "src/lib.rs", "pub mod cache;\npub mod security;\n");
        write(
            &root,
            "src/cache/mod.rs",
            "pub mod api;\npub mod dup_one;\npub mod dup_two;\npub mod large;\npub mod store;\n",
        );
        write(
            &root,
            "src/cache/api.rs",
            "/// CANARY_API\npub fn cache_lookup(key: &str) -> Option<u32> {\n    super::store::store_read(key)\n}\n",
        );
        // Canaries sit inside function bodies: an entity's source range is the
        // declaration itself, so a definition excerpt carries the body, while a
        // whole-file issue also carries the surrounding doc comments.
        write(
            &root,
            "src/cache/store.rs",
            "/// Cache store reads.\npub fn store_read(key: &str) -> Option<u32> {\n    let _ = (key, \"CANARY_STORE\");\n    None\n}\n",
        );
        // Two entities sharing a name make name lookup ambiguous on purpose.
        for suffix in ["one", "two"] {
            write(
                &root,
                &format!("src/cache/dup_{suffix}.rs"),
                "pub fn duplicated_helper() -> u32 {\n    0\n}\n",
            );
        }
        // An individually ignored file and a binary file: neither is issuable.
        write(&root, ".gitignore", "*.log\n");
        write(&root, "src/cache/debug.log", "ignored, never issued\n");
        fs::write(root.join("src/cache/blob.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        // Unrelated files that the old runtime would have injected wholesale.
        for name in ["alpha", "beta", "delta", "gamma", "omega", "sigma", "zulu"] {
            write(
                &root,
                &format!("src/cache/{name}.rs"),
                &format!(
                    "/// CANARY_{}\npub fn cache_{name}() {{}}\n",
                    name.to_uppercase()
                ),
            );
        }
        let large: String = (0..LARGE_FILE_LINES)
            .map(|i| format!("pub const CACHE_ENTRY_{i}: u32 = {i};\n"))
            .collect();
        assert!(
            large.len() > 160 * 1024,
            "large fixture file is {} bytes",
            large.len()
        );
        write(&root, "src/cache/large.rs", &large);
        write(
            &root,
            "src/security/mod.rs",
            "/// CANARY_SECURITY\npub fn enforce_policy() -> bool {\n    true\n}\n",
        );
        write(
            &root,
            "tests/cache.rs",
            "#[test]\nfn cache_lookup_returns_none() {\n    assert!(fixture::cache::api::cache_lookup(\"k\").is_none());\n}\n",
        );
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
                    description: format!("{name} checks"),
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
        git(&root, &["commit", "--quiet", "-m", "cache baseline"]);
        let paths = MachinePaths::resolve(&PathContext {
            home: Some(temp.0.join("home")),
            ..Default::default()
        })
        .unwrap();
        paths.create_directories().unwrap();
        let mut config = RuntimeConfig::default();
        for (name, adapter) in [("codex", "codex"), ("claude", "claude")] {
            config.providers.insert(
                name.into(),
                ProviderConfig {
                    authentication: Default::default(),
                    adapter: adapter.into(),
                    executable: "/usr/bin/true".into(),
                },
            );
        }
        for role in ["planner", "executor", "verifier"] {
            config.roles.insert(
                role.into(),
                RoleConfig {
                    provider: "codex".into(),
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
        let mut store = Store::open(&paths.database, 5000).unwrap();
        store
            .register_repository(RepositoryInfo::discover(&root).unwrap())
            .unwrap();
        assert_eq!(store.index_repository(&root).unwrap().failed, 0);
        // Canonical memory the planner can reference, plus two it did not:
        // one linked inside the read envelope and one outside it.
        let mut memory = BTreeMap::new();
        for (key, note, path) in [
            (
                "selected",
                "Cache lookups must stay allocation free.",
                "src/cache/api.rs",
            ),
            (
                "inside",
                "The cache store returns None until warmed.",
                "src/cache/store.rs",
            ),
            (
                "outside",
                "Policy enforcement is audited separately.",
                "src/security/mod.rs",
            ),
        ] {
            let entry = store
                .add_memory(
                    &root,
                    MemoryDraft {
                        kind: MemoryKind::ArchitectureDecision,
                        content: note.into(),
                        workspace_id: None,
                        canonical_key: None,
                        actor: "fixture".into(),
                        author_job_id: None,
                        links: vec![local::memory::MemoryLink::File { path: path.into() }],
                    },
                    MemoryTrustClass::Canonical,
                    None,
                )
                .unwrap();
            memory.insert(key, entry.id.as_str().to_string());
        }
        Self {
            _temp: temp,
            root,
            paths,
            config,
            memory,
            issued: Arc::new(Mutex::new(vec![])),
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
                    objective: "Make the cache lookup path read through the store".into(),
                    query: Some("cache_lookup".into()),
                    scope: vec![ScopePath::Directory {
                        path: "src/cache".into(),
                    }],
                    constraints: vec!["No Git history mutation".into()],
                    definition_of_done: vec!["Cache lookup reads through the store".into()],
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
    /// A one-task plan: a Directory read scope covering many files, exactly one
    /// planner-selected symbol, one File write target, one memory reference.
    fn plan(&self, write_target: &str) -> ExecutionPlan {
        self.plan_with_scope(
            vec![ScopePath::Directory {
                path: "src/cache".into(),
            }],
            write_target,
        )
    }
    /// The same plan with an explicit read scope, for narrow envelopes whose
    /// expansion needs a planner decision.
    fn plan_with_scope(&self, read_scope: Vec<ScopePath>, write_target: &str) -> ExecutionPlan {
        let prepared = self.prepare();
        let entity = prepared.context.graph.primary[0].entity.clone();
        assert_eq!(
            entity.name, "cache_lookup",
            "fixture selects the lookup symbol"
        );
        let task = TaskPacket {
            version: ProtocolVersion::V1,
            task_id: TaskId::new("task:cache").unwrap(),
            objective: "Read through the cache store".into(),
            read_scope,
            write_scope: vec![ScopePath::File {
                path: write_target.into(),
            }],
            graph_entities: vec![entity.id.clone()],
            invariant_refs: prepared.request.intent.invariant_refs.clone(),
            dependencies: vec![],
            definition_of_done: vec!["Lookup reads through the store".into()],
            verification: requirements("unit"),
        };
        let packet = PlanPacket {
            version: ProtocolVersion::V1,
            plan_id: PlanId::new("plan:relay").unwrap(),
            objective: prepared.request.intent.objective.clone(),
            tasks: vec![task],
            integration_verification: requirements("integration"),
        };
        let plan = ExecutionPlan {
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
                        memory_refs: vec![
                            local::memory::MemoryId::new(self.memory["selected"].clone()).unwrap(),
                        ],
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
        };
        self.store()
            .import_execution_plan(&self.root, &plan)
            .unwrap();
        self.store()
            .activate_execution_plan(&self.root, &plan.packet.plan_id)
            .unwrap();
        plan
    }
    /// Routes every role at the named configured provider, so the relay runs
    /// through that adapter's real output path.
    fn use_provider(&mut self, provider: &str) {
        for role in ["planner", "executor", "verifier"] {
            self.config.roles.insert(
                role.into(),
                RoleConfig {
                    provider: provider.into(),
                    model: Some(format!("opaque-{role}")),
                    effort: None,
                },
            );
        }
    }
    /// Runs the plan with a scripted provider, returning the run outcome and
    /// every issued job input.
    fn run(
        &self,
        plan: &ExecutionPlan,
        script: Script,
    ) -> (local::Result<RunRecord>, Arc<Mutex<Vec<JobInput>>>) {
        let seen = Arc::new(Mutex::new(vec![]));
        let script = Arc::new(Mutex::new(script));
        let mut store = self.store();
        let mut adapters: BTreeMap<String, Box<dyn ProviderAdapter>> = BTreeMap::new();
        for (name, flavor) in [("codex", Flavor::Codex), ("claude", Flavor::Claude)] {
            adapters.insert(
                name.into(),
                Box::new(Scripted {
                    script: script.clone(),
                    seen: seen.clone(),
                    issued: self.issued.clone(),
                    flavor,
                }),
            );
        }
        let outcome = Runtime::new(
            &mut store,
            self.paths.clone(),
            self.config.clone(),
            adapters,
        )
        .unwrap()
        .with_check_launcher(Box::new(Checks))
        .run(&self.root, &plan.packet.plan_id);
        (outcome, seen)
    }
    fn ledger(&self, plan: &ExecutionPlan, subject: &str) -> context::ContextLedger {
        self.store()
            .runtime_status(&self.root, &plan.packet.plan_id)
            .unwrap()
            .unwrap()
            .context
            .get(subject)
            .cloned()
            .unwrap_or_else(|| panic!("no ledger for {subject}"))
    }
    /// Consumes a planner decision on an escalated request, the only path that
    /// can widen a task's relay envelope.
    fn decide(&self, decision: &context::ContextDecision) -> local::Result<RunRecord> {
        let mut store = self.store();
        Runtime::new(
            &mut store,
            self.paths.clone(),
            self.config.clone(),
            BTreeMap::new(),
        )?
        .decide_context(&self.root, decision)
    }
    /// The pending escalation's decision template, as a planner would receive it.
    fn pending_decision(&self, plan: &ExecutionPlan) -> (context::ContextDecision, Value) {
        let report = self.report(plan);
        let subject = report["subjects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["pending_decision"].is_object())
            .expect("a pending escalation")
            .clone();
        let pending = subject["pending_decision"].clone();
        let mut decision: context::ContextDecision =
            serde_json::from_value(pending["decision_template"].clone()).unwrap();
        decision.reason = "The planner intends this expansion".into();
        decision.actor = "planner".into();
        (decision, pending)
    }
    /// The installed CLI, isolated to this fixture's machine state.
    fn cli(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_agentctl"))
            .current_dir(&self.root)
            .args(args)
            .env("HOME", self._temp.0.join("home"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CACHE_HOME")
            .output()
            .unwrap()
    }
    fn report(&self, plan: &ExecutionPlan) -> Value {
        let store = self.store();
        let artifacts = Artifacts::new(&self.paths.data_root.join("runtime/blobs")).unwrap();
        context::report(
            &store,
            &artifacts,
            &self.root,
            &plan.packet.plan_id,
            &self.config.context,
        )
        .unwrap()
    }
}

fn requirements(name: &str) -> VerificationRequirements {
    VerificationRequirements {
        requirement_refs: vec![name.into()],
        evidence_required: true,
    }
}

fn executors(inputs: &[JobInput]) -> Vec<&JobInput> {
    inputs
        .iter()
        .filter(|i| i.role == AgentRole::Executor)
        .collect()
}
fn verifiers(inputs: &[JobInput]) -> Vec<&JobInput> {
    inputs
        .iter()
        .filter(|i| i.role == AgentRole::Verifier)
        .collect()
}
fn prompt(input: &JobInput) -> String {
    String::from_utf8_lossy(&input.compiled.as_ref().unwrap().bytes).into_owned()
}
fn script(executor: Vec<Reply>, verifier: Vec<Reply>) -> Script {
    Script { executor, verifier }
}

/// Part A/L1: a Directory read scope authorizes requests; it injects nothing.
/// The executor receives exactly the planner's references — one symbol with its
/// definition and in-envelope relation stubs, the File write target, the
/// selected memory and the task's checks — and no other repository file.
#[test]
fn directory_read_scope_injects_no_files_and_every_item_traces_to_the_planner() {
    let f = Fixture::new();
    let plan = f.plan("src/cache/api.rs");
    let (outcome, inputs) = f.run(&plan, script(vec![Reply::Edit], vec![]));
    assert_eq!(outcome.unwrap().state, RunState::Complete);
    let inputs = inputs.lock().unwrap();
    let executor = executors(&inputs)[0];
    let issued: context::IssuedContext =
        serde_json::from_value(executor.artifact["context"].clone()).unwrap();
    assert_eq!(issued.symbols.len(), 1);
    assert_eq!(issued.symbols[0].entity.name, "cache_lookup");
    let definition = issued.symbols[0].definition.as_ref().unwrap();
    assert!(definition.text.contains("super::store::store_read"));
    // The one callee is inside the envelope and offered as identity only.
    let stubs = &issued.symbols[0].callees;
    assert_eq!(stubs.len(), 1);
    assert_eq!(stubs[0].name, "store_read");
    // Only the planner's File write target is issued as content.
    assert_eq!(
        issued
            .files
            .iter()
            .map(|x| x.path.as_str())
            .collect::<Vec<_>>(),
        ["src/cache/api.rs"]
    );
    assert_eq!(
        issued.files[0].authority,
        manifest::Authority::PlannerWriteTarget
    );
    assert_eq!(
        issued
            .memory
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        [f.memory["selected"].as_str()]
    );
    assert_eq!(issued.checks.len(), 1);
    assert_eq!(issued.checks[0].requirement, "unit");
    // No unrelated file under the Directory scope reaches the provider, and
    // neither does the store's own body (only its identity).
    let text = prompt(executor);
    for canary in [
        "CANARY_ALPHA",
        "CANARY_BETA",
        "CANARY_DELTA",
        "CANARY_GAMMA",
        "CANARY_OMEGA",
        "CANARY_SIGMA",
        "CANARY_ZULU",
        "CANARY_SECURITY",
        "CANARY_STORE",
        "CACHE_ENTRY_9",
    ] {
        assert!(
            !text.contains(canary),
            "{canary} leaked into executor context"
        );
    }
    assert!(text.contains("CANARY_API"));
    // Every issued byte traces to a planner-authored authority.
    let jobs = f.store().runtime_jobs(&f.root, None).unwrap();
    let manifest = jobs
        .iter()
        .find(|j| j.job_id == executor.job_id)
        .unwrap()
        .context_manifest
        .clone()
        .unwrap();
    let authorities: BTreeSet<manifest::Authority> =
        manifest.issued.iter().map(|i| i.authority).collect();
    assert_eq!(
        authorities,
        BTreeSet::from([
            manifest::Authority::PlannerGraphEntity,
            manifest::Authority::PlannerWriteTarget,
            manifest::Authority::PlannerMemoryRef,
            manifest::Authority::PlannerVerificationRef,
        ])
    );
    assert_eq!(manifest.context_round, Some(0));
    assert!(manifest.context_deltas.is_empty());
    // The manifest describes context; it never copies it.
    let described = serde_json::to_string(&manifest).unwrap();
    for canary in ["CANARY_API", "CANARY_STORE", "allocation free"] {
        assert!(!described.contains(canary), "manifest copied {canary}");
    }
    // Item bytes are exact: each equals the serialized size of that item.
    let value = serde_json::to_value(executor).unwrap();
    let symbol = &value["artifact"]["context"]["symbols"][0];
    let recorded = manifest
        .issued
        .iter()
        .find(|i| i.authority == manifest::Authority::PlannerGraphEntity)
        .unwrap();
    assert_eq!(recorded.bytes, serde_json::to_vec(symbol).unwrap().len());
    assert_eq!(recorded.reference, issued.symbols[0].entity.id.as_str());
}

/// Part B/D/F/L2: an under-contextualized executor requests one symbol, the
/// resolver grants it inside the envelope, and a *fresh* second job receives
/// base plus delta and finishes the task normally.
#[test]
fn an_inside_envelope_request_is_granted_and_re_issued_as_a_fresh_job() {
    let f = Fixture::new();
    let plan = f.plan("src/cache/api.rs");
    let (outcome, inputs) = f.run(
        &plan,
        script(vec![Reply::RequestCallee, Reply::Edit], vec![]),
    );
    assert_eq!(outcome.unwrap().state, RunState::Complete);
    let inputs = inputs.lock().unwrap();
    let rounds = executors(&inputs);
    assert_eq!(rounds.len(), 2, "the grant re-issues a second executor job");
    // A fresh provider job: new job ID, new session, no conversation reuse.
    assert_ne!(rounds[0].job_id, rounds[1].job_id);
    assert_ne!(rounds[0].session_id, rounds[1].session_id);
    // Round 2 carries the same base context plus exactly one delta.
    assert_eq!(rounds[0].artifact["context"], rounds[1].artifact["context"]);
    assert_eq!(rounds[0].artifact["deltas"].as_array().unwrap().len(), 0);
    let deltas = rounds[1].artifact["deltas"].as_array().unwrap();
    assert_eq!(deltas.len(), 1);
    let delta: context::ContextDelta = serde_json::from_value(deltas[0].clone()).unwrap();
    assert_eq!(delta.round, 1);
    assert_eq!(delta.parent_job_id, rounds[0].job_id);
    assert!(matches!(delta.grant, context::Grant::Automatic));
    assert_eq!(delta.items.len(), 1);
    let context::DeltaItem::Symbol {
        entity, definition, ..
    } = &delta.items[0]
    else {
        panic!("expected a symbol definition, got {:?}", delta.items[0]);
    };
    assert_eq!(entity.name, "store_read");
    assert!(definition.as_ref().unwrap().text.contains("CANARY_STORE"));
    // Round 2 sees the granted body and still nothing unrelated.
    let text = prompt(rounds[1]);
    assert!(text.contains("CANARY_STORE") && text.contains("CANARY_API"));
    for canary in ["CANARY_ALPHA", "CANARY_SECURITY", "CACHE_ENTRY_9"] {
        assert!(!text.contains(canary), "{canary} leaked after the grant");
    }
    // The ledger and the manifest both record the grant, hash-bound.
    let ledger = f.ledger(&plan, "executor:task:cache");
    assert_eq!(ledger.state, context::LedgerState::Open);
    assert_eq!(ledger.rounds_used(), 1);
    assert_eq!(ledger.rounds.len(), 1);
    assert_eq!(ledger.rounds[0].outcome, context::RoundOutcome::Granted);
    let recorded = ledger.rounds[0].delta.as_ref().unwrap();
    assert_eq!(recorded.delta_id, delta.delta_id);
    assert_eq!(ledger.granted_bytes, delta.bytes);
    let jobs = f.store().runtime_jobs(&f.root, None).unwrap();
    let requester = jobs.iter().find(|j| j.job_id == rounds[0].job_id).unwrap();
    assert!(
        requester.context_request.is_some(),
        "the request is persisted"
    );
    let second = jobs
        .iter()
        .find(|j| j.job_id == rounds[1].job_id)
        .unwrap()
        .context_manifest
        .clone()
        .unwrap();
    assert_eq!(second.context_round, Some(1));
    assert_eq!(second.context_deltas.len(), 1);
    assert_eq!(second.context_deltas[0].delta_id, delta.delta_id);
    assert_eq!(second.context_deltas[0].hash, recorded.artifact.hash);
    assert_eq!(second.context_deltas[0].bytes, delta.bytes);
    assert!(!second.context_deltas[0].planner_approved);
    assert!(
        second
            .issued
            .iter()
            .any(|i| i.authority == manifest::Authority::ContextDelta
                && i.delta_id.as_deref() == Some(delta.delta_id.as_str()))
    );
    // The journal reconstructs the exchange.
    let phases = journal_phases(&f);
    for phase in [
        "CONTEXT_BASE_ISSUED",
        "CONTEXT_REQUESTED",
        "CONTEXT_DELTA_GRANTED",
    ] {
        assert!(
            phases.contains(&phase.to_string()),
            "missing {phase}: {phases:?}"
        );
    }
}

/// Part C/K: an executor that edited the workspace cannot also ask for context.
#[test]
fn a_context_request_after_editing_fails_closed() {
    let f = Fixture::new();
    let plan = f.plan("src/cache/api.rs");
    let (outcome, inputs) = f.run(&plan, script(vec![Reply::EditThenRequest], vec![]));
    let message = outcome.unwrap_err().to_string();
    assert!(message.contains("CONTEXT_REQUEST_WITH_EDITS"), "{message}");
    assert_eq!(executors(&inputs.lock().unwrap()).len(), 1);
    let run = f
        .store()
        .runtime_status(&f.root, &plan.packet.plan_id)
        .unwrap()
        .unwrap();
    assert_eq!(run.state, RunState::Blocked);
    // No delta exists, and the edit was neither accepted nor rolled back.
    assert_eq!(f.ledger(&plan, "executor:task:cache").rounds_used(), 0);
    assert!(
        fs::read_to_string(f.root.join("src/cache/api.rs"))
            .unwrap()
            .contains("accepted fixture change")
    );
    let tasks = f
        .store()
        .tasks(
            &RepositoryInfo::discover(&f.root).unwrap().repository_id,
            Some(&plan.packet.plan_id),
        )
        .unwrap();
    assert_eq!(tasks[0].state, TaskState::Blocked);
}

/// Part E/L3: a request that needs a path outside the envelope is never
/// granted automatically. The run blocks for a planner, the executor cannot
/// widen its own scope by retrying, and a denial leaves the task blocked.
#[test]
fn an_out_of_envelope_request_escalates_and_a_denial_keeps_the_task_blocked() {
    let f = Fixture::new();
    let plan = f.plan("src/cache/api.rs");
    let outside = vec![ContextRequestItem::FileRange {
        path: "src/security/mod.rs".into(),
        start_line: 1,
        end_line: 10,
    }];
    let (outcome, inputs) = f.run(
        &plan,
        script(vec![Reply::Request(outside, 8192), Reply::Edit], vec![]),
    );
    let message = outcome.unwrap_err().to_string();
    assert!(
        message.contains("NEEDS_PLANNER_CONTEXT_APPROVAL"),
        "{message}"
    );
    assert_eq!(
        executors(&inputs.lock().unwrap()).len(),
        1,
        "no fresh job without a planner decision"
    );
    let ledger = f.ledger(&plan, "executor:task:cache");
    assert_eq!(
        ledger.state,
        context::LedgerState::NeedsPlannerContextApproval
    );
    assert_eq!(ledger.rounds_used(), 0, "nothing was granted");
    assert_eq!(ledger.rounds[0].outcome, context::RoundOutcome::Escalated);
    assert!(ledger.approved_scope.is_empty());
    assert_eq!(ledger.escalations, 1);
    // The planner sees exactly which paths are involved; the worker got none.
    let (_, pending) = f.pending_decision(&plan);
    assert_eq!(pending["outside_paths"], json!(["src/security/mod.rs"]));
    let (_, seen) = f.run(&plan, script(vec![Reply::Edit], vec![]));
    assert!(
        seen.lock().unwrap().is_empty(),
        "a blocked relay launches nothing"
    );
    // Denial: the task stays blocked and still nothing is granted.
    let (mut deny, _) = f.pending_decision(&plan);
    deny.decision = context::DecisionKind::Deny;
    deny.read_scope_additions.clear();
    let run = f.decide(&deny).unwrap();
    assert_eq!(run.state, RunState::Blocked);
    let ledger = f.ledger(&plan, "executor:task:cache");
    assert_eq!(ledger.state, context::LedgerState::PlannerDenied);
    assert_eq!(ledger.rounds_used(), 0);
    assert_eq!(
        ledger.rounds[0].outcome,
        context::RoundOutcome::PlannerDenied
    );
    let (after, seen) = f.run(&plan, script(vec![Reply::Edit], vec![]));
    assert!(after.is_err());
    assert!(seen.lock().unwrap().is_empty());
    assert!(
        journal_phases(&f).contains(&"CONTEXT_ESCALATION_DENIED".to_string()),
        "{:?}",
        journal_phases(&f)
    );
    let artifacts = Artifacts::new(&f.paths.data_root.join("runtime/blobs")).unwrap();
    let report = f
        .store()
        .control_plane_capabilities(&artifacts, &f.root, &plan.packet.plan_id)
        .unwrap();
    assert_eq!(
        report
            .capabilities
            .iter()
            .find(|finding| {
                finding.capability == ControlPlaneCapability::MediateContextEscalation
            })
            .unwrap()
            .status,
        CapabilityStatus::Supported
    );
}

/// Part E/F: an approval is validated and re-resolved under the wider
/// envelope, and `run resume` re-issues the task as a fresh job carrying the
/// planner-approved delta.
#[test]
fn a_planner_approval_widens_the_envelope_and_re_issues_a_fresh_job() {
    let f = Fixture::new();
    // A deliberately narrow task envelope: one file, while the plan's own
    // request scope covers the whole cache module.
    let plan = f.plan_with_scope(
        vec![ScopePath::File {
            path: "src/cache/api.rs".into(),
        }],
        "src/cache/api.rs",
    );
    let sibling = vec![ContextRequestItem::FileRange {
        path: "src/cache/store.rs".into(),
        start_line: 1,
        end_line: 10,
    }];
    let (outcome, _) = f.run(&plan, script(vec![Reply::Request(sibling, 8192)], vec![]));
    assert!(outcome.is_err());
    let (approval, _) = f.pending_decision(&plan);
    assert_eq!(
        approval.read_scope_additions,
        vec![ScopePath::File {
            path: "src/cache/store.rs".into()
        }]
    );
    let run = f.decide(&approval).unwrap();
    assert_eq!(run.state, RunState::Running);
    let ledger = f.ledger(&plan, "executor:task:cache");
    assert_eq!(ledger.state, context::LedgerState::Open);
    assert_eq!(ledger.rounds_used(), 1);
    assert_eq!(ledger.rounds[0].outcome, context::RoundOutcome::Approved);
    assert_eq!(ledger.approved_scope, approval.read_scope_additions);
    // A second decision cannot be replayed onto a settled ledger.
    let message = f.decide(&approval).unwrap_err().to_string();
    assert!(
        message.contains("no escalated context request"),
        "{message}"
    );
    let (resumed, inputs) = f.run(&plan, script(vec![Reply::Edit], vec![]));
    assert_eq!(resumed.unwrap().state, RunState::Complete);
    let inputs = inputs.lock().unwrap();
    let executor = executors(&inputs)[0];
    let deltas = executor.artifact["deltas"].as_array().unwrap();
    assert_eq!(deltas.len(), 1);
    let delta: context::ContextDelta = serde_json::from_value(deltas[0].clone()).unwrap();
    let context::Grant::PlannerApproved {
        decision_hash,
        scope_additions,
    } = &delta.grant
    else {
        panic!("expected a planner-approved grant, got {:?}", delta.grant);
    };
    assert!(decision_hash.starts_with("blake3:"));
    assert_eq!(scope_additions, &approval.read_scope_additions);
    assert!(prompt(executor).contains("CANARY_STORE"));
    let jobs = f.store().runtime_jobs(&f.root, None).unwrap();
    let manifest = jobs
        .iter()
        .find(|j| j.job_id == executor.job_id)
        .unwrap()
        .context_manifest
        .clone()
        .unwrap();
    assert!(manifest.context_deltas[0].planner_approved);
    assert!(journal_phases(&f).contains(&"CONTEXT_ESCALATION_APPROVED".to_string()));
    let artifacts = Artifacts::new(&f.paths.data_root.join("runtime/blobs")).unwrap();
    let report = f
        .store()
        .control_plane_capabilities(&artifacts, &f.root, &plan.packet.plan_id)
        .unwrap();
    assert_eq!(
        report
            .capabilities
            .iter()
            .find(|finding| {
                finding.capability == ControlPlaneCapability::MediateContextEscalation
            })
            .unwrap()
            .status,
        CapabilityStatus::Supported
    );

    // An otherwise valid decision from another request/round cannot be
    // borrowed by this round in the derived evidence join.
    let mut borrowed = approval.clone();
    borrowed.request_hash = "blake3:another-request".into();
    let borrowed_ref = artifacts.json(&borrowed).unwrap();
    let mut run = f
        .store()
        .runtime_status(&f.root, &plan.packet.plan_id)
        .unwrap()
        .unwrap();
    run.context.get_mut("executor:task:cache").unwrap().rounds[0].decision = Some(borrowed_ref);
    let db = common::sql(&f.paths.database);
    let runtime_runs_update: String = db
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type='trigger' AND name='runtime_runs_update'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    db.execute_batch("DROP TRIGGER runtime_runs_update;")
        .unwrap();
    db.execute(
        "UPDATE runtime_runs SET record_json=?1 WHERE plan_id=?2",
        rusqlite::params![
            serde_json::to_string(&run).unwrap(),
            plan.packet.plan_id.as_str()
        ],
    )
    .unwrap();
    db.execute_batch(&runtime_runs_update).unwrap();
    let attacked = f
        .store()
        .control_plane_capabilities(&artifacts, &f.root, &plan.packet.plan_id)
        .unwrap();
    assert_eq!(
        attacked
            .capabilities
            .iter()
            .find(|finding| {
                finding.capability == ControlPlaneCapability::MediateContextEscalation
            })
            .unwrap()
            .status,
        CapabilityStatus::NotDemonstrated
    );
}

/// Part 7/L5: machine-owned round budgets end the loop, whatever the worker asks.
#[test]
fn repeated_requests_stop_at_the_machine_owned_round_budget() {
    let mut f = Fixture::new();
    f.config.context.max_rounds = 1;
    let plan = f.plan("src/cache/api.rs");
    let (outcome, inputs) = f.run(
        &plan,
        script(
            vec![Reply::RequestCallee, Reply::RequestCallee, Reply::Edit],
            vec![],
        ),
    );
    let message = outcome.unwrap_err().to_string();
    assert!(
        message.contains("CONTEXT_REQUEST_DENIED: ROUNDS_EXHAUSTED"),
        "{message}"
    );
    assert_eq!(
        executors(&inputs.lock().unwrap()).len(),
        2,
        "one grant, then the next request is refused"
    );
    let ledger = f.ledger(&plan, "executor:task:cache");
    assert_eq!(ledger.state, context::LedgerState::Denied);
    assert_eq!(ledger.rounds_used(), 1);
    assert_eq!(ledger.rounds.len(), 2);
    assert_eq!(ledger.rounds[1].outcome, context::RoundOutcome::Denied);
    let run = f
        .store()
        .runtime_status(&f.root, &plan.packet.plan_id)
        .unwrap()
        .unwrap();
    assert_eq!(run.state, RunState::Blocked);
}

/// Part 7/K: the per-round byte budget refuses one oversized request.
#[test]
fn a_request_over_the_round_byte_budget_is_denied() {
    let mut f = Fixture::new();
    f.config.context.max_round_bytes = 1024;
    f.config.context.max_task_bytes = 4096;
    let plan = f.plan("src/cache/api.rs");
    let large = vec![ContextRequestItem::FileRange {
        path: "src/cache/large.rs".into(),
        start_line: 1,
        end_line: 300,
    }];
    let (outcome, _) = f.run(&plan, script(vec![Reply::Request(large, 32768)], vec![]));
    let message = outcome.unwrap_err().to_string();
    assert!(message.contains("ROUND_BUDGET_EXCEEDED"), "{message}");
    let ledger = f.ledger(&plan, "executor:task:cache");
    assert_eq!(ledger.state, context::LedgerState::Denied);
    assert_eq!(ledger.granted_bytes, 0);
}

/// Part 7/K: the cumulative budget ends a relay that stays inside the
/// per-round limit but adds up across rounds.
#[test]
fn accumulated_deltas_stop_at_the_cumulative_task_byte_budget() {
    let mut f = Fixture::new();
    f.config.context.max_round_bytes = 8192;
    f.config.context.max_task_bytes = 8192;
    f.config.context.max_rounds = 4;
    let plan = f.plan("src/cache/api.rs");
    let range = |start: u32, end: u32| {
        Reply::Request(
            vec![ContextRequestItem::FileRange {
                path: "src/cache/large.rs".into(),
                start_line: start,
                end_line: end,
            }],
            8192,
        )
    };
    let (outcome, inputs) = f.run(
        &plan,
        script(vec![range(1, 150), range(151, 250), Reply::Edit], vec![]),
    );
    let message = outcome.unwrap_err().to_string();
    assert!(message.contains("TASK_BUDGET_EXHAUSTED"), "{message}");
    assert_eq!(executors(&inputs.lock().unwrap()).len(), 2);
    let ledger = f.ledger(&plan, "executor:task:cache");
    assert_eq!(ledger.state, context::LedgerState::Denied);
    assert_eq!(ledger.rounds_used(), 1, "only the first round was granted");
    assert!(ledger.granted_bytes > 0 && ledger.granted_bytes <= 8192);
}

/// Part E: an approval can never exceed the plan's own requested scope, so a
/// planner cannot use an escalation to leave the plan's boundary.
#[test]
fn an_approval_cannot_exceed_the_planning_requests_own_scope() {
    let f = Fixture::new();
    let plan = f.plan_with_scope(
        vec![ScopePath::File {
            path: "src/cache/api.rs".into(),
        }],
        "src/cache/api.rs",
    );
    let outside = vec![ContextRequestItem::FileRange {
        path: "src/security/mod.rs".into(),
        start_line: 1,
        end_line: 5,
    }];
    let (outcome, _) = f.run(&plan, script(vec![Reply::Request(outside, 4096)], vec![]));
    assert!(
        outcome
            .unwrap_err()
            .to_string()
            .contains("NEEDS_PLANNER_CONTEXT_APPROVAL")
    );
    let (approval, _) = f.pending_decision(&plan);
    let message = f.decide(&approval).unwrap_err().to_string();
    assert!(
        message.contains("exceeds the planning request's own scope"),
        "{message}"
    );
    // A refused decision changes nothing: the escalation is still pending.
    let ledger = f.ledger(&plan, "executor:task:cache");
    assert_eq!(
        ledger.state,
        context::LedgerState::NeedsPlannerContextApproval
    );
    assert_eq!(ledger.rounds_used(), 0);
}

/// Part G/L6: a one-line edit to a ~160 KiB file produces a verifier diff of a
/// few KiB that still carries both content hashes and the changed hunk.
#[test]
fn a_one_line_edit_to_a_large_file_keeps_verifier_diff_context_small() {
    let f = Fixture::new();
    let plan = f.plan("src/cache/large.rs");
    let (outcome, inputs) = f.run(&plan, script(vec![Reply::Edit], vec![]));
    assert_eq!(outcome.unwrap().state, RunState::Complete);
    let size = fs::metadata(f.root.join("src/cache/large.rs"))
        .unwrap()
        .len();
    assert!(size >= 160 * 1024, "fixture file is only {size} bytes");
    let inputs = inputs.lock().unwrap();
    for verifier in verifiers(&inputs) {
        let diff = &verifier.artifact["diff"];
        let bytes = serde_json::to_vec(diff).unwrap().len();
        assert!(
            bytes <= 8 * 1024,
            "verifier diff context is {bytes} bytes for a {size}-byte file"
        );
        let change = &diff["changes"][0];
        assert_eq!(change["path"], json!("src/cache/large.rs"));
        assert_eq!(change["status"], json!("MODIFIED"));
        let hunks = change["hunks"].as_array().unwrap();
        assert_eq!(hunks.len(), 1, "one edit, one hunk");
        let lines = hunks[0]["lines"].as_array().unwrap();
        assert!(lines.len() <= 2 * 3 + 2, "hunk carries bounded context");
        assert!(lines.iter().any(|l| {
            l.as_str()
                .unwrap()
                .starts_with("+// accepted fixture change")
        }));
        // Hash binding survives compaction.
        let binding = &diff["binding"]["changes"][0];
        for side in ["before", "after"] {
            assert!(
                binding[side]["content"]["hash"]
                    .as_str()
                    .unwrap()
                    .starts_with("blake3:")
            );
        }
        // The whole file is never duplicated into the prompt.
        let text = prompt(verifier);
        assert!(!text.contains("CACHE_ENTRY_4000"));
    }
}

/// Part H/L7: the verifier relay is independent. It re-issues a fresh verifier
/// with its own delta and never carries executor request history.
#[test]
fn the_verifier_relay_is_independent_of_executor_request_history() {
    let f = Fixture::new();
    let plan = f.plan("src/cache/api.rs");
    let (outcome, inputs) = f.run(
        &plan,
        script(
            vec![Reply::RequestCallee, Reply::Edit],
            vec![Reply::RequestCallee],
        ),
    );
    assert_eq!(outcome.unwrap().state, RunState::Complete);
    let inputs = inputs.lock().unwrap();
    let packet: Vec<&JobInput> = verifiers(&inputs)
        .into_iter()
        .filter(|i| i.task_id.is_some())
        .collect();
    assert_eq!(packet.len(), 2, "the verifier was re-issued, not resumed");
    assert_ne!(packet[0].job_id, packet[1].job_id);
    assert_ne!(packet[0].session_id, packet[1].session_id);
    assert!(packet[0].artifact["deltas"].as_array().unwrap().is_empty());
    let deltas = packet[1].artifact["deltas"].as_array().unwrap();
    assert_eq!(deltas.len(), 1);
    let delta: context::ContextDelta = serde_json::from_value(deltas[0].clone()).unwrap();
    assert_eq!(delta.role, AgentRole::Verifier);
    assert_eq!(delta.parent_job_id, packet[0].job_id);
    let context::DeltaItem::Symbol { entity, .. } = &delta.items[0] else {
        panic!("expected the named symbol, got {:?}", delta.items[0]);
    };
    assert_eq!(entity.name, "store_read");
    // Ledgers and budgets are separate, and no executor history leaks.
    let executor = f.ledger(&plan, "executor:task:cache");
    let verifier = f.ledger(&plan, "verifier:task:cache");
    assert_eq!(verifier.role, AgentRole::Verifier);
    assert_eq!((executor.rounds_used(), verifier.rounds_used()), (1, 1));
    let executor_delta = executor.deltas().next().unwrap().delta_id.clone();
    let executor_request = "the callee this task must change is not issued";
    for input in &packet {
        let text = prompt(input);
        assert!(!text.contains(executor_request), "executor reason leaked");
        assert!(!text.contains(&executor_delta), "executor delta ID leaked");
        assert!(
            !text.contains("\"context\":{\"version\":\"agentctl-issued-context-1\""),
            "executor base context leaked"
        );
    }
}

/// Part H/K: a verifier cannot escalate, so an out-of-envelope verifier
/// request is denied and reaches no decision.
#[test]
fn an_out_of_envelope_verifier_request_is_denied_without_escalation() {
    let f = Fixture::new();
    let plan = f.plan("src/cache/api.rs");
    let outside = vec![ContextRequestItem::FileRange {
        path: "src/security/mod.rs".into(),
        start_line: 1,
        end_line: 5,
    }];
    let (outcome, _) = f.run(
        &plan,
        script(vec![Reply::Edit], vec![Reply::Request(outside, 4096)]),
    );
    let message = outcome.unwrap_err().to_string();
    assert!(
        message.contains("VERIFIER_CONTEXT_REQUEST_DENIED")
            && message.contains("ESCALATION_NOT_PERMITTED"),
        "{message}"
    );
    let ledger = f.ledger(&plan, "verifier:task:cache");
    assert_eq!(ledger.state, context::LedgerState::Denied);
    assert_eq!(ledger.rounds_used(), 0);
    // The task neither passed nor was rejected.
    let tasks = f
        .store()
        .tasks(
            &RepositoryInfo::discover(&f.root).unwrap().repository_id,
            Some(&plan.packet.plan_id),
        )
        .unwrap();
    assert_eq!(tasks[0].state, TaskState::Blocked);
}

/// Part 9/L8: source or ontology drift between rounds fails closed rather than
/// issuing a delta derived from stale assumptions.
#[test]
fn drift_between_context_rounds_fails_closed() {
    for reindex in [false, true] {
        let f = Fixture::new();
        let plan = f.plan_with_scope(
            vec![ScopePath::File {
                path: "src/cache/api.rs".into(),
            }],
            "src/cache/api.rs",
        );
        let outside = vec![ContextRequestItem::FileRange {
            path: "src/cache/store.rs".into(),
            start_line: 1,
            end_line: 5,
        }];
        let (outcome, _) = f.run(&plan, script(vec![Reply::Request(outside, 4096)], vec![]));
        assert!(outcome.is_err());
        let (approval, _) = f.pending_decision(&plan);
        // The repository moves under the escalation, with or without a reindex.
        fs::write(
            f.root.join("src/cache/store.rs"),
            "/// Cache store reads.\npub fn store_read(key: &str) -> Option<u32> {\n    let _ = (key, \"CANARY_STORE\");\n    Some(1)\n}\n",
        )
        .unwrap();
        if reindex {
            f.store().index_repository(&f.root).unwrap();
        }
        let message = f.decide(&approval).unwrap_err().to_string();
        assert!(
            message.contains("SOURCE_DRIFT"),
            "reindex={reindex}: {message}"
        );
        assert_eq!(
            f.ledger(&plan, "executor:task:cache").state,
            context::LedgerState::NeedsPlannerContextApproval,
            "the escalation is untouched"
        );
    }
}

/// Stage 3: a context base and its escalation are bound to the ontology
/// generation they were derived from. When the accepted generation moves on,
/// they stay inspectable but never become usable again, not even when the
/// source returns to byte-identical content (same fingerprint, new position).
#[test]
fn context_from_an_older_ontology_generation_is_never_reused_after_it_moves() {
    use local::graph::{DecisionReason, GenerationState};
    let f = Fixture::new();
    let accepted = f
        .store()
        .ontology_status(&f.root)
        .unwrap()
        .accepted
        .unwrap();
    let plan = f.plan_with_scope(
        vec![ScopePath::File {
            path: "src/cache/api.rs".into(),
        }],
        "src/cache/api.rs",
    );
    let outside = vec![ContextRequestItem::FileRange {
        path: "src/cache/store.rs".into(),
        start_line: 1,
        end_line: 5,
    }];
    let (outcome, _) = f.run(&plan, script(vec![Reply::Request(outside, 4096)], vec![]));
    assert!(outcome.is_err());
    let base = f.ledger(&plan, "executor:task:cache").base.clone().unwrap();
    assert_eq!(base.graph_generation.as_ref(), Some(&accepted.generation));
    let (approval, _) = f.pending_decision(&plan);
    let original = fs::read_to_string(f.root.join("src/cache/store.rs")).unwrap();
    // An external edit is observed: a candidate, not accepted truth.
    fs::write(
        f.root.join("src/cache/store.rs"),
        original.replace("None", "Some(7)"),
    )
    .unwrap();
    f.store().index_repository(&f.root).unwrap();
    let status = f.store().ontology_status(&f.root).unwrap();
    assert_eq!(status.accepted.as_ref(), Some(&accepted));
    assert!(status.candidate.is_some());
    let message = f.decide(&approval).unwrap_err().to_string();
    assert!(message.contains("SOURCE_DRIFT"), "{message}");
    // The source returns to the original bytes: the re-observation is accepted
    // mechanically, with the original fingerprint at a later sequence.
    fs::write(f.root.join("src/cache/store.rs"), &original).unwrap();
    f.store().index_repository(&f.root).unwrap();
    let now = f
        .store()
        .ontology_status(&f.root)
        .unwrap()
        .accepted
        .unwrap();
    assert_eq!(
        now.acceptance.as_ref().unwrap().reason,
        DecisionReason::IdenticalToAccepted
    );
    assert_eq!(now.generation.fingerprint, accepted.generation.fingerprint);
    assert_ne!(now.generation, accepted.generation);
    assert_eq!(
        f.store()
            .ontology_generation(&f.root, &accepted.generation_id)
            .unwrap()
            .state,
        GenerationState::Retired
    );
    // The old base is still bound to the retired position, so the old
    // escalation cannot be approved against the new accepted generation.
    let message = f.decide(&approval).unwrap_err().to_string();
    assert!(message.contains("SOURCE_DRIFT"), "{message}");
    let ledger = f.ledger(&plan, "executor:task:cache");
    assert_eq!(
        ledger.state,
        context::LedgerState::NeedsPlannerContextApproval
    );
    assert_eq!(ledger.base.as_ref(), Some(&base));
    assert_eq!(ledger.rounds_used(), 0);
    // History remains inspectable.
    let (again, pending) = f.pending_decision(&plan);
    assert_eq!(again.request_hash, approval.request_hash);
    assert_eq!(pending["outside_paths"], json!(["src/cache/store.rs"]));
    let (resumed, seen) = f.run(&plan, script(vec![Reply::Edit], vec![]));
    assert!(resumed.is_err());
    assert!(seen.lock().unwrap().is_empty(), "nothing is re-issued");
}

/// Part L9: the same relay state machine, driven through both providers' real
/// output paths (Codex returns bare JSON; Claude wraps it in a result field).
#[test]
fn the_relay_runs_through_fake_claude_and_codex_output_paths() {
    for provider in ["codex", "claude"] {
        let mut f = Fixture::new();
        f.use_provider(provider);
        let plan = f.plan("src/cache/api.rs");
        let (outcome, inputs) = f.run(
            &plan,
            script(
                vec![Reply::RequestCallee, Reply::Edit],
                vec![Reply::RequestCallee],
            ),
        );
        assert_eq!(
            outcome.unwrap().state,
            RunState::Complete,
            "provider {provider}"
        );
        let inputs = inputs.lock().unwrap();
        assert_eq!(executors(&inputs).len(), 2, "provider {provider}");
        assert_eq!(
            verifiers(&inputs)
                .iter()
                .filter(|i| i.task_id.is_some())
                .count(),
            2,
            "provider {provider}"
        );
        assert_eq!(
            (
                f.ledger(&plan, "executor:task:cache").rounds_used(),
                f.ledger(&plan, "verifier:task:cache").rounds_used()
            ),
            (1, 1),
            "provider {provider}"
        );
    }
}

/// Part 11/L10: manifests account for base and delta bytes exactly, and never
/// copy repository text.
#[test]
fn manifests_account_base_and_delta_bytes_exactly_without_copying_source() {
    let f = Fixture::new();
    let plan = f.plan("src/cache/api.rs");
    let (outcome, inputs) = f.run(
        &plan,
        script(
            vec![Reply::RequestCallee, Reply::Edit],
            vec![Reply::RequestCallee],
        ),
    );
    assert_eq!(outcome.unwrap().state, RunState::Complete);
    let inputs = inputs.lock().unwrap();
    let jobs = f.store().runtime_jobs(&f.root, None).unwrap();
    assert!(jobs.len() >= 5);
    for job in &jobs {
        let m = job.context_manifest.clone().unwrap();
        let input = inputs.iter().find(|i| i.job_id == job.job_id).unwrap();
        let value = serde_json::to_value(input).unwrap();
        let compiled = input.compiled.as_ref().unwrap().bytes.len();
        assert_eq!(m.bytes.total, compiled);
        assert_eq!(
            m.bytes.categories.iter().map(|c| c.bytes).sum::<usize>(),
            compiled
        );
        // Every issued item's recorded bytes are that item's serialized size.
        let mut expected = 0usize;
        for key in ["symbols", "files", "memory", "checks"] {
            if let Some(items) = value["artifact"]["context"][key].as_array() {
                expected += items
                    .iter()
                    .map(|i| serde_json::to_vec(i).unwrap().len())
                    .sum::<usize>();
            }
        }
        if let Some(deltas) = value["artifact"]["deltas"].as_array() {
            for delta in deltas {
                expected += delta["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| serde_json::to_vec(i).unwrap().len())
                    .sum::<usize>();
            }
        }
        assert_eq!(
            m.issued.iter().map(|i| i.bytes).sum::<usize>(),
            expected,
            "issued accounting for {:?}",
            job.role
        );
        // Deltas are recorded by ID, hash, bytes and round, never by content.
        let described = serde_json::to_string(&m).unwrap();
        for canary in [
            "CANARY_API",
            "CANARY_STORE",
            "CANARY_SECURITY",
            "CACHE_ENTRY_1",
            "allocation free",
            "accepted fixture change",
        ] {
            assert!(!described.contains(canary), "manifest copied {canary}");
        }
    }
}

/// Part D: memory is dereferenced, not searched. A memory entry linked inside
/// the envelope resolves; the planner's own reference is already issued.
#[test]
fn memory_inside_the_envelope_resolves_and_outside_it_escalates() {
    let f = Fixture::new();
    let plan = f.plan("src/cache/api.rs");
    let inside = vec![ContextRequestItem::Memory {
        memory_id: f.memory["inside"].clone(),
    }];
    let (outcome, inputs) = f.run(
        &plan,
        script(vec![Reply::Request(inside, 8192), Reply::Edit], vec![]),
    );
    assert_eq!(outcome.unwrap().state, RunState::Complete);
    let inputs = inputs.lock().unwrap();
    let rounds = executors(&inputs);
    let delta: context::ContextDelta =
        serde_json::from_value(rounds[1].artifact["deltas"][0].clone()).unwrap();
    let context::DeltaItem::Memory { memory, .. } = &delta.items[0] else {
        panic!("expected memory, got {:?}", delta.items[0]);
    };
    assert_eq!(memory.id, f.memory["inside"]);
    assert!(memory.content.contains("returns None until warmed"));
    assert_eq!(memory.trust, MemoryTrustClass::Canonical);
}

/// Part K: every malformed, foreign, stale, ambiguous, unissuable or oversized
/// request fails closed, grants nothing, and leaves understandable state.
#[test]
fn malformed_foreign_stale_and_unissuable_requests_all_fail_closed() {
    let entity = |id: &str| ContextRequestItem::SymbolDefinition {
        entity_id: GraphEntityId::new(id).unwrap(),
    };
    let range = |path: &str, start: u32, end: u32| ContextRequestItem::FileRange {
        path: path.into(),
        start_line: start,
        end_line: end,
    };
    let cases: Vec<(&str, Reply, &str)> = vec![
        (
            "foreign job identity",
            Reply::RequestForeignIdentity,
            "context_request",
        ),
        ("no items", Reply::Request(vec![], 4096), "1–16 items"),
        (
            "zero byte budget",
            Reply::Request(vec![entity("graph:0123456789abcdef")], 0),
            "max_bytes",
        ),
        (
            "absent entity",
            Reply::Request(vec![entity("graph:0123456789abcdef")], 4096),
            "ITEM_UNRESOLVABLE",
        ),
        (
            "ambiguous name",
            Reply::Request(
                vec![ContextRequestItem::SymbolByName {
                    name: "duplicated_helper".into(),
                }],
                4096,
            ),
            "ITEM_UNRESOLVABLE",
        ),
        (
            "oversized range",
            Reply::Request(vec![range("src/cache/large.rs", 1, 500)], 4096),
            "400 lines",
        ),
        (
            "excessive neighborhood depth",
            Reply::Request(
                vec![ContextRequestItem::Neighborhood {
                    entity_id: GraphEntityId::new("graph:0123456789abcdef").unwrap(),
                    depth: 3,
                }],
                4096,
            ),
            "depth must be 1–2",
        ),
        (
            "ignored file is not source",
            Reply::Request(vec![range("src/cache/debug.log", 1, 5)], 4096),
            "ITEM_UNRESOLVABLE",
        ),
        (
            "binary file",
            Reply::Request(vec![range("src/cache/blob.bin", 1, 2)], 4096),
            "ITEM_UNRESOLVABLE",
        ),
        (
            "range past end of file",
            Reply::Request(vec![range("src/cache/store.rs", 4000, 4002)], 4096),
            "ITEM_UNRESOLVABLE",
        ),
        (
            "memory outside the envelope",
            Reply::Request(
                vec![ContextRequestItem::Memory {
                    memory_id: "memory:0123456789abcdef".into(),
                }],
                4096,
            ),
            "ITEM_UNRESOLVABLE",
        ),
    ];
    for (name, reply, expected) in cases {
        let f = Fixture::new();
        let plan = f.plan("src/cache/api.rs");
        let (outcome, inputs) = f.run(&plan, script(vec![reply, Reply::Edit], vec![]));
        let message = outcome.unwrap_err().to_string();
        assert!(
            message.contains(expected),
            "{name}: expected {expected}, got {message}"
        );
        assert_eq!(executors(&inputs.lock().unwrap()).len(), 1, "{name}");
        let run = f
            .store()
            .runtime_status(&f.root, &plan.packet.plan_id)
            .unwrap()
            .unwrap();
        assert_eq!(run.state, RunState::Blocked, "{name}");
        assert!(run.reason.is_some(), "{name}");
        // Nothing was granted, whatever the failure was.
        let ledger = f.ledger(&plan, "executor:task:cache");
        assert_eq!(ledger.rounds_used(), 0, "{name}");
        assert!(
            ledger.deltas().next().is_none() && ledger.granted_bytes == 0,
            "{name}"
        );
    }
}

/// Part K: a provider that crashes in a re-issued round leaves the granted
/// delta persisted and the task blocked, never half-accepted.
#[test]
fn a_provider_crash_in_a_re_issued_round_leaves_the_grant_persisted() {
    let f = Fixture::new();
    let plan = f.plan("src/cache/api.rs");
    let (outcome, inputs) = f.run(
        &plan,
        script(vec![Reply::RequestCallee, Reply::Crash], vec![]),
    );
    assert!(outcome.is_err());
    assert_eq!(executors(&inputs.lock().unwrap()).len(), 2);
    let ledger = f.ledger(&plan, "executor:task:cache");
    assert_eq!(ledger.state, context::LedgerState::Open);
    assert_eq!(ledger.rounds_used(), 1);
    let jobs = f.store().runtime_jobs(&f.root, None).unwrap();
    let crashed = jobs
        .iter()
        .filter(|j| j.role == AgentRole::Executor)
        .find(|j| j.context_request.is_none())
        .unwrap();
    assert_eq!(crashed.state, RuntimeJobState::Failed);
    let requester = jobs
        .iter()
        .filter(|j| j.role == AgentRole::Executor)
        .find(|j| j.context_request.is_some())
        .unwrap();
    assert_eq!(requester.state, RuntimeJobState::Succeeded);
    let tasks = f
        .store()
        .tasks(
            &RepositoryInfo::discover(&f.root).unwrap().repository_id,
            Some(&plan.packet.plan_id),
        )
        .unwrap();
    assert_eq!(tasks[0].state, TaskState::Blocked);
}

/// Part 12/D: issued source is hash-bound. The definition of a symbol in the
/// file the executor just changed cannot be reissued: its ontology row no
/// longer matches the captured source, so the relay fails closed instead of
/// handing a verifier text that was never in the diff. (Identity facts from
/// files the change did not touch still resolve; see the independence test.)
#[test]
fn a_definition_from_the_just_changed_file_fails_closed_as_stale() {
    let f = Fixture::new();
    let plan = f.plan("src/cache/api.rs");
    let changed = vec![ContextRequestItem::SymbolDefinition {
        entity_id: plan.packet.tasks[0].graph_entities[0].clone(),
    }];
    let (outcome, _) = f.run(
        &plan,
        script(vec![Reply::Edit], vec![Reply::Request(changed, 8192)]),
    );
    let message = outcome.unwrap_err().to_string();
    assert!(
        message.contains("VERIFIER_CONTEXT_REQUEST_DENIED")
            && message.contains("Stale")
            && message.contains("no longer matches captured source"),
        "{message}"
    );
    let ledger = f.ledger(&plan, "verifier:task:cache");
    assert_eq!(ledger.state, context::LedgerState::Denied);
    assert_eq!(ledger.rounds_used(), 0);
    assert_eq!(ledger.granted_bytes, 0);
}

/// Part N: the relay is inspectable and decidable from the CLI. Inspection is
/// read-only, and a decision is an operator document, never provider output.
#[test]
fn the_context_relay_is_inspectable_and_decidable_from_the_cli() {
    let f = Fixture::new();
    let plan = f.plan_with_scope(
        vec![ScopePath::File {
            path: "src/cache/api.rs".into(),
        }],
        "src/cache/api.rs",
    );
    let id = plan.packet.plan_id.as_str();
    let sibling = vec![ContextRequestItem::FileRange {
        path: "src/cache/store.rs".into(),
        start_line: 1,
        end_line: 10,
    }];
    let (outcome, _) = f.run(&plan, script(vec![Reply::Request(sibling, 8192)], vec![]));
    assert!(outcome.is_err());
    let shown = f.cli(&["run", "context", id, "--json"]);
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let report: Value = serde_json::from_slice(&shown.stdout).unwrap();
    let subject = &report["subjects"][0];
    assert_eq!(subject["state"], json!("NEEDS_PLANNER_CONTEXT_APPROVAL"));
    assert_eq!(subject["rounds_used"], json!(0));
    let pending = &subject["pending_decision"];
    assert_eq!(pending["outside_paths"], json!(["src/cache/store.rs"]));
    // The printed report explains the relay without copying source.
    let printed = String::from_utf8_lossy(&shown.stdout);
    assert!(
        !printed.contains("CANARY_STORE"),
        "the report copied source"
    );
    let mut decision: context::ContextDecision =
        serde_json::from_value(pending["decision_template"].clone()).unwrap();
    decision.reason = "The sibling module is the task's real subject".into();
    decision.actor = "operator".into();
    // The decision lives outside the workspace: writing it inside would be drift.
    let path = f._temp.0.join("decision.json");
    fs::write(&path, serde_json::to_vec_pretty(&decision).unwrap()).unwrap();
    let file = path.to_str().unwrap();
    let wrong = f.cli(&["run", "context", "decide", "plan:other", file]);
    assert!(!wrong.status.success(), "a decision for another plan");
    let applied = f.cli(&["run", "context", "decide", id, file, "--json"]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let ledger = f.ledger(&plan, "executor:task:cache");
    assert_eq!(ledger.state, context::LedgerState::Open);
    assert_eq!(ledger.rounds_used(), 1);
    // Resuming now issues a fresh job carrying the planner-approved delta.
    let (resumed, inputs) = f.run(&plan, script(vec![Reply::Edit], vec![]));
    assert_eq!(resumed.unwrap().state, RunState::Complete);
    let inputs = inputs.lock().unwrap();
    assert!(prompt(executors(&inputs)[0]).contains("CANARY_STORE"));
}

/// Part J: with `visibility = "issued"` a job is confined to the repository
/// files it was actually issued -- for an executor its planner-named write
/// target, for a verifier nothing at all, since a verifier receives diff hunks
/// rather than whole files. The default requests no confinement.
#[test]
fn issued_visibility_confines_each_job_to_what_it_was_issued() {
    let mut f = Fixture::new();
    f.config.context.visibility = ContextVisibility::Issued;
    let plan = f.plan("src/cache/api.rs");
    let (outcome, _) = f.run(
        &plan,
        script(vec![Reply::RequestCallee, Reply::Edit], vec![]),
    );
    assert_eq!(outcome.unwrap().state, RunState::Complete);
    let confinement = f.issued.lock().unwrap();
    assert!(confinement.len() >= 4, "{confinement:?}");
    // The runtime issues canonical paths; the fixture's own root may still be a
    // symlink (macOS `/var` -> `/private/var`).
    let root = fs::canonicalize(&f.root).unwrap();
    for (role, visibility) in confinement.iter() {
        let visibility = visibility
            .as_ref()
            .unwrap_or_else(|| panic!("{role:?} ran unconfined under issued visibility"));
        assert!(
            visibility
                .read_files
                .iter()
                .chain(&visibility.write_paths)
                .all(|p| p.starts_with(&root)),
            "{role:?} was confined to a path outside the workspace"
        );
        match role {
            AgentRole::Executor => {
                assert_eq!(visibility.read_files, [root.join("src/cache/api.rs")]);
                assert_eq!(visibility.write_paths, [root.join("src/cache/api.rs")]);
            }
            // A verifier is issued the change, not the files around it.
            AgentRole::Verifier => {
                assert!(visibility.read_files.is_empty());
                assert!(visibility.write_paths.is_empty());
            }
            AgentRole::Planner => unreachable!("the fixture imports plans directly"),
        }
    }
    drop(confinement);
    // The default asks for no confinement at all.
    let g = Fixture::new();
    let plan = g.plan("src/cache/api.rs");
    assert_eq!(g.config.context.visibility, ContextVisibility::Workspace);
    assert_eq!(
        g.run(&plan, script(vec![Reply::Edit], vec![]))
            .0
            .unwrap()
            .state,
        RunState::Complete
    );
    assert!(g.issued.lock().unwrap().iter().all(|(_, v)| v.is_none()));
}

/// Runtime phases in the append-only journal, in order.
fn journal_phases(f: &Fixture) -> Vec<String> {
    let connection = rusqlite::Connection::open(&f.paths.database).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT json_extract(entry_json,'$.phase') FROM events WHERE json_extract(entry_json,'$.kind')='RUNTIME' ORDER BY sequence",
        )
        .unwrap();
    statement
        .query_map([], |row| row.get::<_, Option<String>>(0))
        .unwrap()
        .map(|p| p.unwrap().unwrap_or_default())
        .collect()
}
