//! Quarkdrive desktop GUI — a native egui client for a Quarkdrive server.
//!
//! Split as a library so the headless integration tests can drive the real
//! app state (`app::App`) through egui's `Context::run` without opening a
//! window; the `main.rs` binary only wires that same app into eframe.

pub mod api;
pub mod app;
pub mod ui;
