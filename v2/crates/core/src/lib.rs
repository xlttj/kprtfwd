//! Domain types, state derivation, validation and storage for kprtfwd.
//!
//! This crate has no async or Kubernetes dependencies, so everything in it can
//! be tested without a cluster.

pub mod filter;
pub mod model;
pub mod naming;
pub mod paths;
pub mod state;
pub mod store;
pub mod svctype;
pub mod validate;

pub use model::{Forward, ForwardId, Project};
pub use state::{ContextState, ForwardState};
pub use store::{Store, StoreError};
pub use validate::ValidationError;
