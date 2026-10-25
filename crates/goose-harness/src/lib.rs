//! Principal layer for WarMachine: persistent memory, a durable task
//! ledger, a proactive daemon, asynchronous approvals, managed
//! connectors, and worker delegation.
//!
//! Phase 0 delivers only the configuration schema (see [`config`]) and
//! the design decisions recorded in
//! `documentation/docs/goose-architecture/harness-design.md`. Each
//! capability lands behind the core crate's `harness` feature in its
//! own phase, gated by the project's acceptance criteria.

pub mod config;

pub use config::{
    ActionClass, ActionClasses, DaemonConfig, DeliveryConfig, HarnessConfig, MemoryConfig,
};
