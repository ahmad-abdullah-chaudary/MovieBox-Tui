//! Loopback HTTP relay for players that cannot attach arbitrary HTTP headers
//! to their own network requests.
//!
//! VLC (and Android external intents) only expose `Referer` / `User-Agent`
//! overrides, so a MovieBox CloudFront DASH stream whose segments are
//! authenticated with a signed `Cookie` cannot be handed to VLC directly. To
//! play such a stream we start a tiny HTTP server bound to `127.0.0.1` on an
//! ephemeral port and give the player `http://127.0.0.1:<port><path>` instead
//! of the original URL.
//!
//! The player fetches the manifest through the relay, then resolves each
//! relative segment URL against the manifest URL, which keeps landing on the
//! relay. Every request is re-issued against the original origin with the
//! required headers attached and streamed back byte-for-byte, so
//! `Accept-Ranges`/seek and DASH quality selection behave exactly as they
//! would against CloudFront directly.
//!
//! The relay lives only for the duration of one playback session and is torn
//! down the moment the player process exits (or the launch fails).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const MAX_HEAD_BYTES: usize = 32 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(60);

/// A running loopback relay serving one playback URL.
pub struct VlcRelay {
    /// Local base address the player connects to, e.g. `http://127.0.0.1:57021`.
    pub base_url: String,
    /// The exact local URL to hand to the player.
    pub play_url: String,
    shutdown: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for VlcRelay {
    fn drop(&mut self) {
        self.stop();
    }
}

impl VlcRelay {
    /// Stops accepting new requests and tears down the relay.
    ///
    /// In-flight segment fetches already past this point still finish writing,
    /// so the relay never corrupts a stream mid-request.
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

/// Starts a relay in front of `origin_url`, replaying every request with
/// `headers` attached. `headers` are sent as-is on every upstream request,
/// including `Cookie`, `Referer`, and `User-Agent`.
pub async fn start(origin_url: &str, headers: Vec<(String, String)>) -> std::io::Result<VlcRelay> {
    let origin = url::Url::parse(origin_url)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let base_url = format!("http://127.0.0.1:{port}");
    let play_url = match origin.query() {
        Some(query) => format!("{base_url}{}?{query}", origin.path()),
        None => format!("{base_url}{}", origin.path()),
    };

    let client = Arc::new(
        reqwest::Client::builder()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()
            .map_err(std::io::Error::other)?,
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let origin = Arc::new(origin);
    let headers = Arc::new(headers);

    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let client = client.clone();
                    let origin = origin.clone();
                    let headers = headers.clone();
                    let shutdown = task_shutdown.clone();
                    tokio::spawn(async move {
                        let _ = handle_request(stream, client, origin, headers, shutdown).await;
                    });
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    });

    Ok(VlcRelay {
        base_url,
        play_url,
        shutdown,
        task,
    })
}

async fn handle_request(
    stream: TcpStream,
    client: Arc<reqwest::Client>,
    origin: Arc<url::Url>,
    headers: Arc<Vec<(String, String)>>,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);
    let head = read_http_head(&mut reader).await?;
    if head.trim().is_empty() || shutdown.load(Ordering::Relaxed) {
        return Ok(());
    }

    let first_line = head.lines().next().unwrap_or("");
    let Some((method, target)) = parse_request_line(first_line) else {
        return Ok(());
    };
    if !target.starts_with('/') {
        return Ok(());
    }

    if !matches!(method, "GET" | "HEAD") {
        write_simple_response(&mut write_half, 405, "Method Not Allowed").await?;
        return Ok(());
    }

    let origin_str = match (origin.host_str(), origin.port()) {
        (Some(host), Some(port)) => format!("{}://{}:{port}", origin.scheme(), host),
        (Some(host), None) => format!("{}://{}", origin.scheme(), host),
        _ => return Ok(()),
    };
    let Ok(upstream) = url::Url::parse(&format!("{origin_str}{target}")) else {
        return Ok(());
    };

    let range = header_value(&head, "range");
    let mut builder = if method == "HEAD" {
        client.head(upstream)
    } else {
        client.get(upstream)
    };
    for (name, value) in headers.iter() {
        builder = builder.header(name, value);
    }
    if !range.is_empty() {
        builder = builder.header(reqwest::header::RANGE, &range);
    }

