use agentctl::local::{
    config::{MachineConfig, ProjectConfig},
    runtime::{routing::*, *},
};
use std::collections::{BTreeMap, BTreeSet};

fn config() -> RuntimeConfig {
    let mut c = RuntimeConfig::default();
    for id in ["p", "e", "v", "alt"] {
        c.providers.insert(
            id.into(),
            ProviderConfig {
                adapter: "codex".into(),
                executable: "/missing/never-launch".into(),
                authentication: Default::default(),
            },
        );
    }
    for (role, provider) in [
        ("planner", "p"),
        ("executor", "e"),
        ("verifier", "v"),
        ("recon", "p"),
        ("reviewer", "v"),
    ] {
        c.roles.insert(role.into(), route(provider));
    }
    c
}
fn route(provider: &str) -> RoleConfig {
    RoleConfig {
        provider: provider.into(),
        model: Some("opaque-next-generation".into()),
        effort: None,
    }
}
fn resolve_default(c: &RuntimeConfig, role: &str) -> ResolvedRoute {
    resolve(c, &ProjectRoles::default(), role, None).unwrap()
}

#[test]
fn builtins_have_safe_distinct_semantics_and_legacy_routes() {
    let c = config();
    for name in BUILTINS {
        let r = resolve_default(&c, name);
        assert_eq!(r.primary, c.roles[name]);
        assert_eq!(r.profile.read_only, name != "executor");
        assert_eq!(r.profile.max_correction_rounds, 2);
        assert_eq!(r.profile.context_bytes, 262144);
        assert!(r.fallbacks.is_empty());
    }
    assert!(
        resolve_default(&c, "planner")
            .profile
            .context_policy
            .contains("PlannerPacket")
    );
    assert!(
        resolve_default(&c, "verifier")
            .profile
            .objective
            .contains("independently")
            || resolve_default(&c, "verifier")
                .profile
                .objective
                .contains("Independently")
    );
}
#[test]
fn explicit_project_machine_precedence_is_field_wise_and_reproducible() {
    let mut c = config();
    c.profiles.insert(
        "executor".into(),
        RolePatch {
            model: Some("machine-model".into()),
            context_bytes: Some(120000),
            ..Default::default()
        },
    );
    let mut project = ProjectRoles::default();
    project.profiles.insert(
        "executor".into(),
        RolePatch {
            provider: Some("v".into()),
            context_bytes: Some(80000),
            ..Default::default()
        },
    );
    let explicit = RolePatch {
        model: Some("user-model".into()),
        ..Default::default()
    };
    let r = resolve(&c, &project, "executor", Some(&explicit)).unwrap();
    assert_eq!(r.primary.provider, "v");
    assert_eq!(r.primary.model.as_deref(), Some("user-model"));
    assert_eq!(r.profile.context_bytes, 80000);
    assert_eq!(r.sources["provider"], "project profiles");
    assert_eq!(r.sources["model"], "explicit user");
    let reopened = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
    let p = toml::from_str(&toml::to_string(&project).unwrap()).unwrap();
    assert_eq!(
        r,
        resolve(&reopened, &p, "executor", Some(&explicit)).unwrap()
    );
    assert_eq!(
        resolve(&c, &project, "executor", None)
            .unwrap()
            .primary
            .model
            .as_deref(),
        Some("machine-model")
    );
}
#[test]
fn custom_role_does_not_change_protocol_enum() {
    let mut c = config();
    c.profiles.insert(
        "security-review".into(),
        RolePatch {
            provider: Some("v".into()),
            objective: Some("Review bounded security findings".into()),
            ..Default::default()
        },
    );
    let r = resolve_default(&c, "security-review");
    assert_eq!(r.profile.role, "security-review");
    assert!(r.profile.read_only);
    assert!(resolve(&c, &ProjectRoles::default(), "unknown", None).is_err());
}
#[test]
fn hard_project_restrictions_beat_override_and_fallback() {
    let mut c = config();
    let p = ProjectRoles {
        allowed_providers: Some(BTreeSet::from(["e".into()])),
        ..Default::default()
    };
    assert!(resolve(&c, &p, "executor", None).is_ok());
    assert!(
        resolve(
            &c,
            &p,
            "executor",
            Some(&RolePatch {
                provider: Some("alt".into()),
                ..Default::default()
            })
        )
        .is_err()
    );
    c.profiles.insert(
        "executor".into(),
        RolePatch {
            fallbacks: Some(vec![route("alt")]),
            ..Default::default()
        },
    );
    let filtered = resolve(&c, &p, "executor", None).unwrap();
    assert_eq!(filtered.primary.provider, "e");
    assert_eq!(filtered.policy_skipped, vec![route("alt")]);
}
#[test]
fn permissions_budgets_and_correction_are_not_provider_capabilities() {
    let c = config();
    let p = ProjectRoles {
        read_only: true,
        deny_network: true,
        max_context_bytes: Some(40000),
        ..Default::default()
    };
    let user = RolePatch {
        read_only: Some(false),
        network: Some(true),
        context_bytes: Some(200000),
        timeout_ms: Some(3600000),
        advisory_tokens: Some(17),
        ..Default::default()
    };
    let r = resolve(&c, &p, "executor", Some(&user)).unwrap();
    assert!(r.profile.read_only);
    assert!(!r.profile.network);
    assert_eq!(r.profile.context_bytes, 40000);
    assert_eq!(r.profile.timeout_ms, c.timeout_ms);
    assert_eq!(r.profile.advisory_tokens, Some(17));
    assert!(resolve(&c, &p, "verifier", Some(&user)).is_err());
}
#[test]
fn cycles_duplicates_and_excessive_chains_fail_closed() {
    let mut c = config();
    for alternatives in [
        vec![route("e")],
        vec![route("alt"), route("alt")],
        vec![route("alt"); 5],
    ] {
        c.profiles.insert(
            "executor".into(),
            RolePatch {
                fallbacks: Some(alternatives),
                ..Default::default()
            },
        );
        assert!(resolve(&c, &ProjectRoles::default(), "executor", None).is_err());
    }
    c.profiles.insert(
        "executor".into(),
        RolePatch {
            max_fallback_attempts: Some(5),
            ..Default::default()
        },
    );
    assert!(c.validate().is_err());
}
#[test]
fn invalid_fields_and_unconfigured_provider_are_rejected() {
    let c = config();
    for patch in [
        RolePatch {
            context_bytes: Some(0),
            ..Default::default()
        },
        RolePatch {
            timeout_ms: Some(0),
            ..Default::default()
        },
        RolePatch {
            advisory_tokens: Some(0),
            ..Default::default()
        },
        RolePatch {
            provider: Some(String::new()),
            ..Default::default()
        },
        RolePatch {
            provider: Some("missing".into()),
            ..Default::default()
        },
        RolePatch {
            model: Some("bad\nmodel".into()),
            ..Default::default()
        },
    ] {
        assert!(resolve(&c, &ProjectRoles::default(), "executor", Some(&patch)).is_err());
    }
    assert!(toml::from_str::<RolePatch>("provider='e'\nprovider='v'").is_err());
    assert!(toml::from_str::<RolePatch>("enforced_tokens=100").is_err());
}
#[test]
fn capabilities_are_checked_without_catalog_or_process() {
    let mut cap = provider::Capabilities {
        model: true,
        effort: true,
        fresh_session: true,
        structured_output: true,
        token_usage: false,
    };
    assert!(validate_capabilities(&cap, &route("p")).is_ok());
    cap.model = false;
    assert!(validate_capabilities(&cap, &route("p")).is_err());
    cap.model = true;
    cap.fresh_session = false;
    assert!(validate_capabilities(&cap, &route("p")).is_err());
    cap.fresh_session = true;
    cap.structured_output = false;
    assert!(validate_capabilities(&cap, &route("p")).is_err());
}
#[test]
fn legacy_config_and_project_serialization_keep_prior_policy_hash_inputs() {
    let old = "version=1\nbusy_timeout_ms=5000\n[runtime.roles.executor]\nprovider='claude-user'\nmodel='opaque'\n[runtime.providers.claude-user]\nadapter='claude'\nexecutable='/usr/bin/true'\n";
    let machine: MachineConfig = toml::from_str(old).unwrap();
    machine.validate().unwrap();
    assert_eq!(
        resolve_default(&machine.runtime, "executor")
            .primary
            .provider,
        "claude-user"
    );
    let p: ProjectConfig = toml::from_str("version=1").unwrap();
    assert!(serde_json::to_value(&p).unwrap().get("routing").is_none());
    assert!(!toml::to_string(&p).unwrap().contains("routing"));
}
#[test]
fn recon_and_custom_compilation_deterministic_bounded_no_repository_dump() {
    for role in ["recon", "security-review"] {
        let profile = builtin(role, &config());
        let one = prompt::compile_helper(
            &profile,
            "Locate cache symbol",
            &["graph: cache_api in src/api.rs".into()],
        )
        .unwrap();
        let two = prompt::compile_helper(
            &profile,
            "Locate cache symbol",
            &["graph: cache_api in src/api.rs".into()],
        )
        .unwrap();
        assert_eq!(one.bytes, two.bytes);
        assert_eq!(one.provenance, two.provenance);
        assert!(one.bytes.len() < 4096);
        let text = String::from_utf8(one.bytes).unwrap();
        assert!(text.contains("graph"));
        assert!(!text.contains("unrelated.rs"));
        let tiny = RoleProfile {
            context_bytes: 20,
            ..profile
        };
        assert!(
            prompt::compile_helper(&tiny, "critical invariant", &[])
                .unwrap_err()
                .to_string()
                .contains("no critical material was truncated")
        );
    }
}
#[test]
fn partial_fallback_override_replaces_not_appends() {
    let mut c = config();
    c.profiles.insert(
        "executor".into(),
        RolePatch {
            fallbacks: Some(vec![route("alt")]),
            ..Default::default()
        },
    );
    let r = resolve(
        &c,
        &ProjectRoles::default(),
        "executor",
        Some(&RolePatch {
            fallbacks: Some(vec![]),
            ..Default::default()
        }),
    )
    .unwrap();
    assert!(r.fallbacks.is_empty());
}
#[test]
fn builtin_only_never_silently_selects_a_provider() {
    assert!(
        resolve(
            &RuntimeConfig::default(),
            &ProjectRoles::default(),
            "executor",
            None
        )
        .unwrap_err()
        .to_string()
        .contains("configure a provider")
    );
    assert!(
        validate_patches(&BTreeMap::from([("BAD ROLE".into(), RolePatch::default())])).is_err()
    );
}

