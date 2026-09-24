//! The host platform boundary: which host agentctl runs on, what agentctl
//! can currently enforce there, and the filesystem primitives whose
//! semantics differ between operating systems.
//!
//! Capabilities report what agentctl itself implements and relies on, not
//! what the operating system could offer. They describe the current host, so
//! they are never persisted and never enter canonical state or graph
//! identity. Mechanism names are diagnostic evidence, not policy.

use std::fmt;

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
compile_error!("agentctl supports macOS, Linux and Windows");

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as backend;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as backend;

pub(crate) use backend::{
    literal_name, open_regular, publish_new, replace, stage, sync_dir, write_back,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
    /// An architecture agentctl is not verified on, as Rust names it.
    Other(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Host {
    pub os: Os,
    pub arch: Arch,
}

/// The host agentctl was compiled for and is running on.
pub fn host() -> Host {
    let os = if cfg!(target_os = "macos") {
        Os::MacOs
    } else if cfg!(target_os = "linux") {
        Os::Linux
    } else {
        Os::Windows
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => Arch::X86_64,
        "aarch64" => Arch::Aarch64,
        other => Arch::Other(other),
    };
    Host { os, arch }
}

/// What agentctl may need a backend to guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Opening an existing entry as a regular file, never letting the final
    /// path component redirect through a symlink or reparse point.
    NoFollowOpen,
    /// Resolving a path beneath a root so that no concurrent change to an
    /// ancestor can redirect it elsewhere.
    ConfinedResolution,
    /// Publishing or replacing a file atomically, such that it survives a
    /// crash once agentctl's durability barrier returns.
    DurablePublication,
    /// Terminating a process together with every descendant it started.
    ProcessTreeTermination,
    /// Bounding a process tree's CPU, memory and process count.
    ResourceLimits,
    /// Restricting which files a process tree can read and write.
    FilesystemIsolation,
    /// Restricting which network destinations a process tree can reach.
    NetworkIsolation,
    /// Starting a process with exactly the environment agentctl constructs.
    EnvironmentControl,
}

impl Capability {
    pub const ALL: [Capability; 8] = [
        Capability::NoFollowOpen,
        Capability::ConfinedResolution,
        Capability::DurablePublication,
        Capability::ProcessTreeTermination,
        Capability::ResourceLimits,
        Capability::FilesystemIsolation,
        Capability::NetworkIsolation,
        Capability::EnvironmentControl,
    ];
}

/// How strongly the current backend provides a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Implemented and used strongly enough to rely on as an invariant.
    Enforced,
    /// A useful mitigation, never a security boundary.
    BestEffort,
    Unsupported,
}

/// A capability's level on this host, with the evidence for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Support {
    pub capability: Capability,
    pub level: Level,
    /// The backend mechanism, for diagnostics.
    pub mechanism: &'static str,
    /// Why the level is not `Enforced`, when that is not self-evident.
    pub reason: Option<&'static str>,
}

/// Every capability's support on this host, in [`Capability::ALL`] order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities([Support; Capability::ALL.len()]);

/// Discovers what agentctl can enforce on this host. Deterministic, and free
/// of side effects: it reports what the compiled backend implements.
pub fn capabilities() -> Capabilities {
    let support = |capability, level, mechanism, reason| Support {
        capability,
        level,
        mechanism,
        reason,
    };
    let not_implemented = |capability| {
        support(
            capability,
            Level::Unsupported,
            "not_implemented",
            Some("agentctl has no backend mechanism for this yet"),
        )
    };
    Capabilities([
        support(
            Capability::NoFollowOpen,
            Level::Enforced,
            backend::NO_FOLLOW_OPEN,
            None,
        ),
        support(
            Capability::ConfinedResolution,
            Level::BestEffort,
            "check_then_use",
            Some("ancestors are checked, then reused by path"),
        ),
        support(
            Capability::DurablePublication,
            Level::Enforced,
            backend::DURABLE_PUBLICATION,
            None,
        ),
        not_implemented(Capability::ProcessTreeTermination),
        not_implemented(Capability::ResourceLimits),
        not_implemented(Capability::FilesystemIsolation),
        not_implemented(Capability::NetworkIsolation),
        not_implemented(Capability::EnvironmentControl),
    ])
}

impl Capabilities {
    pub fn get(&self, capability: Capability) -> Support {
        self.0[Capability::ALL
            .iter()
            .position(|&c| c == capability)
            .expect("every capability is listed")]
    }

    pub fn iter(&self) -> impl Iterator<Item = Support> + '_ {
        self.0.iter().copied()
    }

    /// Checks `requirements` against this host, reporting every one it
    /// cannot meet. Requirements are never relaxed.
    pub fn check(&self, requirements: &[Requirement]) -> Result<(), Unmet> {
        let shortfalls: Vec<Shortfall> = requirements
            .iter()
            .filter_map(|&required| {
                let actual = self.get(required.capability);
                (!required.need.accepts(actual.level)).then_some(Shortfall { required, actual })
            })
            .collect();
        match shortfalls.is_empty() {
            true => Ok(()),
            false => Err(Unmet(shortfalls)),
        }
    }
}

/// The strength a caller requires of a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    RequireEnforced,
    AllowBestEffort,
}

