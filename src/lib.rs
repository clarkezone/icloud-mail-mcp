//! Bounded, read-only iCloud Mail access through Streamable HTTP MCP.
pub mod credentials;
pub mod error;
pub mod logging;
mod mail;
pub mod model;
pub mod server;
mod state;
#[cfg(test)]
mod tests;
mod wire;
pub mod worker;
