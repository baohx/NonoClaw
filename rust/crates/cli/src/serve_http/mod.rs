//! HTTP + WebSocket application shell.
//!
//! The public [`serve`] entrypoint is preserved while implementation details
//! are owned by responsibility-focused submodules.

mod connection;
mod http_error;
mod project_service;
mod protocol;
mod run_handler;
mod session_hub;
mod speech_service;
mod static_service;
mod upload_service;

pub use connection::serve;
