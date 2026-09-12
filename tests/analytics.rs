#[allow(dead_code)]
mod common;
use agentctl::{
    local::{
        analytics::*,
        config::ProjectConfig,
        repository::RepositoryInfo,
        store::{JournalEntry, Store},
    },
    protocol::*,
};
use rusqlite::params;
use serde_json::{Value, json};
use std::{fs, process::Command, time::Instant};

struct Fixture {
    temp: common::TempDir,
    info: RepositoryInfo,
    db: std::path::PathBuf,
}
fn persisted_route(source: &str, depth: u64) -> Value {
    json!({"requested_role":"executor", "primary":{"provider":"a","model":"m1","effort":null},
        "selected":{"provider":if depth==0 {"a"} else {"b"},"model":if depth==0 {"m2"} else {"mb"},"effort":null},
        "attempt":depth,"failures":if depth==0 {json!([])} else {json!([{"route":{"provider":"a","model":"m1"},"reason":"PROVIDER_UNAVAILABLE"}])},
        "sources":{"provider":"machine profiles","model":"explicit user","fallbacks":source,"arbitrary":"PRIVATE_CANARY"},
        "profile_hash":"hash","project_policy_hash":"hash","policy_skipped":[],"private":"PRIVATE_CANARY"})
}

#[test]
fn route_sources_filters_failure_history_and_cli_are_historical_allowlisted() {
    for source in ["project profiles", "explicit user"] {
        let f = Fixture::new();
        let c = f.sql();
        f.job(&c, "job:a", "session:r", "executor", "a", "m1", "FAILED");
        f.job(&c, "job:b", "session:r", "executor", "b", "mb", "SUCCEEDED");
        c.execute("UPDATE runtime_jobs SET record_json=json_set(record_json,'$.route',json(?1)) WHERE job_id='job:b'",[persisted_route(source,1).to_string()]).unwrap();
        drop(c);
        let before = fs::read(&f.db).unwrap();
        for dimension in ["provider", "model"] {
            let mut q = f.query();
            if dimension == "provider" {
                q.provider = Some("b".into());
            } else {
                q.model = Some("mb".into());
            }
            let s = f.read(q.clone());
            assert_eq!(s.jobs.len(), 1);
            let r = s.jobs[0].route_provenance.as_ref().unwrap();
            assert_eq!(r.provider_source.as_deref(), Some(source));
            assert_eq!(r.model_source.as_deref(), Some(source));
            assert_eq!(r.fallback_source.as_deref(), Some(source));
            assert_eq!(
                r.configured_provider_source.as_deref(),
                Some("machine profiles")
            );
            assert_eq!(r.fallback_used, Some(true));
            assert_eq!(r.actual_provider.as_deref(), Some("b"));
            assert_eq!(
                s.summary.preceding_failure_occurrences["PROVIDER_UNAVAILABLE"],
                1
            );
            assert!(s.summary.availability_failures.is_empty());
            let original = serde_json::to_value(&s).unwrap();
            fs::write(
                agentctl::local::paths::project_config(&f.temp.0.join("repo")),
                "changed current project routing",
            )
            .unwrap();
            fs::create_dir_all(f.temp.0.join("config/agentctl")).unwrap();
            fs::write(
                f.temp.0.join("config/agentctl/config.toml"),
                "changed current machine routing",
            )
            .unwrap();
            assert_eq!(original, serde_json::to_value(f.read(q)).unwrap());
            assert!(!original.to_string().contains("CANARY"));
        }
        assert_eq!(before, fs::read(&f.db).unwrap());
        fs::create_dir_all(f.temp.0.join("data/agentctl")).unwrap();
        fs::copy(&f.db, f.temp.0.join("data/agentctl/state.sqlite3")).unwrap();
        for json_mode in [false, true] {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentctl"));
            cmd.current_dir(f.temp.0.join("repo"))
                .env("XDG_DATA_HOME", f.temp.0.join("data"))
                .env("XDG_CONFIG_HOME", f.temp.0.join("config"))
                .args(["analytics", "routes", "--provider", "b", "--from-ms", "0"]);
            if json_mode {
                cmd.arg("--json");
            }
            let result = cmd.output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let text = String::from_utf8(result.stdout).unwrap();
            assert!(text.contains(source));
            assert!(!text.contains("CANARY"));
            assert!(text.contains(if json_mode {
                "PROVIDER_UNAVAILABLE"
            } else {
                "ProviderUnavailable"
            }));
        }
    }
}