    let mut response = tokio::time::timeout(CONNECT_TIMEOUT, builder.send())
        .await
        .map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "upstream connect timed out")
        })?
        .map_err(std::io::Error::other)?;

    let status = response.status();
    write_half
        .write_all(
            format!(
                "HTTP/1.1 {} {}\r\n",
                status.as_u16(),
                status.canonical_reason().unwrap_or("")
            )
            .as_bytes(),
        )
        .await?;
    write_half.write_all(b"Connection: close\r\n").await?;
    for (name, value) in response.headers() {
        if matches!(
            name.as_str(),
            "content-type"
                | "content-length"
                | "content-range"
                | "etag"
                | "last-modified"
                | "accept-ranges"
        ) {
            let value = value.to_str().unwrap_or("");
            write_half
                .write_all(format!("{}: {}\r\n", name.as_str(), value).as_bytes())
                .await?;
        }
    }
    if !response
        .headers()
        .contains_key(reqwest::header::ACCEPT_RANGES)
    {
        write_half.write_all(b"Accept-Ranges: bytes\r\n").await?;
    }
    write_half.write_all(b"\r\n").await?;

    if method == "HEAD" {
        write_half.shutdown().await?;
        return Ok(());
    }

    while let Some(chunk) = tokio::time::timeout(CHUNK_TIMEOUT, response.chunk())
        .await
        .map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "upstream body read timed out")
        })?
        .map_err(std::io::Error::other)?
    {
        if shutdown.load(Ordering::Relaxed) || chunk.is_empty() {
            break;
        }
        write_half.write_all(&chunk).await?;
    }
    write_half.shutdown().await?;
    Ok(())
}

async fn read_http_head<R>(reader: &mut R) -> std::io::Result<String>
where
    R: AsyncBufRead + Unpin,
{
    let mut head = Vec::with_capacity(1024);
    loop {
        let n = tokio::time::timeout(CHUNK_TIMEOUT, reader.read_until(b'\n', &mut head))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout reading request head")
            })?
            .map_err(std::io::Error::other)?;
        if n == 0 {
            return Ok(String::new());
        }
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            break;
        }
        if head.len() > MAX_HEAD_BYTES {
            return Ok(String::new());
        }
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

fn parse_request_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let target_os = parts.next()?;
    Some((method, target_os))
}

fn header_value(head: &str, name: &str) -> String {
    for line in head.lines().skip(1) {
        let Some((header_name, rest)) = line.split_once(':') else {
            continue;
        };
        if header_name.trim().eq_ignore_ascii_case(name) {
            return rest.trim().to_string();
        }
    }
    String::new()
}

