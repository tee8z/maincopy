use std::{io, net::SocketAddr};

use axum::Router;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::{PublicState, public_router};

/// Bound public HTTP server and its request-facing dependencies.
pub(crate) struct PublicServer {
    pub(crate) local_addr: SocketAddr,
    listener: TcpListener,
    router: Router,
}

impl PublicServer {
    pub(crate) async fn bind(bind: SocketAddr, state: PublicState) -> io::Result<Self> {
        let listener = TcpListener::bind(bind).await?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            local_addr,
            listener,
            router: public_router(state),
        })
    }

    pub(crate) async fn serve(self, cancellation: CancellationToken) -> io::Result<()> {
        axum::serve(self.listener, self.router)
            .with_graceful_shutdown(cancellation.cancelled_owned())
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, path::Path, sync::Arc, time::Duration};

    use tokio::time::timeout;

    use super::*;
    use crate::{
        content::{ContentTreeLimits, discover_content_tree, resolve_content_assets},
        frontend_assets::embedded_manifest,
        render::{
            PublicLedgerProjection, SiteSnapshotReader, build_site_snapshot,
            compile_content_catalog, render_site_shell,
        },
        web::Readiness,
    };

    fn public_state() -> PublicState {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/content");
        let tree = discover_content_tree(&root, ContentTreeLimits::default()).unwrap();
        let content = tree.validate().unwrap();
        let assets = resolve_content_assets(&tree, &content).unwrap();
        let catalog = Arc::new(compile_content_catalog(&content, &assets).unwrap());
        let ledger = PublicLedgerProjection::empty();
        let shell = render_site_shell(catalog, embedded_manifest(), &ledger).unwrap();
        let snapshot = build_site_snapshot(shell, &ledger).unwrap();
        PublicState {
            snapshots: SiteSnapshotReader::from_snapshot(snapshot),
            readiness: Readiness::new(true),
        }
    }

    #[tokio::test]
    async fn ephemeral_listener_serves_the_public_router() {
        let cancellation = CancellationToken::new();
        let server = PublicServer::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), public_state())
            .await
            .unwrap();
        let address = server.local_addr;
        let serving = tokio::spawn(server.serve(cancellation.clone()));
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let response = client
            .get(format!("http://{address}/health/live"))
            .send()
            .await
            .unwrap();

        assert!(address.ip().is_loopback());
        assert_ne!(address.port(), 0);
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(response.text().await.unwrap(), r#"{"status":"live"}"#);

        cancellation.cancel();
        timeout(Duration::from_secs(2), serving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn cancellation_stops_the_server_and_releases_its_address() {
        let cancellation = CancellationToken::new();
        let server = PublicServer::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), public_state())
            .await
            .unwrap();
        let address = server.local_addr;
        let serving = tokio::spawn(server.serve(cancellation.clone()));
        tokio::task::yield_now().await;

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
