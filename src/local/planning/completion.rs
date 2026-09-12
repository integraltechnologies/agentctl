//! Connection-local capability used by the durable completion trigger. There is
//! no SQL setter or writable token table. An ordinary connection denies by default.
use rusqlite::{Connection, functions::FunctionFlags};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use super::Result;

const FUNCTION: &str = "agentctl_completion_authorized";
// Safe under trusted_schema=OFF: this predicate only compares supplied values to
// an in-memory capability; it performs no SQL, I/O, or authorization mutation.
const FLAGS: FunctionFlags = FunctionFlags::SQLITE_UTF8.union(FunctionFlags::SQLITE_INNOCUOUS);

pub(in crate::local) fn register(c: &Connection) -> Result<()> {
    c.create_scalar_function(FUNCTION, 5, FLAGS, |_| Ok(false))?;
    Ok(())
}

pub(super) struct Permit(Arc<AtomicBool>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Call only after completion validation, inside its write transaction. Bind the
/// capability to the exact repository, plan, workspace, proof and final source.
/// Dropping it revokes authorization on success, error, or unwind, even if the
/// connection is reused. The SQL function itself cannot enable the capability.
pub(super) fn authorize(c: &Connection, expected: [String; 5]) -> Result<Permit> {
    let enabled = Arc::new(AtomicBool::new(true));
    let flag = enabled.clone();
    c.create_scalar_function(FUNCTION, 5, FLAGS, move |ctx| {
        if !flag.load(Ordering::SeqCst) {
            return Ok(false);
        }
        for (i, value) in expected.iter().enumerate() {
            if ctx.get::<String>(i)? != *value {
                return Ok(false);
            }
        }
        Ok(true)
    })?;
    Ok(Permit(enabled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_capability_is_connection_local_exact_and_revoked_on_error() {
        let c = Connection::open_in_memory().unwrap();
        let other = Connection::open_in_memory().unwrap();
        register(&c).unwrap();
        register(&other).unwrap();
        let expected = ["repo", "plan", "workspace", "proof", "source"].map(str::to_owned);
        let allowed = |c: &Connection, values: &[String; 5]| -> bool {
            c.query_row(
                "SELECT agentctl_completion_authorized(?1,?2,?3,?4,?5)",
                rusqlite::params_from_iter(values),
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(!allowed(&c, &expected));
        let attempt = || -> Result<()> {
            let _permit = authorize(&c, expected.clone())?;
            assert!(allowed(&c, &expected));
            assert!(!allowed(&other, &expected));
            for i in 0..5 {
                let mut wrong = expected.clone();
                wrong[i].push_str("-other");
                assert!(!allowed(&c, &wrong));
            }
            Err(super::super::Error::Invalid("injected".into()))
        };
        assert!(attempt().is_err());
        assert!(!allowed(&c, &expected));
        let permit = authorize(&c, expected.clone()).unwrap();
        assert!(allowed(&c, &expected));
        drop(permit);
        assert!(!allowed(&c, &expected));
    }
}
