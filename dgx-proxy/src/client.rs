//! Upstream HTTP/1.1 client: one TCP connection per attempt (like dgx-lb). The connection task is
//! returned so the caller can abort it — that is how a client disconnect cancels the upstream.

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Limited};
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type ReqBody = BoxBody<Bytes, BoxError>;

#[derive(Debug, Clone)]
pub struct Upstream {
    /// host:port, also used as the Host header.
    pub authority: String,
    /// Path prefix of the base URL, without a trailing slash ("" for none).
    pub prefix: String,
}

impl Upstream {
    pub fn parse(url: &str) -> Result<Upstream, String> {
        let rest = url.strip_prefix("http://").ok_or("only http:// URLs are supported")?;
        let (auth, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if auth.is_empty() {
            return Err("missing host".into());
        }
        let authority = if auth.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) && !auth.ends_with(']') {
            auth.to_string()
        } else {
            format!("{auth}:80")
        };
        Ok(Upstream { authority, prefix: path.trim_end_matches('/').to_string() })
    }
}

#[derive(Debug)]
pub enum ConnectError {
    Timeout,
    Io(String),
}

pub struct Conn {
    pub sender: SendRequest<ReqBody>,
    pub task: AbortOnDrop,
}

/// Aborts the connection task when dropped: whoever drops the last handle closes the socket.
pub struct AbortOnDrop(pub JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn connect(up: &Upstream, timeout: Duration) -> Result<Conn, ConnectError> {
    let stream = match tokio::time::timeout(timeout, TcpStream::connect(&up.authority)).await {
        Err(_) => return Err(ConnectError::Timeout),
        Ok(Err(e)) => return Err(ConnectError::Io(format!("connect: {e}"))),
        Ok(Ok(s)) => s,
    };
    let _ = stream.set_nodelay(true);
    let (sender, conn) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.map_err(|e| ConnectError::Io(format!("handshake: {e}")))?;
    let task = tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(Conn { sender, task: AbortOnDrop(task) })
}

pub fn empty_body() -> ReqBody {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

/// A small GET (health probe, backend /metrics). The whole exchange is bounded by `timeout`.
pub async fn get(up: &Upstream, path: &str, timeout: Duration, limit: usize) -> Result<(u16, Bytes), String> {
    let fut = async {
        let mut c = connect(up, timeout).await.map_err(|e| match e {
            ConnectError::Timeout => "connect_timeout".to_string(),
            ConnectError::Io(s) => s,
        })?;
        let req = hyper::Request::get(format!("{}{}", up.prefix, path))
            .header(hyper::header::HOST, &up.authority)
            .body(empty_body())
            .map_err(|e| e.to_string())?;
        let resp = c.sender.send_request(req).await.map_err(|e| format!("request: {e}"))?;
        let status = resp.status().as_u16();
        let body = Limited::new(resp.into_body(), limit).collect().await.map_err(|e| format!("body: {e}"))?.to_bytes();
        drop(c);
        Ok::<_, String>((status, body))
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(r) => r,
        Err(_) => Err("timeout".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_urls() {
        let u = Upstream::parse("http://127.0.0.1:8888").unwrap();
        assert_eq!((u.authority.as_str(), u.prefix.as_str()), ("127.0.0.1:8888", ""));
        let u = Upstream::parse("http://example:9/base/").unwrap();
        assert_eq!((u.authority.as_str(), u.prefix.as_str()), ("example:9", "/base"));
        let u = Upstream::parse("http://example").unwrap();
        assert_eq!(u.authority, "example:80");
        assert!(Upstream::parse("https://x").is_err());
    }
}
