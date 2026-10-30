//! Domain types, validation and storage for kprtfwd.
//!
//! This crate has no async or Kubernetes dependencies, so everything in it can
//! be tested without a cluster.

pub mod model;
pub mod paths;
pub mod store;
pub mod validate;

pub use model::{Forward, ForwardId, Project};
pub use store::{Store, StoreError};
pub use validate::ValidationError;
