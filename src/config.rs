#![allow(unused_imports)]

//! Runtime configuration boundary.
//!
//! This module is the facade for profile, environment, and runtime config setup.
//! It re-exports the existing config module while the monolithic app startup is
//! being split.

pub(crate) use crate::runtime_config::*;