impl Need {
    pub fn accepts(self, level: Level) -> bool {
        match level {
            Level::Enforced => true,
            Level::BestEffort => self == Need::AllowBestEffort,
            Level::Unsupported => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requirement {
    pub capability: Capability,
    pub need: Need,
}

/// A requirement the host does not meet, and what it offers instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortfall {
    pub required: Requirement,
    pub actual: Support,
}

/// The requirements a host does not meet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmet(pub Vec<Shortfall>);

impl fmt::Display for Unmet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "this host cannot meet:")?;
        for s in &self.0 {
            write!(
                f,
                " {:?} ({:?}, host is {:?} via {})",
                s.required.capability, s.required.need, s.actual.level, s.actual.mechanism
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for Unmet {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use std::io::{self, Read, Write};

    fn host_with(level: Level) -> Capabilities {
        let mut caps = capabilities();
        caps.0[0].level = level;
        caps
    }

    fn require(capability: Capability, need: Need) -> Requirement {
        Requirement { capability, need }
    }

    #[test]
    fn identifies_the_compiled_host() {
        let host = host();
        assert_eq!(host.os == Os::MacOs, cfg!(target_os = "macos"));
        assert_eq!(host.os == Os::Linux, cfg!(target_os = "linux"));
        assert_eq!(host.os == Os::Windows, cfg!(target_os = "windows"));
        let arch = if cfg!(target_arch = "x86_64") {
            Arch::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Arch::Aarch64
        } else {
            Arch::Other(std::env::consts::ARCH)
        };
        assert_eq!(host.arch, arch);
    }

    #[test]
    fn discovery_lists_each_capability_once_deterministically() {
        let caps = capabilities();
        let listed: Vec<_> = caps.iter().map(|s| s.capability).collect();
        assert_eq!(listed, Capability::ALL);
        assert_eq!(listed.iter().collect::<HashSet<_>>().len(), listed.len());
        for capability in Capability::ALL {
            assert_eq!(caps.get(capability).capability, capability);
        }
        assert_eq!(caps, capabilities());
    }

    #[test]
    fn reports_only_what_agentctl_implements() {
        let caps = capabilities();
        for capability in [
            Capability::ProcessTreeTermination,
            Capability::ResourceLimits,
            Capability::FilesystemIsolation,
            Capability::NetworkIsolation,
            Capability::EnvironmentControl,
        ] {
            assert_eq!(caps.get(capability).level, Level::Unsupported);
        }
        // Ancestors are checked by path, so a concurrent swap is possible.
        assert_eq!(
            caps.get(Capability::ConfinedResolution).level,
            Level::BestEffort
        );
        for capability in [Capability::NoFollowOpen, Capability::DurablePublication] {
            assert_eq!(caps.get(capability).level, Level::Enforced);
        }
    }

    #[test]
    fn needs_accept_only_sufficient_levels() {
        use Level::*;
        use Need::*;
        assert!(RequireEnforced.accepts(Enforced));
        assert!(!RequireEnforced.accepts(BestEffort));
        assert!(!RequireEnforced.accepts(Unsupported));
        assert!(AllowBestEffort.accepts(Enforced));
        assert!(AllowBestEffort.accepts(BestEffort));
        assert!(!AllowBestEffort.accepts(Unsupported));
    }

    #[test]
    fn check_reports_every_unmet_requirement_unrelaxed() {
        let caps = host_with(Level::BestEffort);
        let weak = caps.get(Capability::NoFollowOpen);
        let network = caps.get(Capability::NetworkIsolation);
        let requirements = [
            require(Capability::NoFollowOpen, Need::RequireEnforced),
            require(Capability::DurablePublication, Need::RequireEnforced),
            require(Capability::NetworkIsolation, Need::AllowBestEffort),
        ];
        let Unmet(shortfalls) = caps.check(&requirements).unwrap_err();
        assert_eq!(
            shortfalls,
            [
                Shortfall {
                    required: requirements[0],
                    actual: weak,
                },
                Shortfall {
                    required: requirements[2],
                    actual: network,
                },
            ]
        );

        assert!(
            caps.check(&[require(Capability::NoFollowOpen, Need::AllowBestEffort)])
                .is_ok()
        );
        assert!(
            host_with(Level::Enforced)
                .check(&[require(Capability::NoFollowOpen, Need::RequireEnforced)])
                .is_ok()
        );
        assert!(
            host_with(Level::Unsupported)
                .check(&[require(Capability::NoFollowOpen, Need::AllowBestEffort)])
                .is_err()
        );
        assert!(caps.check(&[]).is_ok());
    }

    #[test]
    fn opens_only_existing_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, b"bytes").unwrap();
        let mut bytes = Vec::new();
        open_regular(&file)
            .unwrap()
            .expect("a regular file")
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"bytes");
        assert!(open_regular(dir.path()).unwrap().is_none(), "a directory");
        let absent = open_regular(&dir.path().join("absent")).unwrap_err();
        assert_eq!(absent.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn publication_never_replaces_but_replacement_does() {
        let dir = tempfile::tempdir().unwrap();
        let staged = |bytes: &[u8]| {
            let mut file = stage(dir.path()).unwrap();
            file.write_all(bytes).unwrap();
            write_back(file.as_file()).unwrap();
            file
        };
        let dest = dir.path().join("dest");

        publish_new(staged(b"first"), &dest).unwrap();
        let clobber = publish_new(staged(b"second"), &dest).unwrap_err();
        assert_eq!(clobber.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&dest).unwrap(), b"first");

        replace(staged(b"third"), &dest).unwrap();
        sync_dir(dir.path()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"third");
        // Only `dest` remains: abandoned staged files are cleaned up.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
