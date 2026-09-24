//! Semantic impact analysis. Every oracle here is the edit the test
//! makes and the declarations it wrote, never the analyzer's own bookkeeping.
//! The suite attacks the invariant that no impact claim exists without an
//! evidence chain, and that uncertainty is represented instead of guessed.
#[allow(dead_code)]
mod common;
use agentctl::{
    local::{
        config::{ProjectConfig, VerificationDefinition},
        graph::{
            BoundaryReason, ImpactClass, ImpactEdge, ImpactLimits, ImpactOrigin, ImpactReport,
            ImpactRequest, RelationKind,
        },
        now_ms,
        planning::{
            ExecutionPlan, IntegrationVerificationContract, PlanMetadata, PlannerPacket,
            PlanningLimits, PlanningProvenance, RequestDraft, VerificationContract, VerifierInput,
            hash,
        },
        repository::RepositoryInfo,
        store::Store,
    },
    protocol::*,
};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// A layered workspace whose important dependents are deliberately in other
/// files: `top -> relay -> capture`, a private relay, and a test on `capture`.
const CORE: &str = "pub fn capture() -> u32 {\n    1\n}\n";
const RELAY: &str = "pub fn relay() -> u32 {\n    crate::core::capture()\n}\n";
const TOP: &str = "pub fn top() -> u32 {\n    crate::relay::relay()\n}\n";
const HIDDEN: &str = "fn hidden() -> u32 {\n    crate::core::capture()\n}\n\npub fn user() -> u32 {\n    hidden()\n}\n";
// In `src/` so that `crate::` resolves to the same crate root as the code it
// tests, and outside a macro: a macro body is an unparsed token
// tree, so a call inside one is not an observed relation.
const TEST: &str = "#[test]\nfn capture_is_one() {\n    let value = crate::core::capture();\n    assert!(value == 1);\n}\n";

fn layered() -> Vec<(&'static str, &'static str)> {
    vec![
        ("src/core.rs", CORE),
        ("src/relay.rs", RELAY),
        ("src/top.rs", TOP),
        ("src/hidden.rs", HIDDEN),
        ("src/capture_test.rs", TEST),
    ]
}

