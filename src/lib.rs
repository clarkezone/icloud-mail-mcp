//! Bounded iCloud Mail reads and optional SMTP submission through Streamable HTTP MCP.
pub mod credentials;
pub mod error;
pub mod logging;
mod mail;
pub mod model;
pub mod sending;
pub mod server;
mod smtp;
mod state;
#[cfg(test)]
mod tests;
mod wire;
pub mod worker;
