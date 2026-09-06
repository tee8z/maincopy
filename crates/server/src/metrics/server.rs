use std::{io, net::SocketAddr};

use axum::{
    Router,
    extract::State,
    http::{
        StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE},
    },
    response::{IntoResponse as _, Response},
    routing::get,
};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::Metrics;
use crate::web::ConnectionListener;

pub(crate) struct MetricsServer {
    pub(crate) local_addr: SocketAddr,
    listener: ConnectionListener,
    router: Router,
}

impl MetricsServer {
    pub(crate) async fn bind(
        address: SocketAddr,
        metrics: Metrics,
    ) -> Result<Self, MetricsServerError> {
        if !address.ip().is_loopback() {
            return Err(MetricsServerError::NotLoopback);
        }
        let listener = TcpListener::bind(address)
            .await
            .map_err(MetricsServerError::Bind)?;
        let local_addr = listener.local_addr().map_err(MetricsServerError::Bind)?;
        Ok(Self {
            local_addr,
            listener: ConnectionListener::new(listener),
            router: router(metrics),
        })
    }

    pub(crate) async fn serve(self, cancellation: CancellationToken) -> io::Result<()> {
        axum::serve(self.listener, self.router)
            .with_graceful_shutdown(cancellation.cancelled_owned())
            .await
    }
}

fn router(metrics: Metrics) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .with_state(metrics)
}

async fn scrape(State(metrics): State<Metrics>) -> Response {
    match metrics.encode() {
        Ok(body) => (
            [
                (CONTENT_TYPE, "text/plain; version=0.0.4"),
                (CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Debug, Error)]
pub(crate) enum MetricsServerError {
    #[error("the metrics listener must bind a loopback address")]
    NotLoopback,
    #[error("the metrics listener could not bind its loopback address")]
    Bind(#[source] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::DatabaseMetrics;
    use axum::serve::Listener as _;
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request},
    };
    use std::{future::Future as _, task::Poll, time::Duration};
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::{TcpSocket, TcpStream},
    };
    use tower::ServiceExt as _;

    fn metrics() -> Metrics {
        Metrics::new(&DatabaseMetrics::new().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn metrics_router_serves_only_standard_get_and_head_scrapes() {
        let app = router(metrics());
        for method in [Method::GET, Method::HEAD] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method.clone())
                        .uri("/metrics")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[CONTENT_TYPE],
                "text/plain; version=0.0.4"
            );
            let body = to_bytes(response.into_body(), 32 * 1024).await.unwrap();
            if method == Method::HEAD {
                assert!(body.is_empty());
            } else {
                assert!(String::from_utf8_lossy(&body).contains("tokio_workers_count"));
            }
        }
        for (method, path, status) in [
            (Method::POST, "/metrics", StatusCode::METHOD_NOT_ALLOWED),
            (Method::GET, "/", StatusCode::NOT_FOUND),
            (Method::GET, "/admin", StatusCode::NOT_FOUND),
            (Method::GET, "/health/ready", StatusCode::NOT_FOUND),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
        }
    }

    #[tokio::test]
    async fn metrics_listener_rejects_external_addresses_and_releases_loopback_on_cancellation() {
        for address in ["0.0.0.0:0", "[::]:0", "192.0.2.1:0"] {
            assert!(matches!(
                MetricsServer::bind(address.parse().unwrap(), metrics()).await,
                Err(MetricsServerError::NotLoopback)
            ));
        }
        let cancellation = CancellationToken::new();
        // A bound, non-listening socket keeps this port out of concurrent
        // ephemeral allocations. SO_REUSEADDR permits the real listener to
        // share the reservation, but not another live listener.
        let reservation = TcpSocket::new_v4().unwrap();
        reservation.set_reuseaddr(true).unwrap();
        reservation.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = reservation.local_addr().unwrap();
        let server = MetricsServer::bind(address, metrics()).await.unwrap();
        assert_eq!(server.local_addr, address);
        assert_eq!(
            TcpListener::bind(address).await.unwrap_err().kind(),
            io::ErrorKind::AddrInUse,
            "the reservation must not permit two live listeners"
        );
        let mut serving = Box::pin(server.serve(cancellation.clone()));
        let clients = async {
            let mut idle_scraper = TcpStream::connect(address).await.unwrap();
            idle_scraper.write_all(b"GET /met").await.unwrap();
            // A completed second request proves the server accepted the earlier
            // partial-header connection before shutdown begins.
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap();
            let response = client
                .get(format!("http://{address}/metrics"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            response.bytes().await.unwrap();
            drop(client);
            idle_scraper
        };
        let idle_scraper = tokio::select! {
            result = &mut serving => panic!("metrics server exited before cancellation: {result:?}"),
            idle_scraper = clients => idle_scraper,
        };
        tokio::time::pause();
        cancellation.cancel();
        assert!(matches!(
            std::future::poll_fn(|context| Poll::Ready(serving.as_mut().poll(context))).await,
            Poll::Pending
        ));
        // Completion must mean accepted connections are drained. Returning after
        // an independent ten-second timeout would detach this live connection.
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(matches!(
            std::future::poll_fn(|context| Poll::Ready(serving.as_mut().poll(context))).await,
            Poll::Pending
        ));
        tokio::time::advance(Duration::from_secs(50)).await;
        tokio::time::resume();
        tokio::time::timeout(Duration::from_secs(2), serving)
            .await
            .unwrap()
            .unwrap();
        let mut closing = idle_scraper.take(4097);
        let mut final_bytes = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            closing.read_to_end(&mut final_bytes),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            final_bytes.len() <= 4096,
            "the incomplete request must close without an unbounded response"
        );
        let rebound = TcpListener::bind(address)
            .await
            .expect("the drained metrics server must release its reserved listener address");
        assert_eq!(rebound.local_addr().unwrap(), address);
        drop(rebound);
        drop(reservation);
    }

    #[tokio::test]
    async fn metrics_connections_obey_the_shared_capacity_and_lifetime() {
        let mut server = MetricsServer::bind("127.0.0.1:0".parse().unwrap(), metrics())
            .await
            .unwrap();
        let mut accepted = Vec::new();
        for _ in 0..256 {
            let client = TcpStream::connect(server.local_addr).await.unwrap();
            let (connection, _) = server.listener.accept().await;
            accepted.push((client, connection));
        }
        let extra_client = TcpStream::connect(server.local_addr).await.unwrap();
        let mut accepting = Box::pin(server.listener.accept());
        assert!(matches!(
            std::future::poll_fn(|context| Poll::Ready(accepting.as_mut().poll(context))).await,
            Poll::Pending
        ));
        drop(accepted.pop());
        let (mut extra_connection, _) = tokio::time::timeout(Duration::from_secs(2), accepting)
            .await
            .unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(
            extra_connection.read(&mut [0; 1]).await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            extra_connection
                .write(b"late scrape")
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        drop(extra_connection);
        drop(extra_client);
        drop(accepted);
    }
}
