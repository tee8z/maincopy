//! A local HTTP peer that answers each request with the next scripted reply
//! and records what the client sent.

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

pub(super) struct Peer {
    pub origin: String,
    task: JoinHandle<Vec<String>>,
}

impl Peer {
    /// Serve one `(status line, JSON body)` reply per connection, then stop.
    pub(super) async fn start(replies: Vec<(&'static str, &'static str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut stream).await);
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).await.unwrap();
            }
            requests
        });
        Self { origin, task }
    }

    /// Every scripted reply must have been requested.
    pub(super) async fn requests(self) -> Vec<String> {
        self.task.await.unwrap()
    }
}

async fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let end = loop {
        let mut chunk = [0; 4096];
        let read = stream.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "peer closed before request headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let length = std::str::from_utf8(&bytes[..end])
        .unwrap()
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < end + length {
        let mut chunk = [0; 4096];
        let read = stream.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "peer closed before request body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8(bytes).unwrap()
}
