//! Adapter layer: SQLite repository, LHDN HTTP client, Axum routes.
//!
//! Each module is the *only* place its respective IO concern lives. The
//! domain crate has no knowledge of any of them.

pub mod api;
pub mod lhdn;
pub mod repo;
