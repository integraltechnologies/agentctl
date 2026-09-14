#[allow(dead_code)]
mod common;
use agentctl::local::{
    agenttop::{self, Action, App},
    observe::{
        self,
        usage::{Scope, WINDOW_MS, series},
        *,
    },
    store::Store,
};
use agentctl::protocol::TokenUsageProvenance as P;

fn usage(id: &str, at: u64, n: Option<u64>, p: P) -> Usage {
    Usage {
        observation_id: id.into(),
        at_ms: at,
        session_id: Some("s".into()),
        agent_id: Some("a".into()),
        job_id: Some("j".into()),
        task_id: Some("task-a".into()),
        provider: Some("local".into()),
        model: None,
        role: Some("EXECUTOR".into()),
        input: n,
        output: None,
        total: n,
        provenance: p,
    }
}
fn snapshot(samples: Vec<Usage>) -> Snapshot {
    Snapshot {
        at_ms: 1_000_000,
        usage: samples,
        ..Default::default()
    }
}

#[test]
fn exact_deltas_rate_window_and_expiry_are_deterministic() {
    let s = snapshot(vec![
        usage("1", 990_000, Some(50), P::Exact),
        usage("2", 960_000, Some(70), P::Exact),
    ]);
    let graph = series(&s, Scope::Aggregate, None);
    assert_eq!(graph.points.len(), 61);
    assert_eq!(graph.points.last().unwrap().tokens_per_minute, Some(120));
    assert_eq!(graph.quality, "EXACT");
    let expired = Snapshot {
        at_ms: 2_000_000,
        ..s
    };
    let graph = series(&expired, Scope::Aggregate, None);
    assert!(graph.points.iter().all(|p| p.tokens_per_minute.is_none()));
    assert_eq!(graph.total_observed, None);
}
#[test]
fn unknown_and_no_data_never_become_zero_but_reported_zero_is_valid() {
    for s in [
        snapshot(vec![]),
        snapshot(vec![usage("unknown", 999_000, None, P::Unknown)]),
    ] {
        let g = series(&s, Scope::Aggregate, None);
        assert_eq!(g.points.last().unwrap().tokens_per_minute, None);
        assert_eq!(g.quality, "UNKNOWN");
        let text = agenttop::render_text(&App::new(s), 100, 30).unwrap();
        assert!(text.contains("UNKNOWN"));
        assert!(!text.contains("67%"));
    }
    let g = series(
        &snapshot(vec![usage("zero", 999_000, Some(0), P::Exact)]),
        Scope::Aggregate,
        None,
    );
    assert_eq!(g.points.last().unwrap().tokens_per_minute, Some(0));
}
#[test]
fn duplicate_out_of_order_restart_and_lower_delta_do_not_double_count() {
    let a = usage("job1:obs1", 995_000, Some(100), P::Exact);
    let mut b = usage("job2:obs1", 997_000, Some(5), P::Exact);
    b.job_id = Some("new-process".into());
    let s = snapshot(vec![b.clone(), a.clone(), a.clone()]);
    let g = series(&s, Scope::Aggregate, None);
    assert_eq!(g.total_observed, Some(105));
    let reopened: Snapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(&g).unwrap(),
        serde_json::to_value(series(&reopened, Scope::Aggregate, None)).unwrap()
    );
    let ordered = snapshot(vec![a, b]);
    assert_eq!(
        g.total_observed,
        series(&ordered, Scope::Aggregate, None).total_observed
    );
}
#[test]
fn mixed_provenance_and_dimensions_preserve_partial_unknowns() {
    let exact = usage("a", 999_000, Some(10), P::Exact);
    let mut estimate = usage("b", 999_000, Some(20), P::Estimated);
    estimate.provider = Some("future".into());
    estimate.task_id = Some("task-b".into());
    estimate.role = Some("RECON".into());
    estimate.session_id = Some("s2".into());
    let mut s = snapshot(vec![exact, estimate]);
    assert_eq!(series(&s, Scope::Aggregate, None).quality, "MIXED");
    for scope in [
        Scope::Provider("future".into()),
        Scope::Task("task-b".into()),
        Scope::Role("RECON".into()),
    ] {
        let g = series(&s, scope, None);
        assert_eq!(g.total_observed, Some(20));
        assert_eq!(g.quality, "ESTIMATED");
    }
    assert_eq!(
        series(&s, Scope::Aggregate, Some("s")).total_observed,
        Some(10)
    );
    s.usage.push(usage("unknown", 999_000, None, P::Unknown));
    let g = series(&s, Scope::Aggregate, None);
    assert_eq!(g.total_observed, Some(30));
    assert_eq!(g.quality, "PARTIAL");
}
#[test]
fn graph_window_boundaries_future_observations_and_overflow_fail_honestly() {
    let mut s = snapshot(vec![
        usage("old", 1_000_000 - WINDOW_MS, Some(1), P::Exact),
        usage("future", 1_000_001, Some(999), P::Exact),
    ]);
    assert_eq!(series(&s, Scope::Aggregate, None).total_observed, None);
    s.usage = vec![
        usage("max", 999_000, Some(u64::MAX), P::Exact),
        usage("over", 999_000, Some(1), P::Exact),
    ];
    assert_eq!(
        series(&s, Scope::Aggregate, None).quality,
        "UNKNOWN_OVERFLOW"
    );
    assert_eq!(series(&s, Scope::Aggregate, None).total_observed, None);
    s.at_ms = 1;
    assert_eq!(series(&s, Scope::Aggregate, None).points.len(), 1);
}
fn agent(id: &str, parent: Option<&str>, session: &str) -> Agent {
    Agent {
        requested_role: None,
        route_attempt: None,
        route_origin: None,
        fallback_reason: None,
        policy_skip_reason: None,
        id: id.into(),
        session_id: Some(session.into()),
        parent_id: parent.map(str::to_owned),
        repository_id: "repo".into(),
        workspace_id: "ws".into(),
        root: "/workspace".into(),
        role: "RECON".into(),
        provider: None,
        model: None,
        plan_id: None,
        task_id: None,
        job_id: format!("job:{id}"),
        state: "RUNNING".into(),
        liveness: Liveness::Unknown,
        activity: "PROVIDER_EXECUTION".into(),
        verification: None,
        blocker: None,
        created_at_ms: Some(1),
        started_at_ms: Some(2),
        finished_at_ms: None,
        last_event: None,
        ownership_uncertain: false,
    }
}
#[test]
fn tree_orders_parent_children_and_isolates_sessions_with_orphans_and_cycles() {
    let s = Snapshot {
        agents: vec![
            agent("p", None, "s"),
            agent("e", Some("p"), "s"),
            agent("v", Some("e"), "s"),
            agent("i", Some("p"), "s"),
            agent("orphan", Some("missing"), "s"),
            agent("other", Some("p"), "s2"),
            agent("cycle1", Some("cycle2"), "s"),
            agent("cycle2", Some("cycle1"), "s"),
        ],
        ..Default::default()
    };
    let tree = observe::tree(&s, Some("s"));
    assert_eq!(&tree[..4], &[(0, 0), (1, 1), (2, 2), (3, 1)]);
    assert_eq!(tree.len(), 7);
    assert!(!tree.iter().any(|(i, _)| *i == 5));
    assert_eq!(tree, observe::tree(&s, Some("s")));
    assert_eq!(observe::tree(&s, Some("s2")), vec![(5, 0)]);
}
#[test]
fn keyboard_selection_empty_states_and_terminal_sizes_do_not_panic() {
    let mut app = App::new(snapshot(vec![usage("a", 999_000, Some(2), P::Exact)]));
    for action in [
        Action::Down,
        Action::Up,
        Action::Panel,
        Action::Inspect,
        Action::Session,
        Action::Graph,
        Action::Help,
        Action::Help,
    ] {
        app.action(action);
    }
    for (w, h) in [(120, 40), (80, 24), (35, 12), (40, 8), (1, 1), (10, 4)] {
        let text = agenttop::render_text(&app, w, h).unwrap();
        assert!(!text.is_empty());
        if w < 35 || h < 12 {
            assert!(text.starts_with("a"));
        }
    }
    assert!(agenttop::render_text(&app, 0, 0).is_err());
    app.replace(Snapshot::default());
    assert_eq!(app.selected, 0);
}
#[test]
fn empty_database_snapshot_is_read_only_and_tui_shares_projection() {
    let temp = common::TempDir::new();
    let db = temp.0.join("state.sqlite3");
    let store = Store::open(&db, 100).unwrap();
    let before = serde_json::to_value(store.status().unwrap()).unwrap();
    let snapshot = store.observe(123).unwrap();
    assert_eq!(snapshot.at_ms, 123);
    assert!(snapshot.sessions.is_empty());
    assert!(snapshot.agents.is_empty());
    assert_eq!(
        before,
        serde_json::to_value(store.status().unwrap()).unwrap()
    );
    let a = agenttop::render_text(&App::new(snapshot.clone()), 100, 30).unwrap();
    let decoded = serde_json::from_value(serde_json::to_value(snapshot).unwrap()).unwrap();
    assert_eq!(
        a,
        agenttop::render_text(&App::new(decoded), 100, 30).unwrap()
    );
}
#[test]
fn labels_do_not_emit_terminal_controls_or_recognizable_credentials() {
    let clean = label("name\u{1b}[31m sk-secret eyJencoded access_token=secret\nrow");
    assert!(!clean.contains('\u{1b}'));
    assert!(!clean.contains("secret"));
    assert!(clean.contains("[REDACTED]"));
    assert!(label(&"x".repeat(999)).len() <= 160);
}

