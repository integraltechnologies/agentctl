//! Deterministic JSON Schema generation from the canonical Rust definitions.

use std::{collections::BTreeMap, path::Path};

use crate::{Validate, protocol::*};

// One registry keeps schema names and CLI validation types synchronized.
macro_rules! documents {
    ($($name:literal => $ty:ty),+ $(,)?) => {
        pub const DOCUMENT_TYPES: &[&str] = &[$($name),+];

        pub fn schemas() -> BTreeMap<&'static str, schemars::Schema> {
            BTreeMap::from([$((concat!($name, ".schema.json"), schemars::schema_for!($ty))),+])
        }

        pub fn validate_json(kind: &str, json: &str) -> Result<(), Box<dyn std::error::Error>> {
            match kind {
                $($name => serde_json::from_str::<$ty>(json)?.validate()?,)+
                _ => return Err(format!("unknown protocol type {kind:?}; expected {}", DOCUMENT_TYPES.join(", ")).into()),
            }
            Ok(())
        }
    };
}

documents! {
    "plan" => PlanPacket,
    "task" => TaskPacket,
    "result" => ResultPacket,
    "verification" => VerificationPacket,
    "resume" => ResumePacket,
    "evidence" => EvidenceRecord,
    "agent-job" => AgentJob,
    "agent-event" => AgentEvent,
    "probe" => ProbeSnapshot,
    "token-usage" => TokenUsageEvent,
    "experiment" => ExperimentSpec,
    "experiment-event" => ExperimentEvent,
    "memory-provenance" => MemoryProvenance,
}

/// Replaces only the named generated schema files; unrelated files are untouched.
pub fn generate(output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(output)?;
    for (name, schema) in schemas() {
        let mut json = serde_json::to_string_pretty(&schema)?;
        json.push('\n');
        std::fs::write(output.join(name), json)?;
    }
    Ok(())
}
