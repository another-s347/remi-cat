//! CLI parsing boundary.
//!
//! The concrete Clap definitions still live in `app` during this split. New CLI
//! parsing and command conversion code should move here instead of growing
//! `app.rs`.

pub(crate) use crate::app::CliConfig;
