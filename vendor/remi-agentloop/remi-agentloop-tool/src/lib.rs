#[cfg(feature = "tool-bash")]
pub mod bash;

#[cfg(feature = "tool-fs")]
pub mod fs;

#[cfg(feature = "tool-fs-virtual")]
pub mod bkfs;

#[cfg(feature = "tool-fs-virtual")]
pub mod vfs;

#[cfg(feature = "tool-bash-virtual")]
pub mod bashkit;

// Re-exports for convenience
#[cfg(feature = "tool-bash")]
pub use bash::BashTool;

#[cfg(feature = "tool-fs")]
pub use fs::{
    LocalFsCreateTool, LocalFsLsTool, LocalFsReadTool, LocalFsRemoveTool, LocalFsToolkit,
    LocalFsWriteTool,
};

#[cfg(feature = "tool-fs-virtual")]
pub use bkfs::{FsCreateTool, FsLsTool, FsReadTool, FsRemoveTool, FsToolkit, FsWriteTool};

#[cfg(feature = "tool-fs-virtual")]
pub use vfs::VirtualFsTool;

#[cfg(feature = "tool-bash-virtual")]
pub use bashkit::{FsMode, Mount, MountSource, VirtualBashTool};
