//! Security regression tests for the OS-agnostic runtime hardening phase.
//! Deterministic and offline: no model call, no network request.
#[allow(dead_code)]
mod common;
#[cfg(unix)]
use agentctl::local::{repository::RepositoryInfo, store::Store};
use agentctl::local::{
    runtime::{RuntimeConfig, routing},
    security::{ProjectSecurity, SecurityConfig},
};
use std::{fs, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
fn script(path: &Path, marker: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(
        path,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn hostile_git_filters_fsmonitor_and_hooks_are_not_executed_by_agentctl_git() {
    let temp = common::TempDir::new();
    let root = temp.0.join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "--quiet", "--initial-branch=main"]);
    fs::write(root.join("tracked.txt"), "one\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "baseline"]);
    let (filter_marker, monitor_marker, hook_marker) = (
        temp.0.join("filter-ran"),
        temp.0.join("fsmonitor-ran"),
        temp.0.join("hook-ran"),
    );
    let (filter, monitor) = (temp.0.join("filter.sh"), temp.0.join("fsmonitor.sh"));
    script(&filter, &filter_marker);
    script(&monitor, &monitor_marker);
    let hooks = temp.0.join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    for hook in ["post-index-change", "pre-commit", "post-checkout"] {
        script(&hooks.join(hook), &hook_marker);
    }
    // Drivers arrive through an included file to prove includes are followed.
    let included = temp.0.join("included.gitconfig");
    fs::write(
        &included,
        format!(
            "[filter \"evil\"]\n\tclean = {0}\n\tsmudge = {0}\n\trequired = true\n[diff \"evil\"]\n\ttextconv = {0}\n",
            filter.display()
        ),
    )
    .unwrap();
    for (key, value) in [
        ("include.path", included.display().to_string()),
        ("core.fsmonitor", monitor.display().to_string()),
        ("core.hooksPath", hooks.display().to_string()),
    ] {
        git(&root, &["config", "--local", key, &value]);
    }
    fs::write(root.join(".gitattributes"), "* filter=evil diff=evil\n").unwrap();
    // Same size, newer mtime: Git must re-read (and filter) the content.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fs::write(root.join("tracked.txt"), "one\n").unwrap();

    // Control: an ordinary Git status really executes the hostile configuration.
    let control = Command::new("git")
        .current_dir(&root)
        .args(["status", "--porcelain"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(control.status.success());
    assert!(
        filter_marker.exists() || monitor_marker.exists(),
        "fixture must be genuinely hostile for plain git"
    );
    for marker in [&filter_marker, &monitor_marker, &hook_marker] {
        let _ = fs::remove_file(marker);
    }
    // Make the index racy again so agentctl's status would have to filter too.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fs::write(root.join("tracked.txt"), "one\n").unwrap();

    let info = RepositoryInfo::discover(&root).unwrap();
    assert!(info.source.dirty, ".gitattributes is untracked");
    for marker in [&filter_marker, &monitor_marker, &hook_marker] {
        assert!(
            !marker.exists(),
            "agentctl Git executed {}",
            marker.display()
        );
    }
}

#[test]
fn repository_config_cannot_grant_itself_security_authority() {
    let machine: RuntimeConfig = toml::from_str(
        "[providers.p]\nadapter = 'claude'\nexecutable = '/usr/bin/true'\n[roles.executor]\nprovider = 'p'\n[profiles.executor]\nnetwork = false\nread_only = false\ntimeout_ms = 1000\ncontext_bytes = 4096\n",
    )
    .unwrap();
    let project: routing::ProjectRoles = toml::from_str(
        "[profiles.executor]\nnetwork = true\ntimeout_ms = 3600000\ncontext_bytes = 262144\n",
    )
    .unwrap();
    let route = routing::resolve(&machine, &project, "executor", None).unwrap();
    assert!(
        !route.profile.network,
        "project re-enabled machine-denied network"
    );
    assert_eq!(route.profile.timeout_ms, 1000);
    assert_eq!(route.profile.context_bytes, 4096);
    // Tightening still works.
    let tighter: routing::ProjectRoles =
        toml::from_str("[profiles.executor]\nread_only = true\ntimeout_ms = 10\n").unwrap();
    let route = routing::resolve(&machine, &tighter, "executor", None).unwrap();
    assert!(route.profile.read_only);
    assert_eq!(route.profile.timeout_ms, 10);
    // Resource/event ceilings: project values only lower machine values, and
    // repository config has no vocabulary for paths, environment or network.
    let project_security: ProjectSecurity = toml::from_str(
        "max_processes = 99999999\nmax_open_files = 100\nmax_experiment_events = 5\n",
    )
    .unwrap();
    let effective = SecurityConfig::default().tightened(&project_security);
    assert_eq!(
        effective.resources.max_processes,
        SecurityConfig::default().resources.max_processes
    );
    assert_eq!(effective.resources.max_open_files, Some(100));
    assert_eq!(effective.experiment_events.max_events_per_attempt, 5);
    for grant in [
        "read_roots = ['/']",
        "inherit_env = ['AWS_SECRET_ACCESS_KEY']",
        "network = true",
        "env = { A = 'b' }",
    ] {
        assert!(toml::from_str::<ProjectSecurity>(grant).is_err(), "{grant}");
    }
}

#[cfg(unix)]
#[test]
fn canonical_state_is_narrowed_to_owner_only_access() {
    use std::os::unix::fs::PermissionsExt;
    let temp = common::TempDir::new();
    let data = temp.0.join("data");
    fs::create_dir_all(&data).unwrap();
    fs::set_permissions(&data, fs::Permissions::from_mode(0o755)).unwrap();
    let database = data.join("state.sqlite3");
    drop(Store::open(&database, 5000).unwrap());
    fs::set_permissions(&database, fs::Permissions::from_mode(0o644)).unwrap();
    drop(Store::open(&database, 5000).unwrap());
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&data), 0o700);
    assert_eq!(mode(&database), 0o600);
    for sidecar in ["state.sqlite3-wal", "state.sqlite3-shm"] {
        if data.join(sidecar).exists() {
            assert_eq!(mode(&data.join(sidecar)) & 0o077, 0, "{sidecar}");
        }
    }
}

fn cli(home: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .current_dir(cwd)
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME")
        .output()
        .unwrap()
}

#[test]
fn human_error_output_neutralizes_terminal_control_sequences() {
    let temp = common::TempDir::new();
    let home = temp.0.join("home");
    fs::create_dir_all(&home).unwrap();
    assert!(cli(&home, &temp.0, &["init"]).status.success());
    let hostile = "--\x1b]0;pwned\x07\x1b[2J\u{9b}31m\u{202e}";
    let output = cli(&home, &temp.0, &["experiment", "run", hostile]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown experiment run flag"), "{stderr}");
    assert!(stderr.contains("\\u{1b}]0;pwned\\u{7}"), "{stderr}");
    assert!(
        !stderr
            .chars()
            .any(|c| (c.is_control() && c != '\n') || c == '\u{202e}')
    );
}

#[test]
fn security_doctor_reports_backend_capabilities_without_model_or_network() {
    let temp = common::TempDir::new();
    let home = temp.0.join("home");
    fs::create_dir_all(&home).unwrap();
    assert!(cli(&home, &temp.0, &["init"]).status.success());
    let output = cli(&home, &temp.0, &["security", "doctor", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let expected_backend = if cfg!(target_os = "macos") {
        "macos-seatbelt"
    } else if cfg!(target_os = "linux") {
        "linux-landlock-seccomp"
    } else if cfg!(windows) {
        "windows-job-objects"
    } else {
        "unsupported"
    };
    assert_eq!(report["backend"], expected_backend);
    let capabilities = report["capabilities"].as_array().unwrap();
    for name in [
        "FILESYSTEM_READ",
        "FILESYSTEM_WRITE",
        "NETWORK_DENY",
        "PROCESS_TREE",
        "MEMORY_LIMIT",
        "CREDENTIAL_ISOLATION",
    ] {
        let entry = capabilities
            .iter()
            .find(|c| c["capability"] == name)
            .unwrap();
        assert!(
            ["ENFORCED", "BEST_EFFORT", "UNSUPPORTED"].contains(&entry["status"].as_str().unwrap())
        );
    }
    if cfg!(target_os = "macos") {
        let memory = capabilities
            .iter()
            .find(|c| c["capability"] == "MEMORY_LIMIT")
            .unwrap();
        assert_eq!(
            memory["status"], "UNSUPPORTED",
            "XNU does not enforce RLIMIT_AS"
        );
    }
    // The exit status is the doctor's verdict and must agree with the report.
    let healthy = report["baseline_enforced"] == true
        && report["unsupported_hard_requirements"]
            .as_array()
            .unwrap()
            .is_empty()
        && report["self_test"] == "PASSED"
        && report["running_as_root"] == false;
    assert_eq!(output.status.success(), healthy, "{report:#}");
    assert!(report["machine_policy"]["explicit_env_names"].is_array());
}

#[test]
#[ignore = "native: requires host sandbox enforcement (macOS Seatbelt / Linux Landlock+seccomp)"]
fn native_explicit_experiment_secret_is_redacted_and_never_persisted() {
    let temp = common::TempDir::new();
    let home = temp.0.join("home");
    let root = temp.0.join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "--quiet", "--initial-branch=main"]);
    fs::write(root.join("README.md"), "fixture\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "baseline"]);
    assert!(cli(&home, &root, &["init"]).status.success());
    assert!(cli(&home, &root, &["repo", "init"]).status.success());
    let secret = "fixture-deploy-secret-7f3a9c1e5b2d";
    let output = Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .current_dir(&root)
        .args([
            "experiment",
            "run",
            "--program",
            "/bin/sh",
            "--arg",
            "-c",
            "--arg",
            "printf 'value=%s' \"$FIXTURE_DEPLOY_SECRET\"",
            "--env",
            "FIXTURE_DEPLOY_SECRET",
            "--timeout-ms",
            "20000",
            "--json",
        ])
        .env("HOME", &home)
        .env("FIXTURE_DEPLOY_SECRET", secret)
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
    let run: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(run["state"], "SUCCEEDED", "{run:#}");
    assert_eq!(
        run["env_passthrough"],
        serde_json::json!(["FIXTURE_DEPLOY_SECRET"])
    );
    // The worker received and printed the value, yet nothing agentctl persisted
    // (database, WAL, artifacts, evidence) contains it.
    let mut pending = vec![home.clone()];
    let mut redacted_stdout = false;
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let bytes = fs::read(&path).unwrap();
                assert!(
                    !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                    "secret persisted in {}",
                    path.display()
                );
                redacted_stdout |= bytes == b"value=[REDACTED]";
            }
        }
    }
    assert!(
        redacted_stdout,
        "captured stdout artifact should show the redaction"
    );
}
