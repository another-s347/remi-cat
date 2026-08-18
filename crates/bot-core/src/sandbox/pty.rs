use std::io::Read;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use remi_agentloop::prelude::CancellationToken;
use tokio::sync::{mpsc, oneshot};

pub struct PtyProcess {
    pub output_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    pub exit_rx: oneshot::Receiver<Result<u32, String>>,
}

pub fn spawn(
    command: CommandBuilder,
    rows: u16,
    cols: u16,
    cancel: CancellationToken,
) -> Result<PtyProcess> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening PTY")?;
    let mut reader = pair
        .master
        .try_clone_reader()
        .context("cloning PTY reader")?;
    let mut child = pair
        .slave
        .spawn_command(command)
        .context("spawning command in PTY")?;
    drop(pair.slave);

    let mut killer = child.clone_killer();
    let watcher_done = CancellationToken::new();
    let watcher_done_for_task = watcher_done.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = killer.kill();
            }
            _ = watcher_done_for_task.cancelled() => {}
        }
    });

    let (output_tx, output_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut buffer = vec![0; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if output_tx.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let (exit_tx, exit_rx) = oneshot::channel();
    std::thread::spawn(move || {
        let result = child
            .wait()
            .map(|status| status.exit_code())
            .map_err(|error| error.to_string());
        let _ = exit_tx.send(result);
        watcher_done.cancel();
    });

    Ok(PtyProcess { output_rx, exit_rx })
}
