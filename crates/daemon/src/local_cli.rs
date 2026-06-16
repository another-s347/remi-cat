use anyhow::Result;
use im_feishu::FeishuMessage;
use std::io::Write as _;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, Mutex};

pub const CLI_CHANNEL: &str = "cli";
pub const CLI_CHAT_ID: &str = "local-dev";
pub const CLI_USER_ID: &str = "local-user";
pub const CLI_USERNAME: &str = "local-user";

#[derive(Debug, Clone)]
pub enum CliEvent {
    MessageReceived(FeishuMessage),
    ShutdownRequested,
}

#[derive(Clone, Default)]
pub struct CliGateway {
    next_message_id: Arc<AtomicU64>,
    stdout_lock: Arc<Mutex<()>>,
}

impl CliGateway {
    pub fn new() -> Self {
        Self {
            next_message_id: Arc::new(AtomicU64::new(1)),
            stdout_lock: Arc::new(Mutex::new(())),
        }
    }

    pub async fn start(&self) -> Result<mpsc::Receiver<CliEvent>> {
        let (tx, rx) = mpsc::channel(64);
        let gateway = self.clone();
        tokio::spawn(async move {
            let stdin = BufReader::new(tokio::io::stdin());
            let mut lines = stdin.lines();
            gateway
                .print_system(
                    "CLI IM ready. Type messages to chat, `/cancel` and `/acp ...` are supported, `quit` exits.",
                )
                .await;
            gateway.print_prompt().await;

            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let text = line.trim_end().to_string();
                        if text.trim().is_empty() {
                            gateway.print_prompt().await;
                            continue;
                        }
                        if matches!(text.trim(), "quit" | "exit") {
                            let _ = tx.send(CliEvent::ShutdownRequested).await;
                            break;
                        }
                        let message_id = format!(
                            "cli-msg-{}",
                            gateway.next_message_id.fetch_add(1, Ordering::Relaxed)
                        );
                        let event =
                            CliEvent::MessageReceived(Self::build_message(message_id, text));
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = tx.send(CliEvent::ShutdownRequested).await;
                        break;
                    }
                    Err(err) => {
                        gateway
                            .print_system(&format!("stdin read failed: {err:#}"))
                            .await;
                        let _ = tx.send(CliEvent::ShutdownRequested).await;
                        break;
                    }
                }
            }
        });
        Ok(rx)
    }

    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<()> {
        self.print_block("send", chat_id, text).await;
        Ok(())
    }

    pub async fn reply_text(&self, message_id: &str, text: &str) -> Result<()> {
        self.print_block("reply", message_id, text).await;
        Ok(())
    }

    pub async fn reply_card(&self, message_id: &str, markdown: &str) -> Result<()> {
        self.print_block("card", message_id, markdown).await;
        Ok(())
    }

    pub fn next_message(&self, text: impl Into<String>) -> FeishuMessage {
        let message_id = format!(
            "cli-msg-{}",
            self.next_message_id.fetch_add(1, Ordering::Relaxed)
        );
        Self::build_message(message_id, text.into())
    }

    pub fn begin_streaming_reply(&self, reply_to: &str) -> CliStreamingReply {
        CliStreamingReply::new(reply_to.to_string(), Arc::clone(&self.stdout_lock))
    }

    pub async fn print_system(&self, text: &str) {
        self.print_block("system", CLI_CHAT_ID, text).await;
    }

    async fn print_prompt(&self) {
        let _guard = self.stdout_lock.lock().await;
        print!("cli> ");
        let _ = std::io::stdout().flush();
    }

    async fn print_block(&self, kind: &str, target: &str, text: &str) {
        let _guard = self.stdout_lock.lock().await;
        println!("\n[{kind}:{target}]");
        println!("{text}");
        print!("cli> ");
        let _ = std::io::stdout().flush();
    }

    fn build_message(message_id: String, text: String) -> FeishuMessage {
        FeishuMessage {
            message_id,
            sender_user_id: CLI_USER_ID.to_string(),
            chat_id: CLI_CHAT_ID.to_string(),
            chat_type: "p2p".to_string(),
            text,
            images: Vec::new(),
            files: Vec::new(),
            documents: Vec::new(),
            parent_id: None,
            thread_id: None,
            at_bot: true,
            mentions: Vec::new(),
        }
    }
}

pub struct CliStreamingReply {
    reply_to: String,
    buffer: String,
    stdout_lock: Arc<Mutex<()>>,
}

impl CliStreamingReply {
    fn new(reply_to: String, stdout_lock: Arc<Mutex<()>>) -> Self {
        Self {
            reply_to,
            buffer: String::new(),
            stdout_lock,
        }
    }

    pub async fn push(&mut self, text: &str) -> Result<()> {
        self.buffer.push_str(text);
        Ok(())
    }

    pub async fn finish(&mut self) -> Result<()> {
        let _guard = self.stdout_lock.lock().await;
        println!("\n[agent:{}]", self.reply_to);
        if self.buffer.trim().is_empty() {
            println!("(empty reply)");
        } else {
            println!("{}", self.buffer);
        }
        print!("cli> ");
        let _ = std::io::stdout().flush();
        Ok(())
    }
}
