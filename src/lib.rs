//! Provider-neutral contracts and a synchronous machine-local persistence substrate.

pub mod lifecycle;
pub mod local;
pub mod protocol;
pub mod schema;
pub mod validation;

pub use validation::{Validate, ValidationError};
