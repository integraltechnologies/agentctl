//! Issue #1: repository paths are literal. Names that framework routing
//! conventions use (`[slug]`, `[...slug]`, `[[...slug]]`, `(group)`, `@slot`) and
//! that pattern-oriented APIs (globs, Git pathspecs, SQL LIKE) would treat as
//! syntax must be discovered, indexed, queried, bound into planner context and
//! authorized exactly as the one path they name, and never broaden to others.
#[allow(dead_code)]
mod common;
use agentctl::{
    local::{
        config::{ProjectConfig, VerificationDefinition},
        graph::{ContextLimits, SearchMode},
        memory::{MemoryDraft, MemoryKind, MemoryLimits, MemoryLink},
        now_ms,
        planning::*,
        repository::RepositoryInfo,
        store::Store,
    },
    protocol::*,
};
use common::TempDir;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Literal names and the unique function each file declares.
const LITERAL: &[(&str, &str)] = &[
    ("app/[slug]/page.tsx", "slugPageHandler"),
    ("app/[...slug]/page.tsx", "catchAllPageHandler"),
    ("app/[[...slug]]/page.tsx", "optionalCatchAllPageHandler"),
    ("app/(customer)/s/[slug]/actions.ts", "customerIntakeAction"),
    ("app/@modal/(.)photo/[id]/page.tsx", "interceptedPhotoView"),
    ("src/routes/[[lang]]/+page.ts", "localizedRouteLoader"),
    ("lib/{brace}/100%_done/x.ts", "percentUnderscoreBrace"),
    ("lib/$param/it's here/x.ts", "dollarQuoteSpace"),
];
/// Valid on Unix hosts, impossible on Windows filesystems.
#[cfg(unix)]
const UNIX_LITERAL: &[(&str, &str)] = &[("lib/star*/q?/x.ts", "starQuestionLiteral")];
#[cfg(not(unix))]
const UNIX_LITERAL: &[(&str, &str)] = &[];
/// What a pattern reading of the literal names would also match: `[slug]` as a
/// character class matches `s`/`g`, `star*` matches `starX`, `100%_done` as a
/// LIKE pattern matches `100xxdone`.
const DECOYS: &[(&str, &str)] = &[
    ("app/s/page.tsx", "decoySingleLetterS"),
    ("app/g/page.tsx", "decoySingleLetterG"),
    ("app/slug/page.tsx", "decoyPlainSlug"),
    ("lib/starX/qZ/x.ts", "decoyStarExpansion"),
    ("lib/{brace}/100xxdone/x.ts", "decoyLikeExpansion"),
];

fn source(name: &str) -> String {
    format!("export function {name}(input: string): string {{ return input.trim(); }}\n")
}
fn write(root: &Path, path: &str, text: &str) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}
fn git(root: &Path, args: &[&str]) {
    let o = Command::new("git")
        .args([
            "-c",
            "user.name=Literal Test",
            "-c",
            "user.email=literal@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "init.templateDir=",
            "-C",
        ])
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

struct Fixture {
    temp: TempDir,
    root: PathBuf,
    db: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new();
        let root = temp.0.join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        for (path, name) in LITERAL.iter().chain(UNIX_LITERAL).chain(DECOYS) {
            write(&root, path, &source(name));
        }
        write(
            &root,
            "app/[slug]/page.test.ts",
            "test('slug page handler trims input', () => { slugPageHandler(' x '); });\n",
        );
        let mut policy = ProjectConfig::default();
        policy.commands.insert(
            "unit".into(),
            CommandSpec {
                program: "npm".into(),
                args: vec!["test".into()],
                cwd: ".".into(),
            },
        );
        policy.verification.insert(
            "unit".into(),
            VerificationDefinition {
                description: "Unit tests".into(),
                command_refs: vec!["unit".into()],
            },
        );
        write(
            &root,
            ".agentctl/project.toml",
            &toml::to_string(&policy).unwrap(),
        );
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "literal fixture"]);
        let db = temp.0.join("state.sqlite3");
        let mut store = Store::open(&db, 5000).unwrap();
        store
            .register_repository(RepositoryInfo::discover(&root).unwrap())
            .unwrap();
        Self { temp, root, db }
    }
    fn store(&self) -> Store {
        Store::open(&self.db, 5000).unwrap()
    }
    fn prepare(
        &self,
        objective: &str,
        scope: Vec<ScopePath>,
    ) -> agentctl::local::Result<PlannerPacket> {
        self.store().prepare_plan(
            &self.root,
            RequestDraft {
                objective: objective.into(),
                query: None,
                scope,
                constraints: vec![],
                definition_of_done: vec!["The route handler keeps its contract".into()],
                verification: Some(VerificationRequirements {
                    requirement_refs: vec!["unit".into()],
                    evidence_required: true,
                }),
                invariant_refs: vec![],
                provenance: PlanningProvenance {
                    actor: "literal-test".into(),
                    source_refs: vec!["issue-1".into()],
                    provider: None,
                },
            },
            PlanningLimits::default(),
        )
    }
}