#[test]
fn primary_mixed_model_source_policy_promotion_and_ordered_multi_fallback() {
    let f = Fixture::new();
    let c = f.sql();
    f.job(&c, "job:r", "session:r", "executor", "a", "m2", "SUCCEEDED");
    let mut route = persisted_route("project profiles", 0);
    // Model-only override changes the configured primary too in Stage 7.
    route["primary"] = route["selected"].clone();
    let set = |r: &Value| {
        c.execute("UPDATE runtime_jobs SET record_json=json_set(record_json,'$.route',json(?1)) WHERE job_id='job:r'",[r.to_string()]).unwrap();
    };
    set(&route);
    let s = f.read(f.query());
    let r = s.jobs[0].route_provenance.as_ref().unwrap();
    assert_eq!(r.provider_source.as_deref(), Some("machine profiles"));
    assert_eq!(r.model_source.as_deref(), Some("explicit user"));
    assert_eq!(r.explicit_override_fields, vec!["model"]);
    assert_eq!(r.fallback_used, Some(false));
    assert!(r.preceding_failures.is_empty());
    route["policy_skipped"] = json!([route["primary"].clone()]);
    route["selected"] = json!({"provider":"b","model":"mb","effort":null});
    set(&route);
    let s = f.read(f.query());
    let r = s.jobs[0].route_provenance.as_ref().unwrap();
    assert_eq!(r.provider_source.as_deref(), Some("project profiles"));
    assert_eq!(r.policy_skipped.len(), 1);
    assert_eq!(r.fallback_used, Some(false));
    assert!(s.summary.preceding_failure_occurrences.is_empty());
    assert!(s.summary.availability_failures.is_empty());
    route["attempt"] = json!(2);
    route["selected"] = json!({"provider":"c","model":"mc"});
    route["failures"] = json!([
        {"route":{"provider":"a","model":"ma"},"reason":"PROVIDER_UNAVAILABLE"},
        {"route":{"provider":"b","model":"mb"},"reason":"AUTH_UNAVAILABLE"}]);
    set(&route);
    let s = f.read(f.query());
    let r = s.jobs[0].route_provenance.as_ref().unwrap();
    assert_eq!(r.actual_provider.as_deref(), Some("c"));
    assert_eq!(r.fallback_depth, Some(2));
    assert_eq!(
        serde_json::to_value(&r.preceding_failures).unwrap()[1]["reason"],
        "AUTH_UNAVAILABLE"
    );
    assert_eq!(s.summary.preceding_failure_occurrences.len(), 2);
}
impl Fixture {
    fn new() -> Self {
        let temp = common::TempDir::new();
        let root = temp.0.join("repo");
        fs::create_dir(&root).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        ProjectConfig::initialize(&root).unwrap();
        let info = RepositoryInfo::discover(&root).unwrap();
        let db = temp.0.join("state.db");
        Store::open(&db, 5000)
            .unwrap()
            .register_repository(info.clone())
            .unwrap();
        Self { temp, info, db }
    }
    fn sql(&self) -> rusqlite::Connection {
        let c = common::sql(&self.db);
        for n in [2, 3] {
            c.create_scalar_function(
                "agentctl_runtime_authorized",
                n,
                rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS,
                |_| Ok(true),
            )
            .unwrap();
        }
        c
    }
    fn query(&self) -> Query {
        let mut q = Query::workspace(
            self.info.repository_id.as_str().into(),
            self.info.workspace_id.as_str().into(),
            10000000,
        );
        q.from_ms = 0;
        q
    }
    fn read(&self, q: Query) -> Snapshot {
        Store::read_only(&self.db, 100)
            .unwrap()
            .analytics(q, 10000000)
            .unwrap()
    }
    #[allow(clippy::too_many_arguments)]
    fn job(
        &self,
        c: &rusqlite::Connection,
        id: &str,
        session: &str,
        role: &str,
        provider: &str,
        model: &str,
        state: &str,
    ) {
        c.execute("INSERT OR IGNORE INTO planning_requests(request_id,repo_id,workspace_id,packet_json) VALUES('request:fixture',?1,?2,'{}')",params![self.info.repository_id.as_str(),self.info.workspace_id.as_str()]).unwrap();
        c.execute("INSERT OR IGNORE INTO plans(repo_id,plan_id,packet_json) VALUES(?1,'plan:synthetic','{}')",[self.info.repository_id.as_str()]).unwrap();
        c.execute("INSERT INTO jobs(repo_id,job_id,plan_id,packet_json,workspace_id) VALUES(?1,?2,'plan:synthetic','{}',?3)",params![self.info.repository_id.as_str(),id,self.info.workspace_id.as_str()]).unwrap();
        let record = json!({"job_id":id,"created_at_ms":1000,"started_at_ms":1100,"finished_at_ms":if state=="RUNNING"{Value::Null}else{json!(1200)},"state":state,"role":role.to_uppercase(),"ownership":{"engineering_session_id":session,"agent_instance_id":format!("agent:{id}"),"parent_agent_instance_id":"agent:root"},"config":{"provider":provider,"model":model},"route":{"requested_role":role,"attempt":0,"policy_skipped":[],"sources":{"provider":"machine profiles"}},"prompt":{"bytes":1000,"context_bytes":800,"instruction_bytes":200,"context_budget":2000,"context_truncated":false},"failure":"PRIVATE_REASONING_CANARY","stdout":{"path":"CREDENTIAL_CANARY"}});
        c.execute("INSERT INTO runtime_jobs(job_id,repo_id,workspace_id,request_id,record_json) VALUES(?1,?2,?3,'request:fixture',?4)",params![id,self.info.repository_id.as_str(),self.info.workspace_id.as_str(),record.to_string()]).unwrap();
    }
    fn usage(
        &self,
        c: &rusqlite::Connection,
        id: &str,
        event_id: &str,
        p: TokenUsageProvenance,
        n: Option<u64>,
    ) {
        let context = EventContext {
            job_id: Some(JobId::new(id).unwrap()),
            ..common::context()
        };
        let mut context = context;
        context.plan_id = None;
        context.task_id = None;
        context.packet_id = None;
        context.agent_id = None;
        context.role = None;
        let usage = TokenUsageEvent {
            version: ProtocolVersion::V1,
            timestamp_ms: 1300,
            context: context.clone(),
            provenance: p,
            input_tokens: n,
            output_tokens: n.map(|_| 2),
            cached_tokens: n.map(|_| 3),
            reasoning_tokens: None,
            total_tokens: None,
        };
        let event = AgentEvent {
            version: ProtocolVersion::V1,
            event_id: event_id.into(),
            timestamp_ms: 1300,
            context,
            event: AgentEventKind::TokenUsageObserved { usage },
        };
        let entry = JournalEntry::Agent {
            event: Box::new(event),
        };
        c.execute("INSERT INTO events(repo_id,workspace_id,timestamp_ms,job_id,entry_json) VALUES(?1,?2,1300,?3,?4)",params![self.info.repository_id.as_str(),self.info.workspace_id.as_str(),id,serde_json::to_string(&entry).unwrap()]).unwrap();
    }
}

