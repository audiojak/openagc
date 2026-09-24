//! OpenAGC's MCP surface (spec §10): the tool catalog agents see, and the
//! private socket protocol that carries tool calls from the `openagc-mcp`
//! shim (spawned by the agent CLI) to the core inside the app.
//!
//! ```text
//! agent CLI ──stdio/MCP──▶ openagc-mcp ──unix socket──▶ core ──▶ ToolHandler
//! ```
//!
//! The shim is stateless: it lists the catalog and forwards each call with
//! the session id it was started with. Everything that matters — the
//! permission decision, scope, caps, approvals, the store — happens in the
//! core behind [`ToolHandler`].

pub mod catalog;
pub mod client;
pub mod server;
pub mod wire;

pub use catalog::{ToolSpec, catalog};
pub use client::ShimClient;
pub use server::{McpSocket, ToolHandler};
pub use wire::{Outcome, PROTOCOL_VERSION};