fn all_literal() -> impl Iterator<Item = &'static (&'static str, &'static str)> {
    LITERAL.iter().chain(UNIX_LITERAL)
}

#[test]
fn literal_framework_paths_index_and_stay_individually_addressable() {
    let f = Fixture::new();
    let stats = f.store().index_repository(&f.root).unwrap();
    let files = LITERAL.len() + UNIX_LITERAL.len() + DECOYS.len() + 1;
    assert_eq!(
        (stats.discovered, stats.indexed, stats.failed),
        (files, files, 0)
    );
    assert!(f.store().index_status(&f.root).unwrap().fresh);
    for (path, name) in all_literal().chain(DECOYS) {
        let found = f
            .store()
            .graph(&f.root)
            .unwrap()
            .symbols(name, SearchMode::Exact, 10)
            .unwrap()
            .data;
        assert_eq!(found.len(), 1, "{name}");
        assert_eq!(found[0].provenance.path, *path);
        // Exact file lookup returns that file's facts only, never a decoy's.
        let in_file = f
            .store()
            .graph(&f.root)
            .unwrap()
            .entities_in_file(path, 100)
            .unwrap()
            .data;
        assert!(in_file.iter().any(|e| e.name == *name), "{path}");
        assert!(in_file.iter().all(|e| e.provenance.path == *path), "{path}");
        let hash = &found[0].provenance.content_hash;
        assert_eq!(
            hash,
            &agentctl::local::graph::content_hash(&fs::read(f.root.join(path)).unwrap())
        );
        // Ranked lookup by exact name ranks it first; by its words, it is found.
        let exact = f
            .store()
            .graph(&f.root)
            .unwrap()
            .locate(name, 3)
            .unwrap()
            .data;
        assert_eq!(exact[0].entity.provenance.path, *path);
        let words = name
            .chars()
            .flat_map(|c| {
                if c.is_uppercase() {
                    vec![' ', c]
                } else {
                    vec![c]
                }
            })
            .collect::<String>();
        let located = f
            .store()
            .graph(&f.root)
            .unwrap()
            .locate(&words, 5)
            .unwrap()
            .data;
        assert!(located.iter().any(|l| l.entity.name == *name), "{words}");
    }
    // SQL LIKE metacharacters in a substring search stay literal too.
    let like = f
        .store()
        .graph(&f.root)
        .unwrap()
        .symbols("lib::{brace}::100%_done", SearchMode::Substring, 10)
        .unwrap()
        .data;
    assert!(!like.is_empty());
    assert!(like.iter().all(|e| e.provenance.path.contains("100%_done")));
}

#[test]
fn literal_paths_participate_in_ranked_context_and_scoped_planning_without_broadening() {
    let f = Fixture::new();
    f.store().index_repository(&f.root).unwrap();
    let context = f
        .store()
        .graph(&f.root)
        .unwrap()
        .context("slug page handler trims input", ContextLimits::default())
        .unwrap();
    assert_eq!(context.primary[0].entity.name, "slugPageHandler");
    assert_eq!(
        context.primary[0].entity.provenance.path,
        "app/[slug]/page.tsx"
    );
    assert!(
        context
            .tests
            .iter()
            .any(|t| t.provenance.path == "app/[slug]/page.test.ts")
    );
    // Round-tripping the normalized packet preserves every literal path.
    let wire = serde_json::to_string(&context).unwrap();
    let back: agentctl::local::graph::ContextPacket = serde_json::from_str(&wire).unwrap();
    assert_eq!(serde_json::to_string(&back).unwrap(), wire);

    for (scope, path) in [
        (
            ScopePath::Directory {
                path: "app/[slug]".into(),
            },
            "app/[slug]/page.tsx",
        ),
        (
            ScopePath::File {
                path: "app/[...slug]/page.tsx".into(),
            },
            "app/[...slug]/page.tsx",
        ),
        (
            ScopePath::Directory {
                path: "app/(customer)/s/[slug]".into(),
            },
            "app/(customer)/s/[slug]/actions.ts",
        ),
    ] {
        let p = f
            .prepare("Make the page handler trim its input", vec![scope.clone()])
            .unwrap();
        let g = &p.context.graph;
        let paths: Vec<&str> = g
            .primary
            .iter()
            .map(|e| e.entity.provenance.path.as_str())
            .chain(g.neighbors.iter().map(|e| e.provenance.path.as_str()))
            .chain(g.tests.iter().map(|e| e.provenance.path.as_str()))
            .chain(
                p.context
                    .excerpts
                    .iter()
                    .map(|e| e.provenance.path.as_str()),
            )
            .chain(p.request.source.support.iter().map(|s| s.path.as_str()))
            .collect();
        assert!(!g.primary.is_empty(), "{scope:?}");
        assert!(paths.contains(&path), "{scope:?}");
        let root = scope.path();
        assert!(
            paths
                .iter()
                .all(|p| *p == root || p.starts_with(&format!("{root}/"))),
            "{scope:?} broadened to {paths:?}"
        );
        for e in &p.context.excerpts {
            let text = fs::read_to_string(f.root.join(&e.provenance.path)).unwrap();
            assert_eq!(&text[e.start_byte..e.end_byte], e.text);
        }
        let packet = serde_json::to_string(&p).unwrap();
        for (decoy, _) in DECOYS {
            assert!(!packet.contains(decoy), "{scope:?} leaked {decoy}");
        }
        let reopened = f
            .store()
            .planning_context(&f.root, &p.request.request_id)
            .unwrap();
        assert_eq!(serde_json::to_string(&reopened).unwrap(), packet);
    }
}