#[test]
fn provenance_matrix_unknown_is_not_zero_and_overflow_is_partial() {
    for (values, quality) in [
        (vec!["EXACT"], "EXACT"),
        (vec!["ESTIMATED"], "ESTIMATED"),
        (vec!["UNKNOWN"], "UNKNOWN"),
        (vec!["EXACT", "ESTIMATED"], "MIXED"),
        (vec!["EXACT", "UNKNOWN"], "PARTIAL"),
        (vec!["ESTIMATED", "UNKNOWN"], "PARTIAL"),
        (vec!["EXACT", "ESTIMATED", "UNKNOWN"], "PARTIAL"),
    ] {
        let mut a = Amount::default();
        for p in values {
            a.add(Some(10), p);
        }
        assert_eq!(a.quality(), quality);
    }
    let mut a = Amount::default();
    a.add(Some(0), "EXACT");
    assert_eq!(a.exact, Some(0));
    assert_eq!(a.quality(), "EXACT");
    a.add(Some(u64::MAX), "EXACT");
    a.add(Some(1), "EXACT");
    assert!(a.overflow);
    assert_eq!(a.quality(), "PARTIAL");
}
#[test]
fn rates_and_distributions_have_denominators_and_empty_nulls() {
    assert_eq!(Rate::new(0, 0).ratio, None);
    assert_eq!(Rate::new(1, 4).ratio, Some(0.25));
    let d = Distribution::of([Some(1), Some(4), None].into_iter());
    assert_eq!(d.median, Some(2.5));
    assert_eq!(d.unknown, 1);
    assert!(Distribution::of([None].into_iter()).median.is_none());
}
#[test]
fn actual_routes_custom_roles_mixed_tokens_and_privacy_survive_reopen() {
    let f = Fixture::new();
    let c = f.sql();
    f.job(
        &c,
        "job:a",
        "session:1",
        "executor",
        "a",
        "model:old",
        "SUCCEEDED",
    );
    f.usage(&c, "job:a", "u:a", TokenUsageProvenance::Exact, Some(10));
    f.job(
        &c,
        "job:b",
        "session:1",
        "recon-custom",
        "b",
        "future-model",
        "SUCCEEDED",
    );
    f.usage(
        &c,
        "job:b",
        "u:b",
        TokenUsageProvenance::Estimated,
        Some(20),
    );
    f.job(&c, "job:c", "session:1", "planner", "a", "old", "SUCCEEDED");
    drop(c);
    let s = f.read(f.query());
    assert_eq!(s.summary.jobs, 3);
    assert_eq!(s.summary.tokens.total.exact, Some(12));
    assert_eq!(s.summary.tokens.total.estimated, Some(22));
    assert_eq!(s.summary.unknown_jobs, 1);
    assert_eq!(s.summary.token_quality, "PARTIAL");
    assert_eq!(s.summary.tokens.cached_read.exact, Some(3));
    assert_eq!(s.summary.tokens.total.exact, Some(12));
    assert_eq!(s.summary.tokens.cache_write.unknown, 3);
    assert!(
        s.roles
            .iter()
            .any(|g| g.role.as_deref() == Some("recon-custom"))
    );
    assert!(
        s.routes
            .iter()
            .any(|g| g.model.as_deref() == Some("model:old"))
    );
    let text = serde_json::to_string(&s).unwrap();
    assert!(!text.contains("CANARY"));
    assert!(
        s.jobs
            .iter()
            .all(|j| j.cost.is_none() && j.active_execution_ms.is_none())
    );
    fs::write(
        f.temp.0.join("config.toml"),
        "new routing must not affect history",
    )
    .unwrap();
    assert_eq!(text, serde_json::to_string(&f.read(f.query())).unwrap());
}
#[test]
fn duplicate_usage_is_not_counted_twice_and_running_is_incomplete() {
    let f = Fixture::new();
    let c = f.sql();
    f.job(&c, "job:a", "session:1", "executor", "a", "m", "RUNNING");
    for _ in 0..2 {
        f.usage(
            &c,
            "job:a",
            "duplicate",
            TokenUsageProvenance::Exact,
            Some(10),
        );
    }
    let s = f.read(f.query());
    assert_eq!(s.summary.tokens.total.exact, Some(12));
    assert_eq!(s.jobs[0].usage_observations, 1);
    assert_eq!(s.summary.token_quality, "PARTIAL");
    assert!(s.jobs[0].completed_execution_ms.is_none());
    assert_eq!(s.jobs[0].lifecycle, "RUNNING");
}
#[test]
fn filters_empty_windows_and_readonly_queries_are_safe() {
    let f = Fixture::new();
    assert_eq!(f.read(f.query()).summary.token_quality, "NOT_APPLICABLE");
    let c = f.sql();
    f.job(&c, "job:a", "session:1", "executor", "a", "m", "SUCCEEDED");
    f.usage(&c, "job:a", "u:a", TokenUsageProvenance::Exact, Some(10));
    drop(c);
    let before = fs::read(&f.db).unwrap();
    let mut q = f.query();
    q.session = Some("session:other".into());
    assert_eq!(f.read(q).summary.jobs, 0);
    let mut q = f.query();
    q.from_ms = 2000;
    assert_eq!(f.read(q).summary.jobs, 0);
    assert_eq!(before, fs::read(&f.db).unwrap());
    let mut q = f.query();
    q.provider = Some("different".into());
    assert_eq!(f.read(q).summary.jobs, 0);
}
#[test]
fn legacy_missing_optional_metadata_is_unknown_not_fabricated() {
    let f = Fixture::new();
    let c = f.sql();
    f.job(
        &c,
        "job:a",
        "session:1",
        "verifier",
        "a",
        "m",
        "INTERRUPTED",
    );
    c.execute("UPDATE runtime_jobs SET record_json=json_remove(record_json,'$.route','$.prompt','$.ownership')",[]).unwrap();
    let s = f.read(f.query());
    assert_eq!(s.jobs[0].provider.as_deref(), Some("a"));
    assert!(s.jobs[0].session.is_none());
    assert!(s.jobs[0].prompt_bytes.is_none());
    assert!(s.jobs[0].route_attempt.is_none());
    assert_eq!(s.summary.decisions_unknown, 1);
    assert_eq!(s.summary.reject_rate.denominator, 0);
}
#[test]
fn synthetic_thousands_are_bounded_and_session_query_is_interactive() {
    let f = Fixture::new();
    let mut c = f.sql();
    let tx = c.transaction().unwrap();
    for i in 0..3000 {
        let id = format!("job:{i}");
        f.job(
            &tx,
            &id,
            &format!("session:{}", i / 10),
            "executor",
            "p",
            "m",
            "SUCCEEDED",
        );
        f.usage(
            &tx,
            &id,
            &format!("u:{i}"),
            TokenUsageProvenance::Exact,
            Some(10),
        );
    }
    tx.commit().unwrap();
    let start = Instant::now();
    let mut q = f.query();
    q.session = Some("session:123".into());
    let s = f.read(q);
    assert_eq!(s.summary.jobs, 10);
    assert_eq!(s.summary.tokens.total.exact, Some(120));
    assert!(start.elapsed().as_secs() < 5);
    eprintln!(
        "300 sessions / 3000 jobs: session query {:?}",
        start.elapsed()
    );
    let all = f.read(f.query());
    assert_eq!(all.summary.jobs, 3000);
    assert_eq!(all.summary.tokens.total.exact, Some(36000));
    let mut q = f.query();
    q.limit = 10;
    assert!(f.read(q).truncated);
}

