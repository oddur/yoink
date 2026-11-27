//! The screens: `app` runs the event loop and routes keys and
//! updates; every other module is one pane, with its state, its
//! refresh data and its render.

pub mod app;
pub mod audit;
pub mod container_detail;
pub mod dashboard;
pub mod doctor;
pub mod drift;
pub mod history;
pub mod host_detail;
pub mod hosts;
pub mod logs;
pub mod resources;
pub mod secrets;
pub mod services;
pub mod shell;
