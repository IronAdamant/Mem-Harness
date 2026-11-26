//! Configuration, the ownership table, project discovery and agent identity.
//!
//! The ownership table is the one place that says which tool owns a
//! feature that several tools implement. Each backend's `*_DISABLE`
//! environment value is computed from it (plus the backend's own extra
//! trims), so a decision is written once.

pub mod config;
pub mod identity;
pub mod project;

pub use config::{
    BackendSpec, Config, Ownership, Route, SectionSpec, StoreSpec, Trim, BASE_ENV, KNOWN_KINDS,
};