#[test]
fn planner_fallback_observation_does_not_duplicate_canonical_event_and_policy_skips_are_separate() {
    let f = Fixture::new();
    let c = f.sql();
    f.job(
        &c,
        "job:planner",
        "session:p",
        "planner",
        "p",
        "m",
        "SUCCEEDED",
    );
    f.usage(
        &c,
        "job:planner",
        "u:p",
        TokenUsageProvenance::Exact,
        Some(10),
    );
    let entry: String = c
        .query_row(
            "SELECT entry_json FROM events WHERE job_id='job:planner'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let v: Value = serde_json::from_str(&entry).unwrap();
    let JournalEntry::Agent { event } = serde_json::from_value(v).unwrap() else {
        panic!()
    };
    let AgentEventKind::TokenUsageObserved { usage } = event.event else {
        panic!()
    };
    c.execute("UPDATE runtime_jobs SET record_json=json_set(record_json,'$.planner_usage',json(?1),'$.route.policy_skipped',json('[{},{}]')) WHERE job_id='job:planner'", [serde_json::to_string(&usage).unwrap()]).unwrap();
    let s = f.read(f.query());
    assert_eq!(s.summary.tokens.total.exact, Some(12));
    assert_eq!(s.jobs[0].usage_observations, 1);
    assert_eq!(s.summary.policy_skipped, 2);
    assert_eq!(s.summary.fallback_attempts, 0);
    assert!(s.summary.availability_failures.is_empty());
    assert_eq!(s.providers.len(), 1);
    assert_eq!(s.models.len(), 1);
    assert_eq!(s.role_providers.len(), 1);
}

#[test]
fn concurrent_uncommitted_writer_is_not_visible_and_scope_does_not_bleed() {
    let f = Fixture::new();
    let mut c = f.sql();
    f.job(
        &c,
        "job:existing",
        "session:1",
        "executor",
        "p",
        "m",
        "SUCCEEDED",
    );
    f.usage(
        &c,
        "job:existing",
        "u:existing",
        TokenUsageProvenance::Exact,
        Some(10),
    );
    let tx = c.transaction().unwrap();
    f.job(
        &tx,
        "job:pending",
        "session:1",
        "executor",
        "p",
        "m",
        "SUCCEEDED",
    );
    f.usage(
        &tx,
        "job:pending",
        "u:pending",
        TokenUsageProvenance::Exact,
        Some(20),
    );
    let s = f.read(f.query());
    assert_eq!(s.summary.jobs, 1);
    assert_eq!(s.summary.tokens.total.exact, Some(12));
    tx.commit().unwrap();
    assert_eq!(f.read(f.query()).summary.tokens.total.exact, Some(34));
    let mut q = f.query();
    q.workspace = Some("workspace:other".into());
    assert_eq!(f.read(q).summary.jobs, 0);
    let mut q = f.query();
    q.repository = "repo:other".into();
    assert_eq!(f.read(q).summary.jobs, 0);
}
