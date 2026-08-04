//! MCP server module for F* IDE.

pub mod tools;
pub mod transport;

pub use tools::{create_fstar_server, SESSION_MANAGER};
pub use transport::DiscoveryAwareStdioTransport;