#[test]
fn policy_filters_ordered_configured_routes_but_rejects_forbidden_user_intent() {
    let mut c = config();
    c.profiles.insert(
        "executor".into(),
        RolePatch {
            fallbacks: Some(vec![route("p"), route("alt"), route("v")]),
            ..Default::default()
        },
    );
    for allowed in [vec!["alt", "v"], vec!["p", "alt"], vec!["v"]] {
        let p = ProjectRoles {
            allowed_providers: Some(allowed.iter().map(|s| (*s).into()).collect()),
            ..Default::default()
        };
        let r = resolve(&c, &p, "executor", None).unwrap();
        assert_eq!(r.configured_primary, route("e"));
        assert_eq!(r.primary.provider, allowed[0]);
        assert_eq!(
            r.fallbacks
                .iter()
                .map(|r| r.provider.as_str())
                .collect::<Vec<_>>(),
            allowed[1..]
        );
        assert!(r.policy_skipped.contains(&route("e")));
        assert!(
            resolve(
                &c,
                &p,
                "executor",
                Some(&RolePatch {
                    provider: Some("e".into()),
                    ..Default::default()
                })
            )
            .unwrap_err()
            .to_string()
            .contains("explicit")
        );
    }
    let p = ProjectRoles {
        allowed_providers: Some(BTreeSet::from(["not-in-chain".into()])),
        ..Default::default()
    };
    assert!(
        resolve(&c, &p, "executor", None)
            .unwrap_err()
            .to_string()
            .contains("all configured candidates forbidden")
    );
    let p = ProjectRoles {
        allowed_providers: Some(BTreeSet::from(["e".into()])),
        ..Default::default()
    };
    assert_eq!(
        resolve(
            &c,
            &p,
            "executor",
            Some(&RolePatch {
                provider: Some("e".into()),
                ..Default::default()
            })
        )
        .unwrap()
        .primary
        .provider,
        "e"
    );
}

#[test]
fn allowed_explicit_override_can_promote_an_existing_alternative() {
    let mut c = config();
    c.profiles.insert(
        "executor".into(),
        RolePatch {
            fallbacks: Some(vec![route("alt")]),
            ..Default::default()
        },
    );
    let p = ProjectRoles {
        allowed_providers: Some(BTreeSet::from(["alt".into()])),
        ..Default::default()
    };
    let r = resolve(
        &c,
        &p,
        "executor",
        Some(&RolePatch {
            provider: Some("alt".into()),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(r.primary.provider, "alt");
    assert!(r.fallbacks.is_empty());
}
