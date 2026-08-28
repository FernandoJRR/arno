//! Home Server Harness — Part B core (SPEC §5.2).
//!
//! Library root so integration tests exercise the real router/stores; the
//! binary in `main.rs` only wires config → runtime → serve.

pub mod api;
pub mod audit;
pub mod config;
pub mod mcp;
pub mod model;
pub mod orchestrator;
pub mod policy;
pub mod queue;
pub mod state;
pub mod stores;
