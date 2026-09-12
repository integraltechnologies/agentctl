//! Provider-owned authentication. Never open or deserialize credential files.
use super::*;
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AuthMode {
    #[default]
    Auto,
    Native,
    ApiKey,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Authentication {
    #[serde(default)]
    pub mode: AuthMode,
    /// Environment variable NAME, never its value. Required to opt into API use.
    pub api_key_env: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct AuthStatus {
    pub authenticated: bool,
    pub method: &'static str,
    pub api_key_required: bool,
    pub guidance: &'static str,
}
#[derive(Debug, Clone)]
pub struct NativeAuth {
    pub provider: String,
    pub home: PathBuf,
    pub provider_home: PathBuf,
    pub config_override: bool,
}
impl NativeAuth {
    pub fn discover(provider: &str) -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Error::Invalid("native provider authentication requires HOME".into()))?;
        let variable = if provider == "codex" {
            "CODEX_HOME"
        } else {
            "CLAUDE_CONFIG_DIR"
        };
        let override_path = std::env::var_os(variable);
        let provider_home = override_path.clone().map(PathBuf::from).unwrap_or_else(|| {
            home.join(if provider == "codex" {
                ".codex"
            } else {
                ".claude"
            })
        });
        paths::absolute_path(&home)?;
        paths::absolute_path(&provider_home)?;
        Ok(Self {
            provider: provider.into(),
            home,
            provider_home,
            config_override: override_path.is_some(),
        })
    }
    pub(super) fn environment(&self, command: &mut Command) {
        command.env("HOME", &self.home);
        // macOS Keychain lookup in Claude requires the ordinary login identity
        // environment as well as HOME; these are names, never credentials.
        for name in ["USER", "LOGNAME"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if self.provider == "codex" {
            command.env("CODEX_HOME", &self.provider_home);
        } else if self.config_override {
            command.env("CLAUDE_CONFIG_DIR", &self.provider_home);
        } else {
            command.env_remove("CLAUDE_CONFIG_DIR");
        }
    }
    pub(super) fn readable_files(&self) -> Vec<PathBuf> {
        if self.provider == "codex" {
            vec![self.provider_home.join("auth.json")]
        } else {
            vec![
                self.provider_home.join(".credentials.json"),
                if self.config_override {
                    self.provider_home.join(".claude.json")
                } else {
                    self.home.join(".claude.json")
                },
            ]
        }
    }
}
impl Authentication {
    pub fn validate(&self) -> Result<()> {
        if let Some(name) = &self.api_key_env {
            require(
                !name.is_empty()
                    && name.len() <= 128
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_'),
                "api_key_env must be an environment variable name, not a credential",
            )?;
        }
        require(
            self.mode != AuthMode::ApiKey || self.api_key_env.is_some(),
            "API_KEY authentication requires api_key_env (variable name only)",
        )
    }
    pub fn preflight(&self, executable: &Path, native: &NativeAuth) -> Result<AuthStatus> {
        self.validate()?;
        if self.mode != AuthMode::ApiKey && native_status(executable, native)? {
            return Ok(AuthStatus {
                authenticated: true,
                method: "NATIVE",
                api_key_required: false,
                guidance: "provider-native login; fresh worker conversations",
            });
        }
        if self.mode != AuthMode::Native
            && self
                .api_key_env
                .as_ref()
                .is_some_and(|n| std::env::var_os(n).is_some_and(|v| !v.is_empty()))
        {
            return Ok(AuthStatus {
                authenticated: true,
                method: "API_KEY",
                api_key_required: true,
                guidance: "explicitly configured API-key environment",
            });
        }
        Ok(AuthStatus {
            authenticated: false,
            method: "UNAVAILABLE",
            api_key_required: false,
            guidance: if native.provider == "codex" {
                "Run codex login with your normal CODEX_HOME, or explicitly configure authentication.api_key_env; API keys are not required for native login."
            } else {
                "Run claude auth login with your normal HOME/CLAUDE_CONFIG_DIR and allow native Keychain access, or explicitly configure authentication.api_key_env."
            },
        })
    }
    pub(super) fn configure(
        &self,
        executable: &Path,
        provider: &str,
        process: &mut process::ProcessSpec,
    ) -> Result<()> {
        let native = NativeAuth::discover(provider)?;
        let status = self.preflight(executable, &native)?;
        require(status.authenticated, status.guidance)?;
        if status.method == "NATIVE" {
            process.native_auth = Some(native);
        } else {
            process.api_key = Some((
                self.api_key_env.clone().expect("explicit key variable"),
                if provider == "codex" {
                    "CODEX_API_KEY"
                } else {
                    "ANTHROPIC_API_KEY"
                }
                .into(),
            ));
        }
        Ok(())
    }
}
fn native_status(executable: &Path, native: &NativeAuth) -> Result<bool> {
    let mut command = Command::new(executable);
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(&native.home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    native.environment(&mut command);
    if native.provider == "codex" {
        command.args([
            "-c",
            "cli_auth_credentials_store=\"auto\"",
            "login",
            "status",
        ]);
    } else {
        command.args(["--safe-mode", "auth", "status", "--json"]);
    }
    let mut child = command.spawn().map_err(|_| {
        Error::Invalid("provider authentication probe could not start; check executable".into())
    })?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                return Ok(false);
            }
            if native.provider == "codex" {
                return Ok(true);
            }
            use std::io::Read;
            let mut bytes = Vec::new();
            child
                .stdout
                .take()
                .expect("piped status")
                .take(8193)
                .read_to_end(&mut bytes)?;
            require(
                bytes.len() <= 8192,
                "provider auth status exceeded its bound",
            )?;
            // Status output is discarded, including account metadata. Never log it.
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|_| Error::Invalid("unsupported Claude auth status response".into()))?;
            return Ok(value["loggedIn"].as_bool() == Some(true));
        }
        if start.elapsed() > Duration::from_secs(5) {
            child.kill()?;
            child.wait()?;
            return Err(Error::Invalid(
                "provider auth status timed out; no model was invoked".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Defense in depth for provider output; no credential files are inspected.
pub(super) fn redact(bytes: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    text.split_inclusive(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ',' | '}'))
        .map(|word| {
            if word.contains("sk-") || (word.contains("eyJ") && word.len() > 24) {
                "[REDACTED]".to_owned()
            } else {
                word.to_owned()
            }
        })
        .collect::<String>()
        .into_bytes()
}
