use std::{
    future::Future,
    io,
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use http::Uri;
use http_body_util::Full;
use hyper_util::{
    client::legacy::{
        connect::{Connected, Connection},
        Client,
    },
    rt::{TokioExecutor, TokioIo},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tower::Service;
use weaver_core::{ClientAddr, VirtualName};
use weaver_crypto::NetworkRootPublic;
use weaver_net::{NetworkHandle, VirtualTcpStream};
use weaver_store::{EncryptedFileSecretStore, RedbStateStore};

pub(crate) fn production_open_options(
    root: NetworkRootPublic,
    data_dir: &Path,
    master_key: [u8; 32],
) -> anyhow::Result<weaver_net::NetworkHandleOpenOptions> {
    Ok(weaver_net::NetworkHandleOpenOptions {
        root,
        state_store: Arc::new(RedbStateStore::open(data_dir.join("state.redb"))?),
        secret_store: Arc::new(EncryptedFileSecretStore::open(
            data_dir.join("secrets"),
            master_key,
        )?),
        config_sync: Default::default(),
        presence_sync: Default::default(),
        allow_insecure_test_stores: false,
    })
}

#[derive(Clone)]
pub(crate) struct WeaverHttpConnector {
    network: Arc<NetworkHandle>,
    source: ClientAddr,
}

pub(crate) struct WeaverHttpConnection(VirtualTcpStream);

impl Connection for WeaverHttpConnection {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl AsyncRead for WeaverHttpConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for WeaverHttpConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl WeaverHttpConnector {
    pub(crate) fn new(network: Arc<NetworkHandle>, source: ClientAddr) -> Self {
        Self { network, source }
    }

    async fn connect_uri(&self, uri: Uri) -> io::Result<TokioIo<WeaverHttpConnection>> {
        if uri.scheme_str() != Some("http") || uri.port().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Weaver HTTP URI must use http and must not contain a port",
            ));
        }
        let host = uri
            .host()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing virtual host"))?;
        let name = VirtualName::new(host)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        self.network
            .connect_tcp_name(self.source, &name)
            .await
            .map(|stream| {
                tracing::info!(host, paths = ?stream.transport_paths(), "connected Weaver HTTP stream");
                TokioIo::new(WeaverHttpConnection(stream))
            })
            .map_err(io::Error::other)
    }
}

impl Service<Uri> for WeaverHttpConnector {
    type Response = TokioIo<WeaverHttpConnection>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let connector = self.clone();
        Box::pin(async move { connector.connect_uri(uri).await })
    }
}

pub(crate) type WeaverHttpClient = Client<WeaverHttpConnector, Full<Bytes>>;

pub(crate) fn http1_client(connector: WeaverHttpConnector) -> WeaverHttpClient {
    Client::builder(TokioExecutor::new()).build(connector)
}
