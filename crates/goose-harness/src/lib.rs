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
pub mod ledger;
pub mod memory;

pub use config::{
    ActionClass, ActionClasses, DaemonConfig, DeliveryConfig, HarnessConfig, MemoryConfig,
};
pub use ledger::{
    DeliveryChannel, DeliveryReport, FileDeliveryChannel, NewTask, Task, TaskEvent, TaskStatus,
    TaskStore,
};
pub use memory::{
    consolidate, extract_candidates, ConsolidationReport, MemoryKind, MemoryRecord, MemoryStore,
    NewRecord, RecordStatus, WritePolicy,
};
