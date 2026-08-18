use std::collections::HashMap;
use std::sync::Arc;

use a2a::{AgentCard, SendMessageRequest, StreamResponse};
use a2a_server::{AgentExecutor, ExecutorContext};
use anyhow::Context;
use futures::StreamExt;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};

use crate::a2a_channel::{stdio_agent_card, ApplicationA2aExecutor};
use crate::application::ApplicationHandle;

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StdioA2aRequest {
    AgentCard {
        id: String,
    },
    MessageStream {
        id: String,
        request: SendMessageRequest,
    },
    Cancel {
        id: String,
        task_id: String,
        context_id: String,
    },
    Shutdown {
        id: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StdioA2aResponse {
    AgentCard {
        id: String,
        card: AgentCard,
    },
    TaskAssigned {
        id: String,
        task_id: String,
        context_id: String,
    },
    Event {
        id: String,
        event: StreamResponse,
    },
    Done {
        id: String,
    },
    Error {
        id: String,
        message: String,
    },
}

pub(crate) async fn read_frame<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
) -> anyhow::Result<Option<T>> {
    let mut length = [0_u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("reading A2A stdio frame length"),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        anyhow::bail!("invalid A2A stdio frame length {length}");
    }
    let mut payload = vec![0_u8; length];
    reader
        .read_exact(&mut payload)
        .await
        .context("reading A2A stdio frame payload")?;
    Ok(Some(
        serde_json::from_slice(&payload).context("decoding A2A stdio frame")?,
    ))
}

pub(crate) async fn write_frame<T: Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(value).context("encoding A2A stdio frame")?;
    if payload.len() > MAX_FRAME_BYTES {
        anyhow::bail!("A2A stdio frame exceeds {MAX_FRAME_BYTES} bytes");
    }
    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Serve an embedded application through Remi's framed A2A stdio binding.
pub async fn serve_application(application: ApplicationHandle) -> anyhow::Result<()> {
    let card = stdio_agent_card(application.profile());
    let executor = ApplicationA2aExecutor::new(application);
    let mut input = tokio::io::stdin();
    let output = Arc::new(Mutex::new(tokio::io::stdout()));
    let (responses, mut response_rx) = mpsc::unbounded_channel::<StdioA2aResponse>();
    let writer_output = Arc::clone(&output);
    let writer = tokio::spawn(async move {
        while let Some(response) = response_rx.recv().await {
            write_frame(&mut *writer_output.lock().await, &response).await?;
        }
        Ok::<_, anyhow::Error>(())
    });

    while let Some(request) = read_frame::<StdioA2aRequest>(&mut input).await? {
        match request {
            StdioA2aRequest::AgentCard { id } => {
                let _ = responses.send(StdioA2aResponse::AgentCard {
                    id,
                    card: card.clone(),
                });
            }
            StdioA2aRequest::MessageStream { id, request } => {
                let executor = executor.clone();
                let responses = responses.clone();
                tokio::spawn(async move {
                    let task_id = uuid::Uuid::new_v4().to_string();
                    let context_id = request
                        .message
                        .context_id
                        .clone()
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    let _ = responses.send(StdioA2aResponse::TaskAssigned {
                        id: id.clone(),
                        task_id: task_id.clone(),
                        context_id: context_id.clone(),
                    });
                    let context = ExecutorContext {
                        message: Some(request.message),
                        task_id,
                        stored_task: None,
                        context_id,
                        metadata: request.metadata,
                        user: None,
                        service_params: HashMap::new(),
                        tenant: request.tenant,
                    };
                    let mut stream = executor.execute(context);
                    while let Some(event) = stream.next().await {
                        match event {
                            Ok(event) => {
                                if responses
                                    .send(StdioA2aResponse::Event {
                                        id: id.clone(),
                                        event,
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Err(error) => {
                                let _ = responses.send(StdioA2aResponse::Error {
                                    id: id.clone(),
                                    message: error.to_string(),
                                });
                                return;
                            }
                        }
                    }
                    let _ = responses.send(StdioA2aResponse::Done { id });
                });
            }
            StdioA2aRequest::Cancel {
                id,
                task_id,
                context_id,
            } => {
                let executor = executor.clone();
                let responses = responses.clone();
                tokio::spawn(async move {
                    let context = ExecutorContext {
                        message: None,
                        task_id,
                        stored_task: None,
                        context_id,
                        metadata: None,
                        user: None,
                        service_params: HashMap::new(),
                        tenant: None,
                    };
                    let mut stream = executor.cancel(context);
                    while let Some(event) = stream.next().await {
                        match event {
                            Ok(event) => {
                                let _ = responses.send(StdioA2aResponse::Event {
                                    id: id.clone(),
                                    event,
                                });
                            }
                            Err(error) => {
                                let _ = responses.send(StdioA2aResponse::Error {
                                    id: id.clone(),
                                    message: error.to_string(),
                                });
                                return;
                            }
                        }
                    }
                    let _ = responses.send(StdioA2aResponse::Done { id });
                });
            }
            StdioA2aRequest::Shutdown { id } => {
                let _ = responses.send(StdioA2aResponse::Done { id });
                break;
            }
        }
    }
    drop(responses);
    writer.await.context("joining A2A stdio writer")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_round_trip() {
        let request = StdioA2aRequest::AgentCard { id: "1".into() };
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        write_frame(&mut writer, &request).await.unwrap();
        let decoded = read_frame::<StdioA2aRequest>(&mut reader)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(decoded, StdioA2aRequest::AgentCard { id } if id == "1"));
    }
}