struct Fixture {
    temp: common::TempDir,
    root: PathBuf,
    db: PathBuf,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let temp = common::TempDir::new();
        let root = temp.0.join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        let mut policy = ProjectConfig::initialize(&root).unwrap();
        for id in ["unit", "integration"] {
            policy.commands.insert(
                id.into(),
                CommandSpec {
                    program: "cargo".into(),
                    args: vec!["test".into()],
                    cwd: ".".into(),
                },
            );
            policy.verification.insert(
                id.into(),
                VerificationDefinition {
                    description: format!("Required {id} tests"),
                    command_refs: vec![id.into()],
                },
            );
        }
        write(
            &root,
            ".agentctl/project.toml",
            &toml::to_string(&policy).unwrap(),
        );
        for (path, text) in files {
            write(&root, path, text);
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "baseline"]);
        let db = temp.0.join("data/agentctl/state.sqlite3");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        Store::open(&db, 5000)
            .unwrap()
            .register_repository(RepositoryInfo::discover(&root).unwrap())
            .unwrap();
        let f = Self { temp, root, db };
        f.index();
        f
    }
    fn store(&self) -> Store {
        Store::open(&self.db, 5000).unwrap()
    }
    fn index(&self) {
        self.store().index_repository(&self.root).unwrap();
    }
    fn write(&self, path: &str, text: &str) {
        write(&self.root, path, text);
    }
    fn candidate(&self) -> String {
        self.store()
            .ontology_status(&self.root)
            .unwrap()
            .candidate
            .expect("an open candidate")
            .generation_id
    }
    /// Apply edits (None deletes), reindex, and analyze the resulting delta.
    fn observe(&self, edits: &[(&str, Option<&str>)], limits: ImpactLimits) -> ImpactReport {
        for (path, text) in edits {
            match text {
                Some(text) => self.write(path, text),
                None => fs::remove_file(self.root.join(path)).unwrap(),
            }
        }
        self.index();
        self.store()
            .ontology_impact(
                &self.root,
                &ImpactRequest::Generation(self.candidate()),
                limits,
            )
            .unwrap()
    }
    /// Apply edits, reindex, and return the resulting generation's ID.
    fn candidate_after(&self, edits: &[(&str, Option<&str>)]) -> String {
        for (path, text) in edits {
            match text {
                Some(text) => self.write(path, text),
                None => fs::remove_file(self.root.join(path)).unwrap(),
            }
        }
        self.index();
        self.candidate()
    }
    fn symbol(&self, name: &str, limits: ImpactLimits) -> ImpactReport {
        self.store()
            .ontology_impact(
                &self.root,
                &ImpactRequest::Symbols(vec![name.into()]),
                limits,
            )
            .unwrap()
    }
    fn prepare(&self, scope: Vec<ScopePath>, limits: PlanningLimits) -> PlannerPacket {
        self.try_prepare(scope, limits).unwrap()
    }
    fn try_prepare(
        &self,
        scope: Vec<ScopePath>,
        limits: PlanningLimits,
    ) -> agentctl::local::Result<PlannerPacket> {
        self.store().prepare_plan(
            &self.root,
            RequestDraft {
                objective: "Change the capture result".into(),
                query: Some("capture".into()),
                scope,
                constraints: vec![],
                definition_of_done: vec!["capture returns the new value".into()],
                verification: None,
                invariant_refs: vec![],
                provenance: PlanningProvenance {
                    actor: "human".into(),
                    source_refs: vec!["objective".into()],
                    provider: None,
                },
            },
            limits,
        )
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agentctl"))
            .current_dir(&self.root)
            .args(args)
            .env("HOME", self.temp.0.join("home"))
            .env("XDG_CONFIG_HOME", self.temp.0.join("config"))
            .env("XDG_DATA_HOME", self.temp.0.join("data"))
            .env("XDG_CACHE_HOME", self.temp.0.join("cache"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap()
    }
    /// The CLI needs its own machine configuration; the fixture's store was
    /// created directly.
    fn cli_ready(&self) {
        self.cli(&["init"]);
    }
    fn cli_json(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn events(&self) -> usize {
        self.store().events(None, None, None, 1000).unwrap().len()
    }
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

fn paths(report: &ImpactReport, class: ImpactClass) -> BTreeSet<String> {
    report
        .items
        .iter()
        .filter(|i| i.class == class)
        .map(|i| i.path.clone())
        .collect()
}

fn names(report: &ImpactReport) -> BTreeSet<String> {
    report
        .items
        .iter()
        .map(|i| i.qualified_name.clone())
        .collect()
}

fn boundaries(report: &ImpactReport) -> Vec<(String, String)> {
    report
        .boundaries
        .iter()
        .map(|b| {
            (
                b.qualified_name.clone(),
                serde_json::to_value(&b.reason).unwrap()["reason"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            )
        })
        .collect()
}

fn has_boundary(report: &ImpactReport, name: &str, reason: &str) -> bool {
    boundaries(report)
        .iter()
        .any(|(n, r)| n.contains(name) && r == reason)
}

// ---------------------------------------------------------------------------
// Evidence and direct impact.
// ---------------------------------------------------------------------------

#[test]
fn a_direct_caller_in_another_file_is_reported_with_its_resolved_relation() {
    let f = Fixture::new(&layered());
    let report = f.symbol("capture", ImpactLimits::default());
    let direct = paths(&report, ImpactClass::DirectDependency);
    assert!(
        direct.contains("src/relay.rs") && direct.contains("src/hidden.rs"),
        "resolved cross-file callers are direct impact: {direct:?}"
    );
    assert!(
        !direct.contains("src/core.rs"),
        "the seed's own file is not its own dependent: {direct:?}"
    );
    let item = report
        .items
        .iter()
        .find(|i| i.path == "src/relay.rs")
        .unwrap();
    assert_eq!(item.distance, 1);
    assert_eq!(item.evidence.len(), 1);
    let step = &item.evidence[0];
    assert_eq!(step.from, report.seeds[0].entity);
    assert_eq!(step.entity, item.entity);
    match &step.edge {
        ImpactEdge::Relation { kind, rules, .. } => {
            assert_eq!(*kind, RelationKind::Calls);
            assert!(!rules.is_empty(), "the resolution rule is carried");
        }
        other => panic!("expected a resolved relation, got {other:?}"),
    }
    assert!(
        report.summary.cross_file >= 2,
        "file-local reasoning would have missed these: {:?}",
        report.summary
    );
}

#[test]
fn a_test_is_verification_relevance_however_it_is_associated() {
    // Reached by its own resolved call.
    let f = Fixture::new(&layered());
    let report = f.symbol("capture", ImpactLimits::default());
    let called = report
        .items
        .iter()
        .find(|i| i.path == "src/capture_test.rs")
        .expect("the test that calls capture");
    assert_eq!(called.class, ImpactClass::VerificationRelevance);
    assert!(matches!(
        called.evidence.last().unwrap().edge,
        ImpactEdge::Relation {
            kind: RelationKind::Calls,
            ..
        }
    ));
    // Reached by container test association when the call itself is inside a
    // macro and therefore not an observed relation.
    let f = Fixture::new(&[(
        "src/core.rs",
        "pub fn capture() -> u32 {\n    1\n}\n\n#[test]\nfn capture_is_one() {\n    assert_eq!(capture(), 1);\n}\n",
    )]);
    let report = f.symbol("capture", ImpactLimits::default());
    let associated = report
        .items
        .iter()
        .find(|i| i.class == ImpactClass::VerificationRelevance)
        .expect("the co-located test");
    match associated.evidence.last().unwrap().edge {
        ImpactEdge::TestAssociation { basis } => assert_eq!(
            basis,
            agentctl::local::graph::AssociationBasis::Container,
            "the basis is carried, so a container guess is never read as coverage"
        ),
        ref other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Change sensitivity and bounded transitive impact.
// ---------------------------------------------------------------------------

#[test]
fn a_signature_change_reaches_further_than_a_body_change_through_an_exported_relay() {
    let f = Fixture::new(&layered());
    let body = f.observe(
        &[("src/core.rs", Some("pub fn capture() -> u32 {\n    2\n}\n"))],
        ImpactLimits::default(),
    );
    assert_eq!(
        paths(&body, ImpactClass::ContractExposure),
        BTreeSet::new(),
        "a body edit proves nothing past its direct callers"
    );
    assert!(
        has_boundary(&body, "relay", "UNDETERMINED_PROPAGATION"),
        "the open question is stated, not answered: {:?}",
        boundaries(&body)
    );
    let f = Fixture::new(&layered());
    let signature = f.observe(
        &[(
            "src/core.rs",
            Some("pub fn capture(n: u32) -> u32 {\n    n\n}\n"),
        )],
        ImpactLimits::default(),
    );
    assert!(
        paths(&signature, ImpactClass::ContractExposure).contains("src/top.rs"),
        "a contract change propagates through an exported caller: {:?}",
        names(&signature)
    );
    let item = signature
        .items
        .iter()
        .find(|i| i.path == "src/top.rs")
        .unwrap();
    assert_eq!(item.distance, 2);
    assert_eq!(item.evidence.len(), 2);
    assert_eq!(item.evidence[0].entity, item.evidence[1].from);
}

#[test]
fn propagation_stops_at_a_declaration_whose_visibility_is_not_provably_exported() {
    let f = Fixture::new(&layered());
    let report = f.observe(
        &[(
            "src/core.rs",
            Some("pub fn capture(n: u32) -> u32 {\n    n\n}\n"),
        )],
        ImpactLimits::default(),
    );
    assert!(
        !names(&report).iter().any(|n| n.contains("user")),
        "a private relay does not carry the change to its own callers: {:?}",
        names(&report)
    );
    assert!(
        has_boundary(&report, "hidden", "UNDETERMINED_PROPAGATION"),
        "{:?}",
        boundaries(&report)
    );
}

#[test]
fn the_depth_bound_is_reported_as_a_boundary_rather_than_silently_truncating() {
    let f = Fixture::new(&layered());
    let shallow = f.symbol(
        "capture",
        ImpactLimits {
            depth: 1,
            ..ImpactLimits::default()
        },
    );
    assert_eq!(shallow.summary.max_distance, 1);
    assert!(
        has_boundary(&shallow, "relay", "DEPTH_LIMIT"),
        "{:?}",
        boundaries(&shallow)
    );
    let deep = f.symbol("capture", ImpactLimits::default());
    assert!(
        names(&deep).iter().any(|n| n.contains("top")),
        "depth 2 reaches the transitive caller: {:?}",
        names(&deep)
    );
}

#[test]
fn a_relation_cycle_terminates_and_reports_each_entity_once() {
    let f = Fixture::new(&[
        (
            "src/a.rs",
            "pub fn one() -> u32 {\n    crate::b::two()\n}\n",
        ),
        (
            "src/b.rs",
            "pub fn two() -> u32 {\n    crate::a::one()\n}\n",
        ),
    ]);
    let report = f.symbol(
        "one",
        ImpactLimits {
            depth: 4,
            ..ImpactLimits::default()
        },
    );
    let ids: Vec<_> = report.items.iter().map(|i| &i.entity).collect();
    let unique: BTreeSet<_> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len(), "no entity is reported twice");
    assert!(
        !report
            .items
            .iter()
            .any(|i| i.entity == report.seeds[0].entity),
        "the seed never becomes its own impact"
    );
    assert!(report.summary.max_distance <= 4);
}

// ---------------------------------------------------------------------------
// Abstention: unresolved, ambiguous and duplicate facts.
// ---------------------------------------------------------------------------

#[test]
fn an_unresolved_reference_by_name_is_a_boundary_and_never_an_impact_item() {
    let f = Fixture::new(&[
        ("src/core.rs", CORE),
        (
            "src/guess.rs",
            "pub fn guess() -> u32 {\n    capture()\n}\n",
        ),
    ]);
    let report = f.symbol("capture", ImpactLimits::default());
    assert!(
        !paths(&report, ImpactClass::DirectDependency).contains("src/guess.rs"),
        "a name match is not a dependency: {:?}",
        names(&report)
    );
    let reason = report
        .boundaries
        .iter()
        .find(|b| matches!(b.reason, BoundaryReason::UnresolvedReferences { .. }))
        .expect("the open question is recorded");
    match &reason.reason {
        BoundaryReason::UnresolvedReferences { count, paths } => {
            assert!(*count >= 1);
            assert!(paths.contains(&"src/guess.rs".to_string()));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_duplicate_ordinal_seed_is_never_traversed() {
    // Two declarations share a path, kind and qualified name, so an
    // ordinal-based ID proves nothing about which one changed.
    let duplicates = "#[cfg(unix)]\npub fn pick() -> u32 {\n    1\n}\n\n#[cfg(windows)]\npub fn pick() -> u32 {\n    2\n}\n";
    let f = Fixture::new(&[
        ("src/dup.rs", duplicates),
        (
            "src/user.rs",
            "pub fn user() -> u32 {\n    crate::dup::pick()\n}\n",
        ),
    ]);
    let edited = "#[cfg(unix)]\npub fn pick() -> u32 {\n    3\n}\n\n#[cfg(windows)]\npub fn pick() -> u32 {\n    2\n}\n";
    let report = f.observe(&[("src/dup.rs", Some(edited))], ImpactLimits::default());
    let unproven: Vec<_> = report
        .seeds
        .iter()
        .filter(|s| {
            matches!(
                s.origin,
                ImpactOrigin::EntityChange {
                    identity: agentctl::local::graph::IdentityBasis::DuplicateOrdinal,
                    ..
                }
            )
        })
        .collect();
    assert!(!unproven.is_empty(), "the edit creates unproven identity");
    assert!(
        unproven.iter().all(|s| s.skipped),
        "unproven identity is not traversed"
    );
    assert_eq!(
        report.summary.seeds_skipped,
        unproven.len(),
        "and it is counted"
    );
    assert!(
        has_boundary(&report, "pick", "UNPROVEN_IDENTITY"),
        "{:?}",
        boundaries(&report)
    );
}

#[test]
fn a_removed_entity_is_analyzed_from_the_relations_the_delta_recorded() {
    let f = Fixture::new(&layered());
    let report = f.observe(
        &[("src/core.rs", Some("pub fn other() {}\n"))],
        ImpactLimits::default(),
    );
    let relay = report
        .items
        .iter()
        .find(|i| i.path == "src/relay.rs")
        .expect("the caller of the removed function");
    assert_eq!(relay.class, ImpactClass::DirectDependency);
    match &relay.evidence[0].edge {
        ImpactEdge::Relation { removed, kind, .. } => {
            assert!(*removed, "the evidence is a relation that no longer exists");
            assert_eq!(*kind, RelationKind::Calls);
        }
        other => panic!("{other:?}"),
    }
    assert!(
        report
            .items
            .iter()
            .any(|i| i.class == ImpactClass::ContainmentOwnership && i.path == "src/core.rs"),
        "the container's composition changed: {:?}",
        names(&report)
    );
}

// ---------------------------------------------------------------------------
// Bounding and determinism.
// ---------------------------------------------------------------------------

#[test]
fn item_and_fanout_bounds_are_enforced_and_the_omission_is_counted() {
    let mut files = vec![("src/core.rs", CORE)];
    let bodies: Vec<String> = (0..12)
        .map(|i| format!("pub fn c{i}() -> u32 {{\n    crate::core::capture()\n}}\n"))
        .collect();
    let owned: Vec<(String, String)> = bodies
        .iter()
        .enumerate()
        .map(|(i, b)| (format!("src/c{i}.rs"), b.clone()))
        .collect();
    let mut refs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    files.append(&mut refs);
    let f = Fixture::new(&files);
    let bounded = f.symbol(
        "capture",
        ImpactLimits {
            items: 3,
            tests: 0,
            ..ImpactLimits::default()
        },
    );
    assert_eq!(bounded.items.len(), 3);
    assert!(bounded.summary.items_omitted > 0);
    let narrow = f.symbol(
        "capture",
        ImpactLimits {
            fanout: 2,
            ..ImpactLimits::default()
        },
    );
    assert!(
        narrow
            .boundaries
            .iter()
            .any(|b| matches!(b.reason, BoundaryReason::FanoutLimit { .. })),
        "{:?}",
        boundaries(&narrow)
    );
}

#[test]
fn identical_inputs_produce_byte_identical_reports_across_processes() {
    let f = Fixture::new(&layered());
    f.cli_ready();
    let a = f.cli_json(&["ontology", "impact", "--symbol", "capture", "--json"]);
    let b = f.cli_json(&["ontology", "impact", "--symbol", "capture", "--json"]);
    assert_eq!(a, b);
    assert_eq!(
        serde_json::to_string(&f.symbol("capture", ImpactLimits::default())).unwrap(),
        serde_json::to_string(&f.symbol("capture", ImpactLimits::default())).unwrap()
    );
}

// ---------------------------------------------------------------------------
// Generation binding and verifier independence.
// ---------------------------------------------------------------------------

#[test]
fn a_delta_that_is_not_the_indexed_generation_is_refused() {
    let f = Fixture::new(&layered());
    let first = f.observe(
        &[("src/core.rs", Some("pub fn capture() -> u32 {\n    2\n}\n"))],
        ImpactLimits::default(),
    );
    let stale = match &first.basis {
        agentctl::local::graph::ImpactBasis::ObservedDelta { to, .. } => to.clone(),
        other => panic!("{other:?}"),
    };
    // Move the workspace on; the earlier candidate is no longer materialized.
    f.write("src/core.rs", "pub fn capture() -> u32 {\n    3\n}\n");
    f.index();
    let error = f
        .store()
        .ontology_impact(
            &f.root,
            &ImpactRequest::Generation(stale),
            ImpactLimits::default(),
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("not the indexed generation"),
        "stale analysis fails closed: {error}"
    );
}

#[test]
fn a_delta_between_two_named_generations_is_analyzable_on_demand() {
    let f = Fixture::new(&layered());
    let first =
        f.candidate_after(&[("src/core.rs", Some("pub fn capture() -> u32 {\n    2\n}\n"))]);
    let second = f.candidate_after(&[(
        "src/core.rs",
        Some("pub fn capture(n: u32) -> u32 {\n    n\n}\n"),
    )]);
    let report = f
        .store()
        .ontology_impact(
            &f.root,
            &ImpactRequest::Diff {
                from: first,
                to: second.clone(),
            },
            ImpactLimits::default(),
        )
        .unwrap();
    match &report.basis {
        agentctl::local::graph::ImpactBasis::ObservedDelta { to, .. } => assert_eq!(to, &second),
        other => panic!("{other:?}"),
    }
    assert!(
        paths(&report, ImpactClass::ContractExposure).contains("src/top.rs"),
        "the signature change between the two named generations propagates: {:?}",
        names(&report)
    );
}

#[test]
fn analysis_is_read_only_and_changes_no_lifecycle_state() {
    let f = Fixture::new(&layered());
    f.write("src/core.rs", "pub fn capture() -> u32 {\n    2\n}\n");
    f.index();
    let before = f.events();
    let states: Vec<_> = f
        .store()
        .ontology_generations(&f.root, 100)
        .unwrap()
        .into_iter()
        .map(|g| (g.generation_id, g.state))
        .collect();
    let candidate = f.candidate();
    f.store()
        .ontology_impact(
            &f.root,
            &ImpactRequest::Generation(candidate),
            ImpactLimits::default(),
        )
        .unwrap();
    f.symbol("capture", ImpactLimits::default());
    assert_eq!(f.events(), before, "impact records no journal event");
    let after: Vec<_> = f
        .store()
        .ontology_generations(&f.root, 100)
        .unwrap()
        .into_iter()
        .map(|g| (g.generation_id, g.state))
        .collect();
    assert_eq!(states, after, "no generation is accepted or rejected");
}

// ---------------------------------------------------------------------------
// Planning and plan review integration.
// ---------------------------------------------------------------------------

#[test]
fn the_planner_packet_carries_bounded_impact_that_widens_no_authority() {
    let f = Fixture::new(&layered());
    let scope = vec![ScopePath::File {
        path: "src/core.rs".into(),
    }];
    let packet = f.prepare(scope.clone(), PlanningLimits::default());
    let impact = packet.context.impact.as_ref().expect("an impact outlook");
    assert_eq!(impact.scope, scope, "the request's own scope is echoed");
    assert_eq!(
        packet.request.intent.scope, scope,
        "impact never widens the request scope"
    );
    assert!(
        impact.outside_scope.contains(&"src/relay.rs".to_string()),
        "the planner learns its edit has consequences elsewhere: {:?}",
        impact.outside_scope
    );
    assert!(impact.outside_scope_items > 0);
    assert_eq!(
        impact.authority,
        agentctl::local::graph::ImpactAuthority::AdvisoryOnly
    );
    assert!(
        impact.report.items.len() <= 8 && impact.report.seeds.len() <= 4,
        "the brief is bounded, not the whole impact graph"
    );
    // Impact never becomes issued context: no source text, and no new
    // provenance-bound file in the packet's support set.
    let support: BTreeSet<_> = packet
        .request
        .source
        .support
        .iter()
        .map(|p| p.path.clone())
        .collect();
    let carried: BTreeSet<_> = packet
        .context
        .graph
        .primary
        .iter()
        .map(|p| p.entity.provenance.path.clone())
        .chain(
            packet
                .context
                .graph
                .neighbors
                .iter()
                .map(|e| e.provenance.path.clone()),
        )
        .chain(
            packet
                .context
                .graph
                .tests
                .iter()
                .map(|e| e.provenance.path.clone()),
        )
        .chain(
            packet
                .context
                .graph
                .relations
                .iter()
                .map(|e| e.provenance.path.clone()),
        )
        .chain(
            packet
                .context
                .excerpts
                .iter()
                .map(|e| e.provenance.path.clone()),
        )
        .collect();
    assert_eq!(
        support, carried,
        "support describes carried facts only, never impact-only files"
    );
    let json = serde_json::to_value(&packet).unwrap();
    let text = serde_json::to_string(&json["context"]["impact"]).unwrap();
    assert!(
        !text.contains("pub fn relay"),
        "the brief carries no source text"
    );
}

#[test]
fn impact_outlives_test_material_and_unresolved_summaries() {
    let f = Fixture::new(&layered());
    let generous = f.prepare(vec![], PlanningLimits::default());
    let outlook = generous.context.impact.as_ref().expect("an impact outlook");
    let brief = serde_json::to_vec(outlook).unwrap().len();
    assert!(brief > 64, "the brief is a real payload, not a stub");
    // Tighten the budget step by step: the impact outlook is relation
    // knowledge a planner cannot recover by reading files, so it is shed only
    // after test material and unresolved summaries, and before neighbors.
    let mut budget = generous.serialized_bytes - 1;
    let mut shed = false;
    let mut first = true;
    while budget >= 4096 {
        let Ok(tight) = f.try_prepare(
            vec![],
            PlanningLimits {
                bytes: budget,
                ..PlanningLimits::default()
            },
        ) else {
            break;
        };
        assert!(tight.serialized_bytes <= budget);
        if tight.context.impact.is_none() {
            shed = true;
            assert!(tight.context.graph.unresolved.is_empty(), "{budget}");
            assert!(tight.context.graph.tests.len() <= 1, "{budget}");
            if first {
                assert_eq!(
                    tight.context.graph.primary.len(),
                    generous.context.graph.primary.len(),
                    "primaries outlive the impact outlook"
                );
                assert_eq!(
                    tight.context.graph.neighbors.len(),
                    generous.context.graph.neighbors.len(),
                    "neighbors outlive the impact outlook"
                );
                first = false;
            }
        }
        budget -= 256;
    }
    assert!(shed, "a tight enough budget must shed the outlook");
}

#[test]
fn a_plan_review_names_observed_impact_the_plan_never_declared() {
    let f = Fixture::new(&layered());
    let prepared = f.prepare(
        vec![ScopePath::File {
            path: "src/core.rs".into(),
        }],
        PlanningLimits::default(),
    );
    let plan = one_task_plan(&prepared, "plan:impact", "src/core.rs");
    f.store()
        .import_execution_plan(&f.root, &plan)
        .expect("a valid one-task plan");
    f.write("src/core.rs", "pub fn capture(n: u32) -> u32 {\n    n\n}\n");
    f.index();
    let candidate = f.candidate();
    let outlook = f
        .store()
        .plan_impact(
            &f.root,
            &PlanId::new("plan:impact").unwrap(),
            &ImpactRequest::Generation(candidate),
            ImpactLimits::default(),
        )
        .unwrap();
    assert_eq!(
        outlook.scope,
        vec![ScopePath::File {
            path: "src/core.rs".into()
        }]
    );
    assert!(
        outlook.outside_scope.contains(&"src/relay.rs".to_string()),
        "the observed change crossed a boundary the plan never declared: {:?}",
        outlook.outside_scope
    );
    // The review decides nothing and grants nothing.
    assert_eq!(
        outlook.authority,
        agentctl::local::graph::ImpactAuthority::AdvisoryOnly
    );
    let after = f
        .store()
        .execution_plan(&f.root, &PlanId::new("plan:impact").unwrap())
        .unwrap();
    assert_eq!(
        after.plan.packet.tasks[0].write_scope, plan.packet.tasks[0].write_scope,
        "review never edits the plan's write scope"
    );
}

fn one_task_plan(prepared: &PlannerPacket, id: &str, write: &str) -> ExecutionPlan {
    let task = TaskPacket {
        version: ProtocolVersion::V1,
        task_id: TaskId::new(format!("{id}:0")).unwrap(),
        objective: "Change the capture result".into(),
        read_scope: vec![ScopePath::File { path: write.into() }],
        write_scope: vec![ScopePath::File { path: write.into() }],
        graph_entities: vec![prepared.context.graph.primary[0].entity.id.clone()],
        invariant_refs: prepared.request.intent.invariant_refs.clone(),
        dependencies: vec![],
        definition_of_done: vec!["capture returns the new value".into()],
        verification: VerificationRequirements {
            requirement_refs: vec!["unit".into()],
            evidence_required: true,
        },
    };
    let packet = PlanPacket {
        version: ProtocolVersion::V1,
        plan_id: PlanId::new(id).unwrap(),
        objective: prepared.request.intent.objective.clone(),
        tasks: vec![task],
        integration_verification: VerificationRequirements {
            requirement_refs: vec!["integration".into()],
            evidence_required: true,
        },
    };
    let contracts = packet
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
        .collect();
    ExecutionPlan {
        metadata: PlanMetadata {
            version: ProtocolVersion::V1,
            request_id: prepared.request.request_id.clone(),
            source: prepared.request.source.clone(),
            created_at_ms: now_ms().unwrap(),
            provenance: PlanningProvenance {
                actor: "external-planner".into(),
                source_refs: vec![prepared.request.request_id.as_str().into()],
                provider: None,
            },
            contracts,
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

// ---------------------------------------------------------------------------
// Dogfood: agentctl's own repository, through the real CLI.
// ---------------------------------------------------------------------------

#[test]
fn the_cli_reports_impact_with_evidence_and_refuses_an_unknown_symbol() {
    let f = Fixture::new(&layered());
    f.cli_ready();
    let report = f.cli_json(&["ontology", "impact", "--symbol", "capture", "--json"]);
    assert!(
        report["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| !i["evidence"].as_array().unwrap().is_empty()),
        "every reported item carries an evidence chain"
    );
    let human = f.cli(&["ontology", "impact", "--symbol", "capture"]);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("<-[Calls]-"), "{text}");
    let missing = f.cli(&["ontology", "impact", "--symbol", "nonexistent_symbol"]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("absent or ambiguous"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
}

// ---------------------------------------------------------------------------
// The context-authority boundary.
// ---------------------------------------------------------------------------

#[test]
fn impact_discovery_never_issues_context_outside_the_request_scope() {
    let f = Fixture::new(&layered());
    let scope = vec![ScopePath::File {
        path: "src/core.rs".into(),
    }];
    let packet = f.prepare(scope, PlanningLimits::default());
    let impact = packet.context.impact.as_ref().expect("an impact outlook");
    assert!(
        !impact.outside_scope.is_empty(),
        "the brief names files the request cannot touch"
    );
    let issued: BTreeSet<String> = packet
        .request
        .source
        .support
        .iter()
        .map(|p| p.path.clone())
        .collect();
    for path in &impact.outside_scope {
        assert!(
            !issued.contains(path),
            "impact discovery is not authorization: {path} became issued context"
        );
    }
    for entity in packet
        .context
        .graph
        .primary
        .iter()
        .map(|p| &p.entity)
        .chain(&packet.context.graph.neighbors)
        .chain(&packet.context.graph.tests)
    {
        assert_eq!(
            entity.provenance.path, "src/core.rs",
            "context selection still obeys the request's scope"
        );
    }
    assert!(
        packet
            .context
            .excerpts
            .iter()
            .all(|x| x.provenance.path == "src/core.rs"),
        "no source text is issued for an impacted file outside scope"
    );
}