#[test]
fn absent_database_queries_and_agenttop_exit_do_not_initialize_state() {
    let temp = common::TempDir::new();
    for (binary, args) in [
        (
            env!("CARGO_BIN_EXE_agentctl"),
            vec!["observe", "snapshot", "--json"],
        ),
        (env!("CARGO_BIN_EXE_agenttop"), vec!["--once"]),
    ] {
        let output = std::process::Command::new(binary)
            .args(args)
            .env("HOME", &temp.0)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CACHE_HOME")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(std::fs::read_dir(&temp.0).unwrap().count(), 0);
}

#[test]
fn busy_database_fails_boundedly_and_stale_tui_remains_renderable() {
    let temp = common::TempDir::new();
    let db = temp.0.join("busy.db");
    drop(Store::open(&db, 100).unwrap());
    let lock = rusqlite::Connection::open(&db).unwrap();
    lock.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE")
        .unwrap();
    let started = std::time::Instant::now();
    assert!(Store::read_only(&db, 50).is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    let app = App {
        error: Some("Database busy — stale snapshot".into()),
        ..Default::default()
    };
    assert!(
        agenttop::render_text(&app, 100, 30)
            .unwrap()
            .contains("Database busy")
    );
    lock.execute_batch("ROLLBACK").unwrap();
    assert!(Store::read_only(&db, 100).unwrap().observe(1).is_ok());
}

#[test]
fn navigation_preserves_selected_agent_and_scope_across_refresh() {
    let session:Session=serde_json::from_value(serde_json::json!({"id":"s","repository_id":"repo","workspace_id":"ws","root":"/workspace","supervisor_id":null,"plans":[],"current_plan":null,"title":"work","state":"RUNNING","verified":0,"task_count":0,"progress_complete":true,"correction_round":null,"blocker":null,"activity":"PROVIDER_EXECUTION","check":null,"last_event":null})).unwrap();
    let mut s = snapshot(vec![usage("obs", 999_000, Some(5), P::Exact)]);
    s.agents = vec![agent("one", None, "s"), agent("two", Some("one"), "s")];
    s.sessions = vec![session];
    let mut app = App::new(s.clone());
    app.action(Action::Down);
    app.action(Action::Graph);
    let selected = app.snapshot.agents[app.agents()[app.selected].0].id.clone();
    let scope = app.graph().scope;
    s.agents.reverse();
    app.replace(s);
    assert_eq!(
        selected,
        app.snapshot.agents[app.agents()[app.selected].0].id
    );
    assert_eq!(scope, app.graph().scope);
    app.action(Action::Inspect);
    let rendered = agenttop::render_text(&app, 100, 40).unwrap();
    assert!(rendered.contains("PROVIDER_EXECUTION"));
    assert!(!rendered.contains('%'));
}

#[test]
fn live_jobs_without_reports_make_known_usage_partial() {
    let mut s = snapshot(vec![usage("obs", 999_000, Some(5), P::Exact)]);
    s.agents = vec![agent("unreported", None, "s")];
    let result = series(&s, Scope::Aggregate, None);
    assert_eq!(result.total_observed, Some(5));
    assert_eq!(result.quality, "PARTIAL");
    assert_eq!(result.unreported_jobs, 1);
}

#[test]
fn legacy_snapshot_missing_liveness_defaults_unknown_and_remains_visible() {
    let mut value = serde_json::to_value(agent("legacy", None, "s")).unwrap();
    value.as_object_mut().unwrap().remove("liveness");
    let decoded: Agent = serde_json::from_value(value).unwrap();
    assert_eq!(decoded.state, "RUNNING");
    assert_eq!(decoded.liveness, Liveness::Unknown);
    assert_eq!(
        serde_json::to_value(&decoded).unwrap()["liveness"],
        "UNKNOWN"
    );
    let mut decoded = decoded;
    decoded.session_id = None;
    let mut app = App::new(Snapshot {
        agents: vec![decoded],
        ..Default::default()
    });
    assert!(
        agenttop::render_text(&app, 100, 30)
            .unwrap()
            .contains("RUNNING/UNKNOWN")
    );
    app.inspect = true;
    assert!(
        agenttop::render_text(&app, 100, 30)
            .unwrap()
            .contains("Liveness UNKNOWN")
    );
}

#[test]
fn agenttop_neutralizes_hostile_dynamic_fields_without_mutating_snapshot() {
    const ESC: &str = "\x1b[31mRED\x1b[0m";
    const NEWLINES: &str = "line1\nline2\rline3";
    const CONTROLS: &str = "a\tb\u{7}c";
    const BIDI: &str = "safe\u{202e}evil";
    const UNICODE: &str = "café 日本語 ok";

    let session = Session {
        id: "s".into(),
        ownership_known: true,
        repository_id: "repo".into(),
        workspace_id: "ws".into(),
        root: "/workspace".into(),
        supervisor_id: None,
        plans: vec![],
        current_plan: None,
        title: ESC.into(),
        state: "RUNNING".into(),
        verified: 0,
        task_count: 1,
        progress_complete: false,
        correction_round: None,
        blocker: Some(Blocker {
            kind: "hostile".into(),
            description: BIDI.into(),
            dependencies: vec![],
        }),
        activity: NEWLINES.into(),
        check: Some(CONTROLS.into()),
        last_event: None,
    };
    let mut a = agent("hostile", None, "s");
    a.role = UNICODE.into();
    a.provider = Some(ESC.into());

    let task = Task {
        id: "t1".into(),
        repository_id: "repo".into(),
        workspace_id: "ws".into(),
        session_id: Some("s".into()),
        plan_id: "p1".into(),
        objective: BIDI.into(),
        dependencies: vec![],
        lifecycle: "ACTIVE".into(),
        presentation: NEWLINES.into(),
        blocker: None,
        executor_job: None,
        verifier_job: None,
        attempts_in_view: 0,
        last_event: None,
    };

    let mut snapshot = Snapshot {
        sessions: vec![session],
        agents: vec![a],
        tasks: vec![task],
        ..Default::default()
    };
    snapshot.warnings = vec![CONTROLS.into()];
    let baseline_json = serde_json::to_string(&snapshot).unwrap();

    let mut app = App::new(snapshot);
    let agents_view = agenttop::render_text(&app, 120, 40).unwrap();

    // 1. ANSI ESC sequence is neutralized but the visible payload stays legible.
    assert!(!agents_view.contains('\u{1b}'));
    assert!(agents_view.contains("\\u{1b}"));
    assert!(agents_view.contains("RED"));

    // 2. newline/carriage return cannot forge additional rendered lines: all
    // three fragments stay on the single row the hostile value occupies.
    let activity_row = agents_view
        .lines()
        .find(|l| l.contains("line1"))
        .expect("session activity row rendered");
    assert!(activity_row.contains("line2") && activity_row.contains("line3"));
    assert!(agents_view.contains("\\u{a}"));
    assert!(agents_view.contains("\\u{d}"));

    // 3. tab/BEL control characters are escaped, not raw.
    assert!(!agents_view.contains('\u{7}'));
    assert!(agents_view.contains("\\u{7}"));
    assert!(agents_view.contains("\\u{9}"));

    // 4. bidi override character is neutralized.
    assert!(!agents_view.contains('\u{202e}'));
    assert!(agents_view.contains("\\u{202e}"));

    // 5. plain ASCII/Unicode text remains legible and untouched (CJK
    // characters land in separate terminal cells because they are
    // double-width, so check each grapheme rather than the substring).
    assert!(agents_view.contains("café"));
    for c in ['日', '本', '語'] {
        assert!(agents_view.contains(c), "missing {c:?} in {agents_view:?}");
    }
    assert!(agents_view.contains(" ok "));

    // Same checks against the task-focused panel (task objective/presentation).
    app.action(Action::Panel);
    let tasks_view = agenttop::render_text(&app, 120, 40).unwrap();
    assert!(!tasks_view.contains('\u{202e}'));
    assert!(tasks_view.contains("\\u{202e}"));
    let presentation_row = tasks_view
        .lines()
        .find(|l| l.contains("line1"))
        .expect("task presentation row rendered");
    assert!(presentation_row.contains("line2") && presentation_row.contains("line3"));

    // 6. sanitization is presentation-only: canonical snapshot/model data (and
    // its JSON serialization) is byte-for-byte unchanged after rendering.
    assert_eq!(app.snapshot.sessions[0].title, ESC);
    assert_eq!(
        app.snapshot.sessions[0]
            .blocker
            .as_ref()
            .unwrap()
            .description,
        BIDI
    );
    assert_eq!(app.snapshot.agents[0].role, UNICODE);
    assert_eq!(app.snapshot.tasks[0].objective, BIDI);
    assert_eq!(serde_json::to_string(&app.snapshot).unwrap(), baseline_json);
}
