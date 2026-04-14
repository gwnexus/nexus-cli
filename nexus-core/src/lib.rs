//! Nexus Core library crate.
//!
//! Provides shared modules for the Nexus CLI:
//! - `api` -- HTTP client and API types
//! - `auth` -- Credential storage and token validation
//! - `config` -- CLI configuration management
//! - `error` -- Unified error types

pub mod api;
pub mod auth;
pub mod config;
pub mod error;

// Re-exports for convenience
pub use error::Error;
pub use config::OutputPreference;
pub use config::McpSource;

/// Convenience type alias for results using the nexus-core error type.
pub type Result<T> = std::result::Result<T, Error>;
