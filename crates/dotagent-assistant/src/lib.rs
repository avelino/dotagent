//! The conversational-assistant harness.
//!
//! The gateway is a harness for one message at a time; this crate is what
//! makes a sequence of messages feel like one conversation. It owns three
//! concerns that dispatcher agents used to reimplement in shell:
//!
//! - a **conversation registry**: pointers (model session id, generation,
//!   toolkit hash, transcript size) keyed by trigger source + session id —
//!   never message bodies, never transcripts;
//! - **memory hooks**: a bounded recall block before the run and `MEMO:`
//!   line capture from the reply after it;
//! - **toolkit provisioning**: assemble the agent's MCP config from the
//!   manifest and hash it, so a changed toolkit invalidates stale sessions
//!   deterministically.
//!
//! Purity follows the `dotagent-scheduler` convention: everything in
//! [`registry`], [`memo`] and [`toolkit`] is decision logic — no filesystem,
//! no clock, no network. IO lives at the edges (the state store and the
//! memory store, wired by the daemon); protocol wire parsing stays in
//! `dotagent-core::assistant`.

pub mod memo;
pub mod memory_hooks;
pub mod registry;
pub mod store;
pub mod toolkit;

pub use memo::{assemble_context_block, strip_memos, CapturedMemo, MemoryContext};
pub use memory_hooks::{flush_memos, recall_context_block};
pub use registry::{RegistryChange, RegistryRecord};
pub use store::{ensure_toolkit_file, RegistryStore, StoreError};
pub use toolkit::{build_mcp_config, toolkit_hash, ToolkitError, ToolkitServer};
