//! Shared spine for every kompressor surface.
//!
//! The HTTP API, the tool server and the proxy are three framings of the same
//! two operations: compress a conversation, and fetch back something a previous
//! compression elided. This crate owns those operations, the configuration that
//! shapes them, and the resolution of a credential into the tenant everything
//! durable is scoped by — so the surfaces are translation and nothing more, and
//! a rule added for one of them holds for all three.

#![forbid(unsafe_code)]

pub mod auth;
pub mod backup;
pub mod config;
pub mod core;
pub mod error;
pub mod retrieve;
pub mod runtime;

pub use auth::{Authenticator, Credentials, Identity};
pub use config::Config;
pub use core::{AppCore, CompressOutcome, RefsSweep, RequestInfo, build_ref_store};
pub use error::{AuthError, ConfigError, CoreError};
pub use retrieve::{RetrieveRange, Retrieved};

mod analysis_context;