async fn write_simple_response<W>(writer: &mut W, status: u16, reason: &str) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer
        .write_all(
            format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A mock origin that behaves like MovieBox's CloudFront: it answers a
    /// manifest, relative segment URLs, and byte-range seeks, and rejects any
    /// request that is not carrying the expected `Cookie` with 403.
    struct MockOrigin {
        base_url: String,
        authorized_requests: Arc<AtomicUsize>,
    }

    async fn start_mock_origin() -> MockOrigin {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let authorized_requests = Arc::new(AtomicUsize::new(0));
        let counter = authorized_requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let counter = counter.clone();
                tokio::spawn(async move {
                    let _ = serve_origin(&mut stream, counter).await;
                });
            }
        });
        MockOrigin {
            base_url,
            authorized_requests,
        }
    }

    async fn serve_origin(
        stream: &mut TcpStream,
        authorized_requests: Arc<AtomicUsize>,
    ) -> std::io::Result<()> {
        let (mut read_half, mut write_half) = stream.split();
        let mut reader = tokio::io::BufReader::new(&mut read_half);
        let head = read_http_head(&mut reader).await?;
        let Some((method, target)) = head.lines().next().and_then(parse_request_line) else {
            return Ok(());
        };

        if !header_value(&head, "cookie").contains("CloudFront-Policy=test") {
            return write_simple_response(&mut write_half, 403, "Forbidden").await;
        }
        authorized_requests.fetch_add(1, Ordering::Relaxed);

        let range = header_value(&head, "range");
        let response = match target {
            "/v1/movie/manifest.mpd" => (
                200,
                "content-type: text/plain\r\ncontent-length: 30\r\n",
                b"manifest references seg-1.m4s\n".to_vec(),
                None,
            ),
            "/v1/movie/seg-1.m4s" if !range.is_empty() => (
                206,
                "content-type: video/mp4\r\ncontent-length: 4\r\ncontent-range: bytes 0-3/6\r\n",
                b"SEGS".to_vec(),
                Some("Range: bytes 0-3\r\n"),
            ),
            "/v1/movie/seg-1.m4s" => (
                200,
                "content-type: video/mp4\r\ncontent-length: 6\r\naccept-ranges: bytes\r\n",
                b"SEGSIX".to_vec(),
                Some("Accept-Ranges: bytes\r\n"),
            ),
            _ => (404, "content-length: 0\r\n", Vec::new(), None),
        };

        write_half
            .write_all(format!("HTTP/1.1 {} OK\r\n{}\r\n", response.0, response.1).as_bytes())
            .await?;
        if method != "HEAD" {
            write_half.write_all(&response.2).await?;
        }
        write_half.shutdown().await?;
        Ok(())
    }

    fn relay_base(play_url: &str) -> String {
        play_url
            .split_once("/v1")
            .map(|(b, _)| b.to_string())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn forwards_cookie_and_relative_segment_urls() {
        let origin = start_mock_origin().await;

        let headers = vec![
            ("Cookie".to_string(), "CloudFront-Policy=test".to_string()),
            ("Referer".to_string(), "https://sportslive.wine".to_string()),
        ];
        let relay = start(
            &format!("{}/v1/movie/manifest.mpd", origin.base_url),
            headers,
        )
        .await
        .unwrap();

        let client = reqwest::Client::new();

        let direct = client
            .get(format!("{}/v1/movie/manifest.mpd", origin.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(
            direct.status().as_u16(),
            403,
            "mock origin requires the Cookie"
        );

        let manifest = client.get(&relay.play_url).send().await.unwrap();
        assert_eq!(manifest.status().as_u16(), 200);
        let body = manifest.text().await.unwrap();
        assert!(body.contains("seg-1.m4s"), "manifest body was: {body}");

        let base = relay_base(&relay.play_url);
        let segment = client
            .get(format!("{base}/v1/movie/seg-1.m4s"))
            .send()
            .await
            .unwrap();
        assert_eq!(segment.status().as_u16(), 200);
        assert_eq!(segment.text().await.unwrap(), "SEGSIX");

        assert!(
            origin.authorized_requests.load(Ordering::Relaxed) >= 2,
            "every relayed request must carry the Cookie upstream"
        );
    }

    #[tokio::test]
    async fn passes_byte_range_requests_through() {
        let origin = start_mock_origin().await;
        let relay = start(
            &format!("{}/v1/movie/manifest.mpd", origin.base_url),
            vec![("Cookie".to_string(), "CloudFront-Policy=test".to_string())],
        )
        .await
        .unwrap();
        let client = reqwest::Client::new();
        let base = relay_base(&relay.play_url);

        let ranged = client
            .get(format!("{base}/v1/movie/seg-1.m4s"))
            .header("Range", "bytes=0-3")
            .send()
            .await
            .unwrap();
        assert_eq!(ranged.status().as_u16(), 206);
        assert_eq!(
            ranged
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok()),
            Some("bytes 0-3/6")
        );
        assert_eq!(ranged.text().await.unwrap(), "SEGS");
    }

    #[tokio::test]
    async fn normalizes_dot_segments_in_request_paths() {
        let origin = start_mock_origin().await;
        let relay = start(
            &format!("{}/v1/movie/manifest.mpd", origin.base_url),
            vec![("Cookie".to_string(), "CloudFront-Policy=test".to_string())],
        )
        .await
        .unwrap();
        let client = reqwest::Client::new();
        let base = relay_base(&relay.play_url);

        let dotty = client
            .get(format!("{base}/v1/../v1/movie/seg-1.m4s"))
            .send()
            .await
            .unwrap();
        assert_eq!(dotty.status().as_u16(), 200);
        assert_eq!(dotty.text().await.unwrap(), "SEGSIX");
    }

    #[tokio::test]
    async fn answers_head_requests_without_a_body() {
        let origin = start_mock_origin().await;
        let relay = start(
            &format!("{}/v1/movie/manifest.mpd", origin.base_url),
            vec![("Cookie".to_string(), "CloudFront-Policy=test".to_string())],
        )
        .await
        .unwrap();
        let client = reqwest::Client::new();
        let base = relay_base(&relay.play_url);

        let head = client
            .head(format!("{base}/v1/movie/seg-1.m4s"))
            .send()
            .await
            .unwrap();
        assert_eq!(head.status().as_u16(), 200);
        assert_eq!(
            head.headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok()),
            Some("6")
        );
        assert_eq!(head.text().await.unwrap(), "");
    }

    #[tokio::test]
    async fn shuts_the_listener_down_on_stop() {
        let origin = start_mock_origin().await;
        let relay = start(
            &format!("{}/v1/movie/manifest.mpd", origin.base_url),
            vec![("Cookie".to_string(), "CloudFront-Policy=test".to_string())],
        )
        .await
        .unwrap();
        let url = relay.play_url.clone();
        relay.stop();

        let result = reqwest::Client::new().get(&url).send().await;
        assert!(result.is_err(), "relay must refuse connections after stop");
    }
}
