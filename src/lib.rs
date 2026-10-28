//! pcstui — a native Rust client and TUI for Baidu Netdisk.
//!
//! The crate is deliberately split into small layers:
//!
//! * [`config`] — portable configuration, import/export, permissions.
//! * [`api`] — native HTTP API, signing and domain models.
//! * [`transfer`] — resumable downloads/uploads and progress events.
//! * [`tui`] — the interactive dual-pane interface.
//! * [`cli`] — headless commands for scripts and smoke tests.

pub mod api;
pub mod cli;
pub mod config;
pub mod lock;
pub mod transfer;
pub mod tui;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
