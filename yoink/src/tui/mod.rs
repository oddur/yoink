//! TUI mode. See `app` for the event loop and per-pane modules for
//! rendering.

mod app;
mod audit;
mod chrome;
mod container_detail;
mod dashboard;
mod doctor;
mod drift;
mod editor;
mod history;
mod host_detail;
mod hosts;
mod logs;
mod pf;
mod progress;
mod resources;
mod secrets;
mod services;
mod shell;
mod ui;
mod vscode;

pub use app::{Mode, run};