fn plan(
    prepared: &PlannerPacket,
    read: ScopePath,
    write: ScopePath,
    entity: GraphEntityId,
) -> ExecutionPlan {
    let task = TaskPacket {
        version: ProtocolVersion::V1,
        task_id: TaskId::new("literal:1").unwrap(),
        objective: "Trim the slug page handler input".into(),
        read_scope: vec![read],
        write_scope: vec![write],
        graph_entities: vec![entity],
        invariant_refs: vec![],
        dependencies: vec![],
        definition_of_done: vec!["Handler trims input".into()],
        verification: VerificationRequirements {
            requirement_refs: vec!["unit".into()],
            evidence_required: true,
        },
    };
    let packet = PlanPacket {
        version: ProtocolVersion::V1,
        plan_id: PlanId::new("plan:literal").unwrap(),
        objective: prepared.request.intent.objective.clone(),
        tasks: vec![task],
        integration_verification: VerificationRequirements {
            requirement_refs: vec!["unit".into()],
            evidence_required: true,
        },
    };
    ExecutionPlan {
        metadata: PlanMetadata {
            version: ProtocolVersion::V1,
            request_id: prepared.request.request_id.clone(),
            source: prepared.request.source.clone(),
            created_at_ms: now_ms().unwrap(),
            provenance: PlanningProvenance {
                actor: "literal-planner".into(),
                source_refs: vec![prepared.request.request_id.as_str().into()],
                provider: None,
            },
            contracts: vec![VerificationContract {
                task_id: packet.tasks[0].task_id.clone(),
                task_packet_hash: hash(&packet.tasks[0]).unwrap(),
                independent_verifier: true,
                input: VerifierInput::PacketDiffAndEvidence,
                memory_refs: vec![],
                exclusions: vec![],
                non_goals: vec![],
            }],
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

#[test]
fn literal_scopes_authorize_exactly_the_named_path_through_import_and_activation() {
    let f = Fixture::new();
    f.store().index_repository(&f.root).unwrap();
    let prepared = f
        .prepare(
            "Make the slug page handler trim its input",
            vec![ScopePath::Directory { path: "app".into() }],
        )
        .unwrap();
    let entity = |name: &str| {
        f.store()
            .graph(&f.root)
            .unwrap()
            .symbols(name, SearchMode::Exact, 1)
            .unwrap()
            .data[0]
            .id
            .clone()
    };
    let slug = entity("slugPageHandler");
    assert!(
        prepared
            .context
            .graph
            .primary
            .iter()
            .any(|p| p.entity.id == slug)
    );
    let good = plan(
        &prepared,
        ScopePath::Directory {
            path: "app/[slug]".into(),
        },
        ScopePath::File {
            path: "app/[slug]/page.tsx".into(),
        },
        slug.clone(),
    );
    f.store().import_execution_plan(&f.root, &good).unwrap();
    f.store()
        .activate_execution_plan(&f.root, &good.packet.plan_id)
        .unwrap();
    // `[slug]` is not a character class: a read scope naming it does not
    // authorize a graph reference to the entity under `app/s`.
    let decoy = entity("decoySingleLetterS");
    let mut broadened = plan(
        &prepared,
        ScopePath::Directory {
            path: "app/[slug]".into(),
        },
        ScopePath::File {
            path: "app/[slug]/page.tsx".into(),
        },
        decoy,
    );
    broadened.packet.plan_id = PlanId::new("plan:broadened").unwrap();
    broadened.metadata.integration.plan_id = broadened.packet.plan_id.clone();
    broadened.metadata.contracts[0].task_packet_hash = hash(&broadened.packet.tasks[0]).unwrap();
    broadened.metadata.integration.plan_packet_hash = hash(&broadened.packet).unwrap();
    assert!(
        f.store()
            .import_execution_plan(&f.root, &broadened)
            .is_err()
    );
}

#[test]
fn literal_paths_are_valid_memory_links_and_reach_planner_memory() {
    let f = Fixture::new();
    f.store().index_repository(&f.root).unwrap();
    let memory = f
        .store()
        .add_memory(
            &f.root,
            MemoryDraft {
                kind: MemoryKind::Finding,
                content: "The catch-all route must keep trailing segments".into(),
                workspace_id: None,
                canonical_key: None,
                actor: "literal-test".into(),
                author_job_id: None,
                links: vec![MemoryLink::File {
                    path: "app/[...slug]/page.tsx".into(),
                }],
            },
            MemoryTrustClass::Canonical,
            None,
        )
        .unwrap();
    let context = f
        .store()
        .graph(&f.root)
        .unwrap()
        .context("catch all page handler", ContextLimits::default())
        .unwrap();
    assert_eq!(
        context.primary[0].entity.provenance.path,
        "app/[...slug]/page.tsx"
    );
    let found = f
        .store()
        .memory_for_code(&f.root, &context, MemoryLimits::default())
        .unwrap();
    assert!(found.items.iter().any(|m| m.id == memory.id.as_str()));
}

#[test]
fn literal_path_support_keeps_traversal_absolute_and_wildcard_scopes_rejected() {
    let f = Fixture::new();
    f.store().index_repository(&f.root).unwrap();
    for bad in [
        "app/[slug]/../s",
        "../app/[slug]",
        "/app/[slug]",
        "app//[slug]",
        "app/./[slug]",
        "app/[slug]/",
        "C:\\app",
        "app\\[slug]",
        "app:[slug]",
        "app/[slug]\n",
        "app/*",
        "app/**",
        "app/[slug]/pa?e.tsx",
    ] {
        for scope in [
            ScopePath::Directory { path: bad.into() },
            ScopePath::File { path: bad.into() },
        ] {
            assert!(
                f.prepare("Make the page handler trim input", vec![scope])
                    .is_err(),
                "{bad:?}"
            );
        }
    }
    // A symlinked literal-looking directory is still not followed or indexed.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(f.temp.0.join("outside"), f.root.join("app/[link]")).unwrap();
        fs::create_dir_all(f.temp.0.join("outside")).unwrap();
        write(&f.temp.0, "outside/leak.ts", &source("outsideLeak"));
        let stats = f.store().index_repository(&f.root).unwrap();
        assert_eq!(stats.failed, 0);
        assert!(
            f.store()
                .graph(&f.root)
                .unwrap()
                .symbols("outsideLeak", SearchMode::Exact, 1)
                .unwrap()
                .data
                .is_empty()
        );
    }
}

#[test]
fn literal_path_changes_are_detected_as_stale_and_reindexed() {
    let f = Fixture::new();
    let first = f.store().index_repository(&f.root).unwrap();
    write(
        &f.root,
        "app/[[...slug]]/page.tsx",
        &source("renamedOptionalHandler"),
    );
    let status = f.store().index_status(&f.root).unwrap();
    assert_eq!(status.stale_files, vec!["app/[[...slug]]/page.tsx"]);
    assert!(f.store().graph(&f.root).is_err());
    let second = f.store().index_repository(&f.root).unwrap();
    assert_eq!((second.indexed, second.failed), (1, 0));
    assert!(
        second.generation.as_ref().unwrap().sequence > first.generation.as_ref().unwrap().sequence
    );
    let renamed = f
        .store()
        .graph(&f.root)
        .unwrap()
        .symbols("renamedOptionalHandler", SearchMode::Exact, 1)
        .unwrap()
        .data;
    assert_eq!(renamed[0].provenance.path, "app/[[...slug]]/page.tsx");
}

#[test]
fn real_cli_indexes_and_queries_the_issue_one_repository_shape() {
    let f = Fixture::new();
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_agentctl"))
            .current_dir(&f.root)
            .args(args)
            .env("HOME", f.temp.0.join("home"))
            .env("XDG_CONFIG_HOME", f.temp.0.join("config"))
            .env("XDG_DATA_HOME", f.temp.0.join("data"))
            .env("XDG_CACHE_HOME", f.temp.0.join("cache"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap()
    };
    for args in [&["init"][..], &["repo", "init"], &["repo", "index"]] {
        let out = cli(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = cli(&[
        "code",
        "file",
        "app/(customer)/s/[slug]/actions.ts",
        "--json",
    ]);
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let data = value["data"].as_array().unwrap();
    assert!(!data.is_empty());
    assert!(
        data.iter()
            .all(|e| e["provenance"]["path"] == "app/(customer)/s/[slug]/actions.ts")
    );
}
