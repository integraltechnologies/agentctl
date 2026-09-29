//! `agenttop` refuses to start without a terminal, and writes no escape
//! codes when it does.

use std::process::{Command, Stdio};

const CONFIG: &str = r#"[project]
name = "demo"
version = "0.1.0"

[codegraph]
roots = ["src"]

[agents]
max_concurrency = 4

[agents.planner]
provider = "claude"
model = "claude-opus-5-5"
reasoning_effort = "high"

[agents.executor]
provider = "claude"
model = "claude-opus-5-5"
reasoning_effort = "medium"

[agents.verifier]
provider = "claude"
model = "claude-opus-5-5"
reasoning_effort = "xhigh"
"#;

#[test]
fn a_pipe_is_refused_before_the_terminal_is_touched() {
    let dir = tempfile::tempdir().unwrap();
    agentctl::config::Config::parse(CONFIG).expect("the fixture config parses");
    std::fs::write(dir.path().join("agentctl.toml"), CONFIG).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agenttop"))
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("a terminal is required"), "{stderr}");
    assert!(!out.stdout.contains(&0x1b), "{:?}", out.stdout);
    assert!(!out.stderr.contains(&0x1b), "{stderr:?}");
    assert!(!dir.path().join(".agentctl").exists());
}
