use std::{io, net::SocketAddr, time::Duration};

use axum::Router;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::origin::AdminBind;

const GRACEFUL_SHUTDOWN_LIMIT: Duration = Duration::from_secs(10);

/// Bound loopback HTTP backend for the authenticated administration router.
///
/// TLS terminates at the configured HTTPS gateway. This backend is deliberately
/// unreachable on non-loopback interfaces and never constructs an
/// unauthenticated router itself.
pub(crate) struct AdminServer {
    pub(crate) local_addr: SocketAddr,
    listener: TcpListener,
    router: Router,
}

impl AdminServer {
    pub(crate) async fn bind(bind: AdminBind, router: Router) -> io::Result<Self> {
        let listener = TcpListener::bind(bind.into_socket_addr()).await?;
        Self::from_listener(listener, router)
    }

    fn from_listener(listener: TcpListener, router: Router) -> io::Result<Self> {
        let local_addr = listener.local_addr()?;
        debug_assert!(local_addr.ip().is_loopback());
        Ok(Self {
            local_addr,
            listener,
            router,
        })
    }

    pub(crate) async fn serve(self, cancellation: CancellationToken) -> io::Result<()> {
        let shutdown = cancellation.clone();
        let serving = async move {
            axum::serve(self.listener, self.router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        };
        tokio::pin!(serving);
        tokio::select! {
            result = &mut serving => result,
            () = cancellation.cancelled() => {
                match tokio::time::timeout(GRACEFUL_SHUTDOWN_LIMIT, &mut serving).await {
                    Ok(result) => result,
                    Err(_) => {
                        tracing::warn!(
                            timeout_seconds = GRACEFUL_SHUTDOWN_LIMIT.as_secs(),
                            "admin connections exceeded the graceful shutdown limit"
                        );
                        Ok(())
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, time::Duration};

    use axum::{Router, routing::get};
    use tokio::time::timeout;

    use super::*;

    async fn test_server(router: Router) -> AdminServer {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .unwrap();
        AdminServer::from_listener(listener, router).unwrap()
    }

    #[tokio::test]
    async fn loopback_backend_serves_only_the_supplied_router() {
        let cancellation = CancellationToken::new();
        let server = test_server(
            Router::new().route("/authenticated", get(|| async { "protected router" })),
        )
        .await;
        let address = server.local_addr;
        let serving = tokio::spawn(server.serve(cancellation.clone()));

        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let response = client
            .get(format!("http://{address}/authenticated"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "protected router");

        cancellation.cancel();
        timeout(Duration::from_secs(2), serving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn cancellation_releases_the_listener() {
        let cancellation = CancellationToken::new();
        let server = test_server(Router::new()).await;
        let address = server.local_addr;
        let serving = tokio::spawn(server.serve(cancellation.clone()));

        cancellation.cancel();
        timeout(Duration::from_secs(2), serving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        let rebound = TcpListener::bind(address).await.unwrap();
        assert_eq!(rebound.local_addr().unwrap(), address);
    }
}
