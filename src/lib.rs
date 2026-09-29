/// Test builds only: ends the process at the boundary named, as if
/// agentctl died there, when the test running it asked it to (see
/// `recovery::tests`). Compiles to nothing otherwise.
macro_rules! failpoint {
    ($($name:tt)+) => {
        #[cfg(test)]
        $crate::recovery::tests::failpoint(&format!($($name)+));
        #[cfg(not(test))]
        let _ = || format!($($name)+);
    };
}

pub mod acceptance;
pub mod config;
pub mod executor;
pub mod graph;
pub mod init;
pub mod integration;
pub mod planner;
pub mod platform;
pub mod procd;
pub mod project;
pub mod recovery;
pub mod runtime;
pub mod scheduler;
pub mod source;
pub mod state;
pub mod verifier;
