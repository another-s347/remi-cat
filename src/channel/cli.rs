use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use bot_core::CatEvent;
use futures::StreamExt;
use im_feishu::FeishuMessage;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::app::CliConfig;
use crate::channel::feishu::collect_cli_bot_reply;
use crate::channel::{Channel, ChannelKind};
use crate::core::{ChatChannel, ChatRequest, CoreChatEvent, Runtime};

pub(crate) struct CliChannel {
    config: CliConfig,
}

pub(crate) async fn run_cli(runtime: Rc<Runtime>, cli: CliConfig) -> anyhow::Result<()> {
    println!(
        "CLI IM ready. channel=`{}` user=`{}`. Type messages to chat, `quit` exits.",
        cli.channel_id, cli.user_id
    );
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    while let Some(line) = lines.next_line().await? {
        let text = line.trim().to_string();
        if text.is_empty() {
            continue;
        }
        if matches!(text.as_str(), "quit" | "exit") {
            break;
        }
        process_cli_message(Rc::clone(&runtime), &cli, text).await?;
    }
    Ok(())
}

pub(crate) async fn process_cli_message(
    runtime: Rc<Runtime>,
    cli: &CliConfig,
    text: String,
) -> anyhow::Result<()> {
    let msg = FeishuMessage {
        message_id: format!("cli-msg-{}", uuid::Uuid::new_v4()),
        sender_user_id: cli.user_id.clone(),
        chat_id: cli.channel_id.clone(),
        chat_type: "p2p".to_string(),
        text,
        images: Vec::new(),
        files: Vec::new(),
        documents: Vec::new(),
        parent_id: None,
        thread_id: None,
        at_bot: true,
        mentions: Vec::new(),
    };
    let reply = collect_cli_bot_reply(runtime, msg, Some(cli.username.clone())).await?;
    println!("{reply}");
    Ok(())
}

pub(crate) async fn process_prompt_message(
    runtime: Rc<Runtime>,
    cli: &CliConfig,
    text: String,
) -> anyhow::Result<()> {
    let session_id = runtime.sessions.lock().await.resolve_channel(
        "prompt",
        &cli.channel_id,
        &runtime.root_agent_id,
    )?;
    let mut stream = std::pin::pin!(Rc::clone(&runtime).chat(ChatRequest::text(
        session_id,
        ChatChannel::Cli,
        text,
    )));
    let mut output = String::new();
    let timeout = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            event = stream.next() => {
                let Some(event) = event else { break };
                match event {
                    CoreChatEvent::Prefix(prefix) | CoreChatEvent::Reply(prefix) => {
                        print!("{prefix}");
                        io::stdout().flush()?;
                        output.push_str(&prefix);
                    }
                    CoreChatEvent::Bot(CatEvent::Text(delta)) => {
                        print!("{delta}");
                        io::stdout().flush()?;
                        output.push_str(&delta);
                    }
                    CoreChatEvent::Bot(CatEvent::Error(err)) => anyhow::bail!(err.to_string()),
                    CoreChatEvent::Bot(CatEvent::Done) | CoreChatEvent::Done => break,
                    _ => {}
                }
            }
            _ = &mut timeout => {
                anyhow::bail!("prompt timed out");
            }
        }
    }
    if !output.ends_with('\n') {
        println!();
    }
    Ok(())
}

impl CliChannel {
    pub(crate) fn new(config: CliConfig) -> Self {
        Self { config }
    }
}

impl Channel for CliChannel {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Cli
    }

    fn run<'a>(
        &'a self,
        runtime: Rc<Runtime>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
        let config = self.config.clone();
        Box::pin(async move { run_cli(runtime, config).await })
    }
}
