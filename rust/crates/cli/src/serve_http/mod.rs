//! HTTP + WebSocket application shell.
//!
//! The public [`serve`] entrypoint is preserved while implementation details
//! are owned by responsibility-focused submodules.

mod api_log_service;
mod connection;
mod download_service;
mod dream;
mod evolution;
mod fork_api;
mod http_error;
mod permission_api;
pub(crate) mod project_context;
mod project_service;
mod protocol;
mod run_api;
mod run_handler;
mod session_hub;
mod speech_service;
mod static_service;
mod upload_service;

pub use connection::serve;
