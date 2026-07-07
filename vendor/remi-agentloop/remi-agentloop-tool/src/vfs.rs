//! Virtual-filesystem tools backed by `bashkit::InMemoryFs`.
//!
//! This module re-exports everything from [`crate::bkfs`] plus a
//! backward-compatible `VirtualFsTool` type alias for [`FsToolkit`].
//! Prefer importing `remi_tool::bkfs` directly for new code.

pub use crate::bkfs::{FsCreateTool, FsLsTool, FsReadTool, FsRemoveTool, FsToolkit, FsWriteTool};

/// Backward-compatible type alias for [`FsToolkit`].
///
/// The previous `VirtualFsTool` was a single multiplexed tool; the new
/// `FsToolkit` provides five focused tools instead. Use `.read()`,
/// `.write()`, `.mkdir()`, `.remove()`, and `.ls()` to obtain them.
pub type VirtualFsTool = FsToolkit;
