use anyhow::Result;
use once_cell::sync::OnceCell;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tonic::transport::Channel;

pub mod proto {
    pub mod public_api {
        pub mod v1 {
            tonic::include_proto!("public_api.v1");
        }
    }
}

use proto::public_api::v1::{
    GetAvatarUploadUrlRequest, GetAvatarUploadUrlResponse, GetMediaUploadUrlRequest,
    GetMediaUploadUrlResponse, GetProfileRequest, ProfileResponse, UpdateProfileRequest,
    UpdateProfileResponse, public_service_client::PublicServiceClient,
};

use crate::auth::{auth_get_bearer_token, auth_get_user_id};
use crate::transport::{SharedTransport, configure_shared_transport};

#[derive(Debug, Clone)]
struct CachedProfileEntry {
    user_id: String,
    profile: ProfileInfo,
}

/// Profile client for managing user profile (display_name, avatar)
#[derive(Clone)]
pub struct ProfileClient {
    state: Arc<ProfileClientState>,
}

struct ProfileClientState {
    transport: Arc<SharedTransport>,
    request_timeout: Duration,
}

impl ProfileClient {
    pub fn from_transport(transport: Arc<SharedTransport>) -> Self {
        let state = ProfileClientState {
            request_timeout: transport.request_timeout(),
            transport,
        };
        Self {
            state: Arc::new(state),
        }
    }

    async fn get_channel(&self) -> Result<Channel> {
        self.state
            .transport
            .get_channel()
            .await
            .map_err(|err| anyhow::anyhow!(err))
    }

    async fn authed_request<T>(&self, inner: T) -> Result<tonic::Request<T>> {
        let bearer_token = auth_get_bearer_token().await.ok_or_else(|| {
            anyhow::anyhow!(
                "Authentication bearer token is not configured — cannot call profile API"
            )
        })?;
        let mut req = tonic::Request::new(inner);
        crate::auth::auth_insert_bearer_header(&mut req, &bearer_token)
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(req)
    }

    /// Get the current user's profile
    pub async fn get_profile(&self) -> Result<ProfileResponse> {
        let request = self.authed_request(GetProfileRequest {}).await?;
        let channel = self.get_channel().await?;
        let mut client = PublicServiceClient::new(channel);
        let response = timeout(self.state.request_timeout, client.get_profile(request))
            .await
            .map_err(|_| anyhow::anyhow!("Get profile timed out"))??
            .into_inner();
        Ok(response)
    }

    /// Update the current user's profile
    pub async fn update_profile(
        &self,
        display_name: Option<String>,
        avatar_url: Option<String>,
    ) -> Result<UpdateProfileResponse> {
        let request = self
            .authed_request(UpdateProfileRequest {
                display_name,
                avatar_url,
            })
            .await?;
        let channel = self.get_channel().await?;
        let mut client = PublicServiceClient::new(channel);
        let response = timeout(self.state.request_timeout, client.update_profile(request))
            .await
            .map_err(|_| anyhow::anyhow!("Update profile timed out"))??
            .into_inner();
        Ok(response)
    }

    /// Get a signed upload URL for avatar image
    pub async fn get_avatar_upload_url(
        &self,
        file_extension: String,
    ) -> Result<GetAvatarUploadUrlResponse> {
        let request = self
            .authed_request(GetAvatarUploadUrlRequest { file_extension })
            .await?;
        let channel = self.get_channel().await?;
        let mut client = PublicServiceClient::new(channel);
        let response = timeout(
            self.state.request_timeout,
            client.get_avatar_upload_url(request),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Get avatar upload URL timed out"))??
        .into_inner();
        Ok(response)
    }

    /// Upload an avatar image to the signed URL and return the public URL
    pub async fn upload_avatar(
        &self,
        file_bytes: Vec<u8>,
        file_extension: String,
    ) -> Result<String> {
        let urls = self.get_avatar_upload_url(file_extension.clone()).await?;

        let content_type = crate::remi_uri::mime_from_extension(&file_extension);

        let http = reqwest::Client::new();
        let response = http
            .put(&urls.upload_url)
            .header("Content-Type", content_type)
            .body(file_bytes)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to upload avatar: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Avatar upload failed: {} - {}", status, body);
        }

        // Update profile with new avatar URL
        self.update_profile(None, Some(urls.public_url.clone()))
            .await?;

        Ok(urls.public_url)
    }

