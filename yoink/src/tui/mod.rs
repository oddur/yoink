//! TUI mode. See `app` for the event loop and per-pane modules for
//! rendering.

mod app;
mod dashboard;
mod host_detail;
mod hosts;
mod logs;
mod progress;
mod services;
mod ui;

pub use app::{Mode, run};
