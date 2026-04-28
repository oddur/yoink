//! TUI mode. See `app` for the event loop and per-pane modules for
//! rendering.

mod app;
mod chrome;
mod container_detail;
mod dashboard;
mod drift;
mod editor;
mod history;
mod host_detail;
mod hosts;
mod logs;
mod progress;
mod resources;
mod secrets;
mod services;
mod shell;
mod ui;

pub use app::{Mode, run};