    /// Get a signed upload URL for media files (images, etc.)
    pub async fn get_media_upload_url(
        &self,
        file_extension: String,
        scenario: String,
    ) -> Result<GetMediaUploadUrlResponse> {
        let request = self
            .authed_request(GetMediaUploadUrlRequest {
                file_extension,
                scenario,
            })
            .await?;
        let channel = self.get_channel().await?;
        let mut client = PublicServiceClient::new(channel);
        let response = timeout(
            self.state.request_timeout,
            client.get_media_upload_url(request),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Get media upload URL timed out"))??
        .into_inner();
        Ok(response)
    }

    /// Upload a media file to Supabase Storage and return the public URL
    pub async fn upload_media(
        &self,
        file_bytes: Vec<u8>,
        file_extension: String,
        scenario: String,
    ) -> Result<String> {
        let urls = self
            .get_media_upload_url(file_extension.clone(), scenario)
            .await?;

        let content_type = crate::remi_uri::mime_from_extension(&file_extension);

        let http = reqwest::Client::new();
        let response = http
            .put(&urls.upload_url)
            .header("Content-Type", content_type)
            .body(file_bytes)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to upload media: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Media upload failed: {} - {}", status, body);
        }

        Ok(urls.public_url)
    }
}

// ========== Global singleton + FRB-exposed functions ==========

static PROFILE_CLIENT: OnceCell<Arc<RwLock<Option<Arc<ProfileClient>>>>> = OnceCell::new();
static PROFILE_CACHE: OnceCell<Arc<RwLock<Option<CachedProfileEntry>>>> = OnceCell::new();

fn profile_client_store() -> &'static Arc<RwLock<Option<Arc<ProfileClient>>>> {
    PROFILE_CLIENT.get_or_init(|| Arc::new(RwLock::new(None)))
}

fn profile_cache_store() -> &'static Arc<RwLock<Option<CachedProfileEntry>>> {
    PROFILE_CACHE.get_or_init(|| Arc::new(RwLock::new(None)))
}

fn profile_info_from_response(resp: ProfileResponse) -> ProfileInfo {
    ProfileInfo {
        user_id: resp.user_id,
        display_name: resp.display_name,
        avatar_url: resp.avatar_url,
        email: resp.email,
    }
}

fn profile_info_from_update_response(resp: UpdateProfileResponse) -> Result<ProfileInfo, String> {
    let profile = resp
        .profile
        .ok_or_else(|| "No profile in response".to_string())?;
    Ok(ProfileInfo {
        user_id: profile.user_id,
        display_name: profile.display_name,
        avatar_url: profile.avatar_url,
        email: profile.email,
    })
}

async fn profile_cache_get_for_current_user() -> Option<ProfileInfo> {
    let current_user_id = auth_get_user_id().await?;
    let guard = profile_cache_store().read().await;
    match guard.as_ref() {
        Some(entry) if entry.user_id == current_user_id => Some(entry.profile.clone()),
        _ => None,
    }
}

async fn profile_cache_set(profile: &ProfileInfo) {
    let mut guard = profile_cache_store().write().await;
    guard.replace(CachedProfileEntry {
        user_id: profile.user_id.clone(),
        profile: profile.clone(),
    });
}

async fn profile_cache_update_avatar_for_current_user(avatar_url: &str) {
    let Some(current_user_id) = auth_get_user_id().await else {
        return;
    };

    let mut guard = profile_cache_store().write().await;
    let Some(entry) = guard.as_mut() else {
        return;
    };

    if entry.user_id == current_user_id {
        entry.profile.avatar_url = avatar_url.to_string();
    }
}

