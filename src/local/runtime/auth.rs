use super::*;
use rusqlite::functions::FunctionFlags;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
const NAME: &str = "agentctl_runtime_authorized";
const FLAGS: FunctionFlags = FunctionFlags::SQLITE_UTF8.union(FunctionFlags::SQLITE_INNOCUOUS);
pub(in crate::local) fn register(c: &Connection) -> Result<()> {
    c.create_scalar_function(NAME, 2, FLAGS, |_| Ok(false))?;
    c.create_scalar_function("agentctl_runtime_session_authorized", 3, FLAGS, |_| {
        Ok(false)
    })?;
    Ok(())
}
pub(super) struct Permit(Arc<AtomicBool>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
pub(super) fn authorize(
    c: &Connection,
    repo: &RepositoryId,
    owner: &str,
    session: &str,
) -> Result<Permit> {
    let active = Arc::new(AtomicBool::new(true));
    let permit = Permit(active.clone()); // revoke even if registering either predicate fails
    let flag = active.clone();
    let repo = repo.as_str().to_owned();
    let owner = owner.to_owned();
    let session_flag = active.clone();
    let session_repo = repo.clone();
    let session_owner = owner.clone();
    let session = session.to_owned();
    c.create_scalar_function(
        "agentctl_runtime_session_authorized",
        3,
        FLAGS,
        move |ctx| {
            Ok(session_flag.load(Ordering::SeqCst)
                && ctx.get::<String>(0)? == session_repo
                && ctx.get::<String>(1)? == session_owner
                && ctx.get::<String>(2)? == session)
        },
    )?;
    c.create_scalar_function(NAME, 2, FLAGS, move |ctx| {
        Ok(flag.load(Ordering::SeqCst)
            && ctx.get::<String>(0)? == repo
            && ctx.get::<String>(1)? == owner)
    })?;
    Ok(permit)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_capability_is_exact_connection_local_and_revoked_on_unwind() {
        let a = Connection::open_in_memory().unwrap();
        let b = Connection::open_in_memory().unwrap();
        register(&a).unwrap();
        register(&b).unwrap();
        let repo = RepositoryId::for_common_directory(Path::new("/fixture/.git"));
        let allowed = |c: &Connection, owner: &str| {
            c.query_row::<bool, _, _>(
                "SELECT agentctl_runtime_authorized(?1,?2)",
                params![repo.as_str(), owner],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(!allowed(&a, "plan:a"));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _permit = authorize(&a, &repo, "plan:a", "engineering:a").unwrap();
            assert!(
                !a.query_row::<bool, _, _>(
                    "SELECT agentctl_runtime_session_authorized(?1,'plan:a','engineering:b')",
                    [repo.as_str()],
                    |r| r.get(0)
                )
                .unwrap()
            );
            assert!(allowed(&a, "plan:a"));
            assert!(!allowed(&a, "plan:b"));
            assert!(!allowed(&b, "plan:a"));
            panic!("simulated controller crash");
        }));
        assert!(!allowed(&a, "plan:a"));
    }
}
