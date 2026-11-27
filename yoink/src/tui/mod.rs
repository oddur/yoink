//! TUI mode. `screens` holds the event loop (`app`) and one module
//! per pane; the modules here are the helpers the screens share.

mod chrome;
mod editor;
mod pf;
mod progress;
mod screens;
mod ui;
mod vscode;

pub use screens::app::{Mode, run};
