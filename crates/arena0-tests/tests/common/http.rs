use std::net::SocketAddr;

use arena0_api::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

pub struct HttpReply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// One request with `Connection: close`; reads a Content-Length body.
pub async fn http(
    address: SocketAddr,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> HttpReply {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let content_type = content_type
        .map(|value| format!("Content-Type: {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Length: {}\r\n{content_type}\r\n",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut read = BufReader::new(stream);
    let mut line = String::new();
    read.read_line(&mut line).await.unwrap();
    let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut headers = Vec::new();
    loop {
        line.clear();
        assert_ne!(
            read.read_line(&mut line).await.unwrap(),
            0,
            "incomplete HTTP headers"
        );
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.trim_end().split_once(':').unwrap();
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map(|(_, value)| value.parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; length];
    read.read_exact(&mut body).await.unwrap();
    HttpReply {
        status,
        headers,
        body,
    }
}

pub async fn rpc(address: SocketAddr, request: &Request) -> Response {
    let reply = http(
        address,
        "POST",
        "/rpc",
        Some("application/json"),
        &serde_json::to_vec(request).unwrap(),
    )
    .await;
    assert_eq!(reply.status, 200);
    serde_json::from_slice(&reply.body).unwrap()
}

/// `GET /events`; decodes chunked transfer encoding and SSE fields.
pub struct EventStream {
    read: BufReader<TcpStream>,
    pending: Vec<u8>,
}

impl EventStream {
    pub async fn open(address: SocketAddr) -> Self {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(format!("GET /events HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut read = BufReader::new(stream);
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("HTTP/1.1 200"), "{line}");
        let mut chunked = false;
        loop {
            line.clear();
            assert_ne!(read.read_line(&mut line).await.unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if line
                .to_ascii_lowercase()
                .starts_with("transfer-encoding: chunked")
            {
                chunked = true;
            }
        }
        assert!(chunked, "SSE should use chunked transfer encoding");
        Self {
            read,
            pending: Vec::new(),
        }
    }

    /// The next SSE message's (event, data).
    pub async fn next(&mut self) -> (String, String) {
        loop {
            if let Some(end) = self.pending.windows(2).position(|bytes| bytes == b"\n\n") {
                let message = String::from_utf8(self.pending.drain(..end + 2).collect()).unwrap();
                let mut event = "message".to_owned();
                let mut data = Vec::new();
                for line in message.lines() {
                    if let Some(value) = line.strip_prefix("event:") {
                        event = value.strip_prefix(' ').unwrap_or(value).to_owned();
                    } else if let Some(value) = line.strip_prefix("data:") {
                        data.push(value.strip_prefix(' ').unwrap_or(value).to_owned());
                    }
                }
                if !data.is_empty() {
                    return (event, data.join("\n"));
                }
                continue;
            }
            let mut line = String::new();
            assert_ne!(
                self.read.read_line(&mut line).await.unwrap(),
                0,
                "SSE closed"
            );
            let length = usize::from_str_radix(line.trim().split(';').next().unwrap(), 16).unwrap();
            assert_ne!(length, 0, "SSE ended");
            let start = self.pending.len();
            self.pending.resize(start + length, 0);
            self.read
                .read_exact(&mut self.pending[start..])
                .await
                .unwrap();
            let mut crlf = [0; 2];
            self.read.read_exact(&mut crlf).await.unwrap();
            assert_eq!(&crlf, b"\r\n");
        }
    }
}
