#![cfg(not(feature = "experimental-pty"))]

use std::sync::{Arc, RwLock};

use bot_core::sandbox::NoSandbox;
use bot_core::tools::{SecretRedactor, WorkspaceBashTool};
use remi_agentloop::prelude::Tool;

#[test]
fn default_build_does_not_expose_experimental_pty_arguments() {
    let root = tempfile::tempdir().unwrap();
    let tool = WorkspaceBashTool::new(
        Arc::new(NoSandbox::new(root.path().to_path_buf())),
        Arc::new(RwLock::new(SecretRedactor::empty())),
    );
    let schema = tool.parameters_schema();

    assert!(schema["properties"].get("experimental_pty").is_none());
    assert!(schema["properties"].get("pty_rows").is_none());
    assert!(schema["properties"].get("pty_cols").is_none());
}
