//! Nexus API client module.
//!
//! Provides the HTTP client for communicating with the Nexus API.

mod client;
mod types;

pub use client::{NexusClient, DISPATCH_LIST_LIMIT, MAX_RESPONSE_BYTES};
pub use types::*;
