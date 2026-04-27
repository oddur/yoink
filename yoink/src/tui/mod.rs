//! TUI mode. See `app` for the event loop and per-pane modules for
//! rendering.

mod app;
mod container_detail;
mod dashboard;
mod history;
mod host_detail;
mod hosts;
mod logs;
mod progress;
mod secrets;
mod services;
mod shell;
mod ui;

pub use app::{Mode, run};