pub async fn profile_clear_cache() {
    *profile_cache_store().write().await = None;
}

async fn get_profile_client() -> Result<Arc<ProfileClient>, String> {
    let guard = profile_client_store().read().await;
    guard
        .as_ref()
        .cloned()
        .ok_or_else(|| "Profile client is not configured".to_string())
}

pub async fn configure_profile_client(config_json: String) -> Result<(), String> {
    let transport = configure_shared_transport(&config_json).await?;
    let client = Arc::new(ProfileClient::from_transport(transport));
    let mut guard = profile_client_store().write().await;
    guard.replace(client);
    Ok(())
}

pub async fn profile_refresh() -> Result<ProfileInfo, String> {
    let client = get_profile_client().await?;
    let resp = client.get_profile().await.map_err(|e| e.to_string())?;
    let profile = profile_info_from_response(resp);
    profile_cache_set(&profile).await;
    Ok(profile)
}

/// FRB: Get user profile
pub async fn profile_get() -> Result<ProfileInfo, String> {
    if let Some(profile) = profile_cache_get_for_current_user().await {
        return Ok(profile);
    }

    profile_refresh().await
}

/// FRB: Update user profile
pub async fn profile_update(
    display_name: Option<String>,
    avatar_url: Option<String>,
) -> Result<ProfileInfo, String> {
    let client = get_profile_client().await?;
    let resp = client
        .update_profile(display_name, avatar_url)
        .await
        .map_err(|e| e.to_string())?;
    let profile = profile_info_from_update_response(resp)?;
    profile_cache_set(&profile).await;
    Ok(profile)
}

/// FRB: Upload avatar from bytes and return public URL
pub async fn profile_upload_avatar(
    file_bytes: Vec<u8>,
    file_extension: String,
) -> Result<String, String> {
    let client = get_profile_client().await?;
    let avatar_url = client
        .upload_avatar(file_bytes, file_extension)
        .await
        .map_err(|e| e.to_string())?;
    profile_cache_update_avatar_for_current_user(&avatar_url).await;
    Ok(avatar_url)
}

/// FRB: Upload media file (image, etc.) and return public URL
pub async fn media_upload(
    file_bytes: Vec<u8>,
    file_extension: String,
    scenario: Option<String>,
) -> Result<String, String> {
    let client = get_profile_client().await?;
    client
        .upload_media(file_bytes, file_extension, scenario.unwrap_or_default())
        .await
        .map_err(|e| e.to_string())
}

/// FRB: Get media upload URL (for manual upload)
pub async fn media_get_upload_url(
    file_extension: String,
    scenario: Option<String>,
) -> Result<MediaUploadInfo, String> {
    let client = get_profile_client().await?;
    let resp = client
        .get_media_upload_url(file_extension, scenario.unwrap_or_default())
        .await
        .map_err(|e| e.to_string())?;
    Ok(MediaUploadInfo {
        upload_url: resp.upload_url,
        public_url: resp.public_url,
    })
}

/// FRB: Get avatar upload URL (for manual upload)
pub async fn profile_get_avatar_upload_url(
    file_extension: String,
) -> Result<AvatarUploadInfo, String> {
    let client = get_profile_client().await?;
    let resp = client
        .get_avatar_upload_url(file_extension)
        .await
        .map_err(|e| e.to_string())?;
    Ok(AvatarUploadInfo {
        upload_url: resp.upload_url,
        public_url: resp.public_url,
    })
}

/// Profile info returned to Flutter via FRB
#[derive(Debug, Clone)]
pub struct ProfileInfo {
    pub user_id: String,
    pub display_name: String,
    pub avatar_url: String,
    pub email: String,
}

/// Avatar upload URL info returned to Flutter via FRB
#[derive(Debug, Clone)]
pub struct AvatarUploadInfo {
    pub upload_url: String,
    pub public_url: String,
}

/// Media upload URL info returned to Flutter via FRB
#[derive(Debug, Clone)]
pub struct MediaUploadInfo {
    pub upload_url: String,
    pub public_url: String,
}
