//! Stage 0 provider-neutral contracts. No execution or persistence runtime.

pub mod lifecycle;
pub mod protocol;
pub mod schema;
pub mod validation;

pub use validation::{Validate, ValidationError};
