use std::{
    future::Future as _,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::serve::Listener;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Sleep, sleep},
};

const MAX_CONNECTIONS: usize = 256;
const CONNECTION_LIFETIME: Duration = Duration::from_secs(60);

/// Connection leases cover slow headers and response delivery, beyond router deadlines.
pub(crate) struct PublicListener {
    listener: TcpListener,
    admission: Arc<Semaphore>,
}

impl PublicListener {
    pub(crate) fn new(listener: TcpListener) -> Self {
        Self {
            listener,
            admission: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        }
    }
}

impl Listener for PublicListener {
    type Io = PublicConnection;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // Only this listener owns the semaphore; it is never closed.
        let lease = self
            .admission
            .clone()
            .acquire_owned()
            .await
            .expect("public connection admission remains open");
        let (stream, address) = Listener::accept(&mut self.listener).await;
        (
            PublicConnection {
                stream,
                _lease: lease,
                deadline: Box::pin(sleep(CONNECTION_LIFETIME)),
            },
            address,
        )
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

pub(crate) struct PublicConnection {
    stream: TcpStream,
    _lease: OwnedSemaphorePermit,
    deadline: Pin<Box<Sleep>>,
}

impl PublicConnection {
    fn deadline_elapsed(&mut self, context: &mut Context<'_>) -> bool {
        self.deadline.as_mut().poll(context).is_ready()
    }
}

impl AsyncRead for PublicConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.deadline_elapsed(context) {
            return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for PublicConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.deadline_elapsed(context) {
            return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        Pin::new(&mut self.stream).poll_write(context, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.deadline_elapsed(context) {
            return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[tokio::test]
    async fn accepted_connections_transfer_bytes_and_expire_even_with_an_idle_peer() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let mut listener = PublicListener::new(listener);
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut connection, _) = listener.accept().await;
        assert_eq!(listener.admission.available_permits(), MAX_CONNECTIONS - 1);
        client.write_all(b"request").await.unwrap();
        let mut read = [0; 7];
        connection.read_exact(&mut read).await.unwrap();
        assert_eq!(&read, b"request");
        connection.write_all(b"response").await.unwrap();
        connection.flush().await.unwrap();
        let mut response = [0; 8];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"response");
        tokio::time::pause();
        tokio::time::advance(CONNECTION_LIFETIME).await;
        assert_eq!(
            connection.read(&mut read).await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            connection.write(b"late").await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            connection.flush().await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        connection.shutdown().await.unwrap();
        drop(connection);
        assert_eq!(listener.admission.available_permits(), MAX_CONNECTIONS);
    }

    #[tokio::test]
    async fn exhausted_connection_capacity_waits_for_a_lease_without_accepting_more_sockets() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let mut listener = PublicListener::new(listener);
        let address = listener.local_addr().unwrap();
        let all_leases = listener
            .admission
            .clone()
            .acquire_many_owned(MAX_CONNECTIONS as u32)
            .await
            .unwrap();
        let _client = TcpStream::connect(address).await.unwrap();
        let mut accepting = Box::pin(listener.accept());
        assert!(matches!(
            std::future::poll_fn(|context| Poll::Ready(accepting.as_mut().poll(context))).await,
            Poll::Pending
        ));
        drop(all_leases);
        let (connection, actual_address) = tokio::time::timeout(Duration::from_secs(2), accepting)
            .await
            .unwrap();
        assert!(actual_address.ip().is_loopback());
        drop(connection);
        assert_eq!(listener.admission.available_permits(), MAX_CONNECTIONS);
    }
}
