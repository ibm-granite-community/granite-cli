//! Generic lifecycle for a process-scoped background HTTP server: bind an
//! ephemeral localhost port, serve an `axum::Router` on it for the duration
//! of one launch, then shut it down. Extracted from the usage-tracking proxy
//! (`crate::proxy::server::ProxyServer`) so any other in-process sub-server
//! -- e.g. an MCP server serving the launched process over HTTP -- can reuse
//! the same bind/spawn/shutdown mechanics instead of re-implementing them.

// Standard
use std::net::SocketAddr;

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use axum::Router;
use tokio::sync::oneshot;

use_channel!("SBSRV");

/// A running background HTTP server bound to an ephemeral localhost port.
pub(crate) struct SubServer {
    pub(crate) local_addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join_handle: tokio::task::JoinHandle<()>,
}

impl SubServer {
    /// Bind an ephemeral localhost port and start serving `router` on it.
    ///
    /// Synchronous: binds via a plain OS `std::net::TcpListener` call and
    /// hands it to `tokio::spawn`, which only needs an *ambient* Tokio
    /// runtime, not an `.await` -- this lets callers start a server from
    /// inside sync code (e.g. `Capability::bind` or `ConfigConstructable::new`)
    /// as long as a runtime is already running somewhere up the call stack.
    ///
    /// `label` is used only for log messages if the server errors.
    pub(crate) fn spawn(router: Router, label: &str) -> anyhow::Result<Self> {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        std_listener.set_nonblocking(true)?;
        let local_addr = std_listener.local_addr()?;
        let listener = tokio::net::TcpListener::from_std(std_listener)?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let label = label.to_string();

        let join_handle = tokio::spawn(async move {
            let server = axum::serve(listener, router).with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            });
            if let Err(e) = server.await {
                alog_channel!(MessageLevel::Warning, "sub-server '{label}' error: {e}");
            }
        });

        Ok(Self {
            local_addr,
            shutdown_tx: Some(shutdown_tx),
            join_handle,
        })
    }

    /// Signal the server to stop accepting new connections and wait for it
    /// to finish draining in-flight ones.
    pub(crate) async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.join_handle.await;
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;

    #[tokio::test]
    async fn spawn_serves_the_given_router_and_shutdown_frees_the_port() {
        let router = Router::new().route("/ping", get(|| async { "pong" }));
        let server = SubServer::spawn(router, "test").unwrap();
        let addr = server.local_addr;

        let resp = reqwest::get(format!("http://{addr}/ping")).await.unwrap();
        assert!(resp.status().is_success());
        assert_eq!(resp.text().await.unwrap(), "pong");

        server.shutdown().await;

        // The port should be free again. On macOS a child process that a
        // parallel test starts at this moment can hold the port for a few
        // milliseconds, so retry for up to a second.
        let mut rebound = std::net::TcpListener::bind(addr);
        for _ in 0..100 {
            if rebound.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            rebound = std::net::TcpListener::bind(addr);
        }
        rebound.unwrap();
    }
}
