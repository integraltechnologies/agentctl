//! Ephemeral evidence from the controller's owned child handles, never a persisted
//! PID/timestamp heuristic. Separate observer processes intentionally see UNKNOWN.
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

type Key = [String; 7];
fn registry() -> &'static Mutex<BTreeMap<Key, Weak<AtomicBool>>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<Key, Weak<AtomicBool>>>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}
fn key(
    database: &str,
    repository: &str,
    workspace: &str,
    session: &str,
    agent: &str,
    job: &str,
) -> Key {
    // The creator PID namespaces *in-memory* evidence, including after fork. It
    // is not the child's persisted PID and is never used to probe an OS PID.
    [
        std::process::id().to_string(),
        database.into(),
        repository.into(),
        workspace.into(),
        session.into(),
        agent.into(),
        job.into(),
    ]
}
pub(super) struct Guard {
    key: Key,
    live: Arc<AtomicBool>,
}
impl Guard {
    pub(super) fn new(
        database: &str,
        repository: &str,
        workspace: &str,
        session: &str,
        agent: &str,
        job: &str,
    ) -> Self {
        let guard = Self {
            key: key(database, repository, workspace, session, agent, job),
            live: Arc::new(AtomicBool::new(false)),
        };
        if let Ok(mut entries) = registry().lock() {
            entries.insert(guard.key.clone(), Arc::downgrade(&guard.live));
        }
        guard
    }
    pub(super) fn set(&self, live: bool) {
        self.live.store(live, Ordering::Release);
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.set(false);
        if let Ok(mut entries) = registry().lock() {
            if entries
                .get(&self.key)
                .is_some_and(|entry| entry.ptr_eq(&Arc::downgrade(&self.live)))
            {
                entries.remove(&self.key);
            }
        }
    }
}
pub(crate) fn is_live(
    database: &str,
    repository: &str,
    workspace: &str,
    session: &str,
    agent: &str,
    job: &str,
) -> bool {
    registry()
        .try_lock()
        .ok()
        .and_then(|entries| {
            entries
                .get(&key(database, repository, workspace, session, agent, job))
                .and_then(Weak::upgrade)
        })
        .is_some_and(|live| live.load(Ordering::Acquire))
}
