#![cfg(feature = "experimental-pty")]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use bot_core::sandbox::{NoSandbox, Sandbox};
use bot_core::tools::{SecretRedactor, WorkspaceBashTool};
use remi_agentloop::prelude::{CancellationToken, Tool};

#[test]
fn feature_exposes_explicit_pty_arguments() {
    let root = tempfile::tempdir().unwrap();
    let tool = WorkspaceBashTool::new(
        Arc::new(NoSandbox::new(root.path().to_path_buf())),
        Arc::new(RwLock::new(SecretRedactor::empty())),
    );
    let schema = tool.parameters_schema();

    assert_eq!(schema["properties"]["experimental_pty"]["type"], "boolean");
    assert_eq!(schema["properties"]["pty_rows"]["default"], 24);
    assert_eq!(schema["properties"]["pty_cols"]["default"], 80);
}

#[tokio::test]
async fn allocates_a_tty_and_vt100_renders_the_current_screen() {
    let root = tempfile::tempdir().unwrap();
    let sandbox = NoSandbox::new(root.path().to_path_buf());
    let mut process = sandbox
        .bash_pty(
            "[ -t 1 ] || exit 9; printf 'first\\033[2K\\rsecond\\n'",
            24,
            80,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut parser = vt100::Parser::new(24, 80, 0);
    let code = tokio::time::timeout(Duration::from_secs(3), async {
        let mut output_open = true;
        loop {
            tokio::select! {
                chunk = process.output_rx.recv(), if output_open => {
                    match chunk {
                        Some(chunk) => parser.process(&chunk),
                        None => output_open = false,
                    }
                }
                result = &mut process.exit_rx => {
                    break result.unwrap().unwrap();
                }
            }
        }
    })
    .await
    .expect("PTY process should exit");
    while let Ok(chunk) = process.output_rx.try_recv() {
        parser.process(&chunk);
    }

    assert_eq!(code, 0);
    let screen = parser.screen().contents();
    assert!(
        screen.ends_with("second"),
        "rendered screen was: {screen:?}"
    );
    assert!(!screen.contains("first"), "rendered screen was: {screen:?}");
}

#[tokio::test]
async fn cancellation_kills_the_pty_child() {
    let root = tempfile::tempdir().unwrap();
    let sandbox = NoSandbox::new(root.path().to_path_buf());
    let cancel = CancellationToken::new();
    let process = sandbox
        .bash_pty("sleep 30", 24, 80, cancel.clone())
        .await
        .unwrap();
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(3), process.exit_rx)
        .await
        .expect("cancelled PTY process should exit")
        .expect("PTY exit channel should remain open");
    assert!(result.is_ok());
}
