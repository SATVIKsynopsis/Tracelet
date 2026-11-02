mod grpc;
mod http;
pub mod store;

use std::net::SocketAddr;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub use store::{Captured, Payload, Protocol, Rejected, Signal, Store, Window, WindowError};

pub const EXPORTER_HEADER: &str = "x-otel-test-exporter";
pub const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("failed to bind capture listener: {0}")]
    Bind(#[source] std::io::Error),
}

#[derive(Debug)]
pub struct CaptureServer {
    store: Store,
    http_addr: SocketAddr,
    grpc_addr: SocketAddr,
    shutdown: Vec<oneshot::Sender<()>>,
    tasks: Vec<JoinHandle<()>>,
}

impl CaptureServer {
    pub async fn start() -> Result<Self, CaptureError> {
        let store = Store::default();
        let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(CaptureError::Bind)?;
        let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(CaptureError::Bind)?;
        let http_addr = http_listener.local_addr().map_err(CaptureError::Bind)?;
        let grpc_addr = grpc_listener.local_addr().map_err(CaptureError::Bind)?;

        let (http_tx, http_rx) = oneshot::channel::<()>();
        let router = http::router(store.clone());
        let http_task = tokio::spawn(async move {
            let serve = axum::serve(http_listener, router).with_graceful_shutdown(async {
                let _ = http_rx.await;
            });
            if let Err(e) = serve.await {
                tracing::error!(error = %e, "capture HTTP server failed");
            }
        });

        let (grpc_tx, grpc_rx) = oneshot::channel::<()>();
        let (traces, metrics, logs) = grpc::services(store.clone());
        let grpc_task = tokio::spawn(async move {
            let incoming = tonic::transport::server::TcpIncoming::from(grpc_listener);
            let serve = tonic::transport::Server::builder()
                .add_service(traces)
                .add_service(metrics)
                .add_service(logs)
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = grpc_rx.await;
                });
            if let Err(e) = serve.await {
                tracing::error!(error = %e, "capture gRPC server failed");
            }
        });

        tracing::debug!(%http_addr, %grpc_addr, "capture server started");
        Ok(CaptureServer {
            store,
            http_addr,
            grpc_addr,
            shutdown: vec![http_tx, grpc_tx],
            tasks: vec![http_task, grpc_task],
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn http_base_url(&self) -> String {
        format!("http://{}", self.http_addr)
    }

    pub fn grpc_endpoint(&self) -> String {
        self.grpc_addr.to_string()
    }

    pub async fn shutdown(mut self) {
        for tx in self.shutdown.drain(..) {
            let _ = tx.send(());
        }
        for t in self.tasks.drain(..) {
            let _ = t.await;
        }
    }
}

impl Drop for CaptureServer {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}
