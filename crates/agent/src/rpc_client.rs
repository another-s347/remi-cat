use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use bot_core::im_tools::{
	DownloadedImFile, ImDownloadRequest, ImFileBridge, ImUploadRequest, UploadedImFile,
};
use remi_proto::{AgentMessage, AgentPayload, ImBridgeRequest, ImBridgeResponse};
use tokio::sync::{mpsc, oneshot, Mutex};
use uuid::Uuid;

#[derive(Clone)]
pub struct GrpcImFileBridge {
	tx: mpsc::Sender<AgentMessage>,
	pending: Arc<Mutex<HashMap<String, oneshot::Sender<ImBridgeResponse>>>>,
}

impl GrpcImFileBridge {
	pub fn new(tx: mpsc::Sender<AgentMessage>) -> Self {
		Self {
			tx,
			pending: Arc::new(Mutex::new(HashMap::new())),
		}
	}

	pub async fn handle_response(&self, response: ImBridgeResponse) {
		if let Some(tx) = self.pending.lock().await.remove(&response.request_id) {
			let _ = tx.send(response);
		}
	}

	async fn request(
		&self,
		reply_to_message_id: &str,
		payload: remi_proto::im_bridge_request::Payload,
	) -> Result<ImBridgeResponse> {
		let request_id = Uuid::new_v4().to_string();
		let (tx, rx) = oneshot::channel();
		self.pending.lock().await.insert(request_id.clone(), tx);

		let send_result = self
			.tx
			.send(AgentMessage {
				reply_to_message_id: reply_to_message_id.to_string(),
				payload: Some(AgentPayload::ImBridgeRequest(ImBridgeRequest {
					request_id: request_id.clone(),
					payload: Some(payload),
				})),
			})
			.await;
		if let Err(err) = send_result {
			self.pending.lock().await.remove(&request_id);
			return Err(anyhow!("send IM bridge request failed: {err}"));
		}

		rx.await.context("IM bridge response channel closed")
	}
}

impl ImFileBridge for GrpcImFileBridge {
	fn download<'a>(
		&'a self,
		request: ImDownloadRequest,
	) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<DownloadedImFile>> + Send + 'a>> {
		Box::pin(async move {
			let reply_to = request.message_id.clone();
			let response = self
				.request(
					&reply_to,
					remi_proto::im_bridge_request::Payload::Download(remi_proto::ImDownloadRequest {
						platform: request.platform,
						message_id: request.message_id,
						chat_id: request.chat_id,
						attachment_key: request.attachment_key.unwrap_or_default(),
						document_url: request.document_url.unwrap_or_default(),
						file_type: request.file_type,
					}),
				)
				.await?;

			if !response.error.is_empty() {
				return Err(anyhow!(response.error));
			}
			match response.payload {
				Some(remi_proto::im_bridge_response::Payload::Downloaded(file)) => Ok(DownloadedImFile {
					file_name: file.file_name,
					mime_type: file.mime_type,
					content: file.content,
					source_label: file.source_label,
				}),
				Some(_) => Err(anyhow!("unexpected IM bridge response payload for download")),
				None => Err(anyhow!("missing IM bridge download payload")),
			}
		})
	}

	fn upload<'a>(
		&'a self,
		request: ImUploadRequest,
	) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<UploadedImFile>> + Send + 'a>> {
		Box::pin(async move {
			let reply_to = request.message_id.clone();
			let response = self
				.request(
					&reply_to,
					remi_proto::im_bridge_request::Payload::Upload(remi_proto::ImUploadRequest {
						platform: request.platform,
						message_id: request.message_id,
						chat_id: request.chat_id,
						file_name: request.file_name,
						mime_type: request.mime_type,
						content: request.content,
						file_type: request.file_type,
					}),
				)
				.await?;

			if !response.error.is_empty() {
				return Err(anyhow!(response.error));
			}
			match response.payload {
				Some(remi_proto::im_bridge_response::Payload::Uploaded(file)) => Ok(UploadedImFile {
					file_name: file.file_name,
					file_key: file.file_key,
					message_id: file.message_id,
					resource_url: file.resource_url,
				}),
				Some(_) => Err(anyhow!("unexpected IM bridge response payload for upload")),
				None => Err(anyhow!("missing IM bridge upload payload")),
			}
		})
	}
}
