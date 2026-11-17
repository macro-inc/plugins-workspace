// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use super::{
    commands::{request_client, ClientOptions, ReqwestResponse, READ_BODY_CAP},
    init_with_cookie_store_dir, Http, REREADS_SYSTEM_PROXY,
};
use reqwest::{cookie::CookieStore, header::HeaderValue};
use std::{
    io::{BufRead, BufReader, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};
use tauri::{
    test::{mock_builder, mock_context, noop_assets, MockRuntime},
    Manager,
};

type TestError = Box<dyn std::error::Error + Send + Sync>;
type TestResult = std::result::Result<(), TestError>;

/// An HTTP/1.1 server that counts the connections it accepts and the
/// requests its first connection serves.
struct Server {
    url: String,
    address: SocketAddr,
    connections: Arc<AtomicUsize>,
    first_connection_requests: Arc<AtomicUsize>,
    stopping: Arc<AtomicBool>,
    accepting: Option<JoinHandle<()>>,
}

impl Server {
    /// A server whose connections stay open for further requests.
    fn start() -> std::io::Result<Self> {
        Self::start_with(true)
    }

    /// A server that closes each connection after one response.
    fn start_closing_connections() -> std::io::Result<Self> {
        Self::start_with(false)
    }

    fn start_with(keep_alive: bool) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let connections = Arc::new(AtomicUsize::new(0));
        let first_connection_requests = Arc::new(AtomicUsize::new(0));
        let stopping = Arc::new(AtomicBool::new(false));
        let accepted = connections.clone();
        let on_first = first_connection_requests.clone();
        let stop = stopping.clone();

        let accepting = std::thread::spawn(move || {
            for stream in listener.incoming() {
                // `stop` connects once to wake this loop; that is not a client.
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(mut stream) = stream else { return };
                let Ok(requests) = stream.try_clone() else {
                    return;
                };
                let first = accepted.fetch_add(1, Ordering::SeqCst) == 0;
                let on_first = on_first.clone();

                // Answer every request on the connection until either end closes it.
                std::thread::spawn(move || {
                    let mut lines = BufReader::new(requests).lines();
                    while let Some(Ok(line)) = lines.next() {
                        if !line.is_empty() {
                            continue;
                        }
                        if first {
                            on_first.fetch_add(1, Ordering::SeqCst);
                        }
                        let connection = if keep_alive { "keep-alive" } else { "close" };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: {connection}\r\n\r\nok"
                        );
                        if stream.write_all(response.as_bytes()).is_err() || !keep_alive {
                            let _ = stream.shutdown(Shutdown::Both);
                            return;
                        }
                    }
                });
            }
        });

        Ok(Self {
            url: format!("http://{address}/"),
            address,
            connections,
            first_connection_requests,
            stopping,
            accepting: Some(accepting),
        })
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn first_connection_requests(&self) -> usize {
        self.first_connection_requests.load(Ordering::SeqCst)
    }

    /// Stops listening. Returns once the port refuses connections.
    fn stop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(accepting) = self.accepting.take() {
            let _ = accepting.join();
        }
    }
}

/// An app with the plugin, and the cookie directory it must outlive.
type TestApp = (tauri::App<MockRuntime>, tempfile::TempDir);

fn app() -> std::result::Result<TestApp, TestError> {
    let directory = tempfile::tempdir()?;
    let app = mock_builder()
        .plugin(init_with_cookie_store_dir(directory.path()))
        .build(mock_context(noop_assets()))?;
    Ok((app, directory))
}

/// Sends a GET the way the `fetch` command does and reads the whole body,
/// which returns the connection to its pool.
async fn get(http: &Http, url: &str, options: ClientOptions) -> std::result::Result<(), TestError> {
    let (client, shares_client) = request_client(http, options)?;
    let request = client.get(url);
    let response = if shares_client {
        http.send_shared(request).await?
    } else {
        request.send().await?
    };
    response.bytes().await?;
    Ok(())
}

fn get_shared(http: &Http, url: &str) -> TestResult {
    tauri::async_runtime::block_on(get(http, url, ClientOptions::default()))
}

#[test]
fn requests_share_a_connection() -> TestResult {
    let server = Server::start()?;
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();

    get_shared(&http, &server.url)?;
    get_shared(&http, &server.url)?;

    assert_eq!(server.connections(), 1);
    Ok(())
}

#[test]
fn a_request_with_client_options_gets_a_client_of_its_own() -> TestResult {
    let server = Server::start()?;
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();
    let no_redirects = || ClientOptions {
        max_redirections: Some(0),
        ..Default::default()
    };

    get_shared(&http, &server.url)?;
    tauri::async_runtime::block_on(get(&http, &server.url, no_redirects()))?;
    tauri::async_runtime::block_on(get(&http, &server.url, no_redirects()))?;

    assert_eq!(server.connections(), 3);
    Ok(())
}

#[test]
fn reset_connections_opens_a_new_connection() -> TestResult {
    let server = Server::start()?;
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();

    get_shared(&http, &server.url)?;
    http.reset_connections()?;
    get_shared(&http, &server.url)?;

    assert_eq!(server.connections(), 2);
    Ok(())
}

#[test]
fn a_sleep_since_the_last_request_opens_a_new_connection() -> TestResult {
    let server = Server::start()?;
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();

    get_shared(&http, &server.url)?;
    http.backdate_last_use(Duration::from_secs(60), Duration::from_secs(60));
    get_shared(&http, &server.url)?;
    assert_eq!(server.connections(), 2);

    // The sleep is behind it: the next request reuses the new connection.
    get_shared(&http, &server.url)?;
    assert_eq!(server.connections(), 2);
    Ok(())
}

#[test]
fn requests_that_wake_together_all_leave_the_old_connection() -> TestResult {
    const ROUNDS: usize = 20;
    const REQUESTS: usize = 8;

    for _ in 0..ROUNDS {
        let server = Server::start()?;
        let (app, _cookies) = app()?;
        let http = app.state::<Http>();

        get_shared(&http, &server.url)?;
        http.backdate_last_use(Duration::from_secs(60), Duration::from_secs(60));

        let together = std::sync::Barrier::new(REQUESTS);
        std::thread::scope(|scope| {
            for _ in 0..REQUESTS {
                scope.spawn(|| {
                    together.wait();
                    get_shared(&http, &server.url).expect("request after the sleep");
                });
            }
        });

        // Only the request before the sleep used the connection from before it.
        assert_eq!(server.first_connection_requests(), 1);
    }
    Ok(())
}

#[test]
fn an_idle_minute_without_sleep_keeps_the_connection() -> TestResult {
    let server = Server::start()?;
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();

    get_shared(&http, &server.url)?;
    http.backdate_last_use(Duration::from_secs(60), Duration::ZERO);
    get_shared(&http, &server.url)?;

    assert_eq!(server.connections(), 1);
    Ok(())
}

#[test]
fn failing_to_connect_to_a_new_origin_keeps_other_connections() -> TestResult {
    let server = Server::start()?;
    // Nothing listens on a port whose listener was dropped.
    let never_running = format!(
        "http://{}/",
        TcpListener::bind("127.0.0.1:0")?.local_addr()?
    );
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();

    get_shared(&http, &server.url)?;
    assert!(get_shared(&http, &never_running).is_err());
    get_shared(&http, &server.url)?;

    assert_eq!(server.connections(), 1);
    Ok(())
}

#[test]
fn failing_to_connect_to_an_origin_reached_before_replaces_the_client_where_it_can_differ(
) -> TestResult {
    let server = Server::start()?;
    let mut stopped = Server::start_closing_connections()?;
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();

    get_shared(&http, &server.url)?;
    get_shared(&http, &stopped.url)?;
    stopped.stop();
    assert!(get_shared(&http, &stopped.url).is_err());
    get_shared(&http, &server.url)?;

    // A new client only where it can read different proxy settings.
    let connections = if REREADS_SYSTEM_PROXY { 2 } else { 1 };
    assert_eq!(server.connections(), connections);

    // The replacement has not reached that origin, so failing again changes nothing.
    assert!(get_shared(&http, &stopped.url).is_err());
    get_shared(&http, &server.url)?;
    assert_eq!(server.connections(), connections);
    Ok(())
}

/// An HTTP/2 server: `/big` answers 3 MB, anything else answers `ok`.
#[cfg(feature = "http2")]
async fn serve_http2() -> std::io::Result<SocketAddr> {
    use http_body_util::Full;
    use hyper::{body::Bytes, service::service_fn, Request, Response};
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tauri::async_runtime::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tauri::async_runtime::spawn(async move {
                let service = service_fn(|request: Request<hyper::body::Incoming>| async move {
                    let body = if request.uri().path() == "/big" {
                        vec![b'x'; 3 * 1024 * 1024]
                    } else {
                        b"ok".to_vec()
                    };
                    Ok::<_, hyper::Error>(Response::new(Full::new(Bytes::from(body))))
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Ok(address)
}

#[cfg(feature = "http2")]
#[test]
fn responses_left_unread_do_not_stall_the_connection() -> TestResult {
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();
    // The shared client's settings, over HTTP/2 without TLS.
    let client = http.client_builder().http2_prior_knowledge().build()?;

    tauri::async_runtime::block_on(async {
        let address = serve_http2().await?;

        // The page reads the first chunk of each body by itself and may never
        // read the rest, nor cancel it.
        let mut unread = Vec::new();
        for _ in 0..3 {
            let mut response = client.get(format!("http://{address}/big")).send().await?;
            response.chunk().await?;
            unread.push(response);
        }

        let small = async {
            let response = client.get(format!("http://{address}/small")).send().await?;
            response.bytes().await
        };
        let body = tokio::time::timeout(Duration::from_secs(10), small).await??;
        assert_eq!(&body[..], b"ok");

        drop(unread);
        Ok(())
    })
}

#[test]
fn configured_directory_persists_cookies_across_restart() -> TestResult {
    let directory = tempfile::tempdir()?;
    let storage_dir = directory.path().join("support");
    let path = storage_dir.join(".cookies");
    let url = "https://example.com/".parse()?;
    let app = mock_builder()
        .plugin(init_with_cookie_store_dir(&storage_dir))
        .build(mock_context(noop_assets()))?;
    {
        let http = app.state::<Http>();
        assert_eq!(http.cookies_jar.path, path);
        let cookie =
            HeaderValue::from_static("session=test; Path=/; Max-Age=3600; Secure; HttpOnly");
        http.cookies_jar
            .set_cookies(&mut [&cookie].into_iter(), &url);
        http.cookies_jar.request_save()?.recv()?;
    }
    drop(app);

    let app = mock_builder()
        .plugin(init_with_cookie_store_dir(&storage_dir))
        .build(mock_context(noop_assets()))?;
    assert_eq!(
        app.state::<Http>().cookies_jar.cookies(&url),
        Some(HeaderValue::from_static("session=test"))
    );
    Ok(())
}

#[test]
fn resolves_tauri_directory_variables() -> TestResult {
    let directory = tempfile::tempdir()?;
    let relative = directory.path().strip_prefix(std::env::temp_dir())?;
    let storage_dir = std::path::PathBuf::from("$TEMP")
        .join(relative)
        .join("nested");
    let app = mock_builder()
        .plugin(init_with_cookie_store_dir(storage_dir))
        .build(mock_context(noop_assets()))?;
    let expected = directory.path().join("nested").join(".cookies");
    assert_eq!(app.state::<Http>().cookies_jar.path, expected);
    assert!(expected.is_file());
    Ok(())
}

#[test]
fn invalid_cookie_directory_fails_setup_without_falling_back() -> TestResult {
    let directory = tempfile::tempdir()?;
    let parent = directory.path().join("not-a-directory");
    std::fs::write(&parent, "keep this file")?;
    let result = mock_builder()
        .plugin(init_with_cookie_store_dir(&parent))
        .build(mock_context(noop_assets()));
    assert!(result.is_err());
    assert_eq!(std::fs::read_to_string(parent)?, "keep this file");
    Ok(())
}

/// One step of a [`ScriptedBody`].
enum Step {
    Data(Vec<u8>),
    /// Not ready on this poll; ready again right after.
    Pending,
    Trailers,
    Error,
}

/// A response body that answers each poll with its next step, then ends.
struct ScriptedBody {
    steps: std::collections::VecDeque<Step>,
    ended: bool,
}

impl hyper::body::Body for ScriptedBody {
    type Data = hyper::body::Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<std::result::Result<hyper::body::Frame<Self::Data>, Self::Error>>>
    {
        use hyper::body::Frame;
        use std::task::Poll;

        assert!(!self.ended, "the body was polled after its end");
        match self.steps.pop_front() {
            Some(Step::Data(data)) => Poll::Ready(Some(Ok(Frame::data(data.into())))),
            Some(Step::Pending) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Some(Step::Trailers) => Poll::Ready(Some(Ok(Frame::trailers(http::HeaderMap::new())))),
            Some(Step::Error) => Poll::Ready(Some(Err(std::io::Error::other("connection reset")))),
            None => {
                self.ended = true;
                Poll::Ready(None)
            }
        }
    }
}

fn scripted(steps: Vec<Step>) -> ReqwestResponse {
    let body = ScriptedBody {
        steps: steps.into(),
        ended: false,
    };
    ReqwestResponse::new(http::Response::new(reqwest::Body::wrap(body)).into())
}

/// Every read until the end of the body or an error, as the page makes them.
async fn read_all_async(
    response: &mut ReqwestResponse,
    cap: usize,
) -> (Vec<Vec<u8>>, Option<crate::Error>) {
    let mut reads = Vec::new();
    loop {
        match response.read(cap).await {
            Ok(Some(data)) => reads.push(data),
            Ok(None) => return (reads, None),
            Err(error) => return (reads, Some(error)),
        }
    }
}

fn read_all(response: &mut ReqwestResponse, cap: usize) -> (Vec<Vec<u8>>, Option<crate::Error>) {
    tauri::async_runtime::block_on(read_all_async(response, cap))
}

fn bytes(text: &str) -> Step {
    Step::Data(text.as_bytes().to_vec())
}

#[test]
fn a_read_takes_every_chunk_that_is_ready_and_waits_for_none() {
    let mut response = scripted(vec![
        bytes("a"),
        bytes("b"),
        Step::Trailers,
        bytes("c"),
        Step::Pending,
        bytes("d"),
        Step::Pending,
        Step::Pending,
        bytes("e"),
    ]);

    let (reads, error) = read_all(&mut response, READ_BODY_CAP);

    assert_eq!(reads, [b"abc".to_vec(), b"d".to_vec(), b"e".to_vec()]);
    assert!(error.is_none());
}

#[test]
fn a_read_stays_within_the_cap() {
    let mut response = scripted(vec![
        bytes("12345"),
        bytes("1234"),
        bytes("123"),
        bytes("1"),
    ]);
    let (reads, _) = read_all(&mut response, 10);
    assert_eq!(reads, [b"123451234".to_vec(), b"1231".to_vec()]);

    // A first chunk larger than the cap is split.
    let mut response = scripted(vec![bytes("1234567890123456789012345"), bytes("1")]);
    let (reads, _) = read_all(&mut response, 10);
    assert_eq!(
        reads,
        [
            b"1234567890".to_vec(),
            b"1234567890".to_vec(),
            b"123451".to_vec()
        ]
    );
}

#[test]
fn an_error_after_data_ends_the_next_read() {
    let mut response = scripted(vec![bytes("a"), bytes("b"), Step::Error, bytes("never")]);

    let (reads, error) = read_all(&mut response, READ_BODY_CAP);

    assert_eq!(reads, [b"ab".to_vec()]);
    assert!(matches!(error, Some(crate::Error::Network(_))));
}

#[test]
fn an_error_before_any_data_ends_the_first_read() {
    let mut response = scripted(vec![Step::Pending, Step::Error]);
    let (reads, error) = read_all(&mut response, READ_BODY_CAP);
    assert!(reads.is_empty());
    assert!(error.is_some());
}

#[test]
fn the_end_found_by_a_read_is_reported_by_the_next_without_polling_again() {
    // `ScriptedBody` panics if polled after its end.
    let mut response = scripted(vec![bytes("a"), bytes("b")]);
    let (reads, error) = read_all(&mut response, READ_BODY_CAP);
    assert_eq!(reads, [b"ab".to_vec()]);
    assert!(error.is_none());

    let mut response = scripted(vec![]);
    let (reads, error) = read_all(&mut response, READ_BODY_CAP);
    assert!(reads.is_empty());
    assert!(error.is_none());
}

/// `len` bytes that are not all the same, so misordered data shows.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// `data` as gzip with stored (uncompressed) deflate blocks.
#[cfg(feature = "gzip")]
fn gzip(data: &[u8]) -> Vec<u8> {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    let mut blocks = data.chunks(0xffff).peekable();
    while let Some(block) = blocks.next() {
        out.push(u8::from(blocks.peek().is_none()));
        let len = block.len() as u16;
        out.extend(len.to_le_bytes());
        out.extend((!len).to_le_bytes());
        out.extend(block);
    }
    out.extend((!crc).to_le_bytes());
    out.extend((data.len() as u32).to_le_bytes());
    out
}

/// A body the test sends piece by piece.
struct ChannelBody(tokio::sync::mpsc::UnboundedReceiver<hyper::body::Bytes>);

impl hyper::body::Body for ChannelBody {
    type Data = hyper::body::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<std::result::Result<hyper::body::Frame<Self::Data>, Self::Error>>>
    {
        self.0
            .poll_recv(cx)
            .map(|piece| piece.map(|piece| Ok(hyper::body::Frame::data(piece))))
    }
}

type Pieces = tokio::sync::mpsc::UnboundedSender<hyper::body::Bytes>;

/// An HTTP/2 server that answers one request with `headers` and a body the
/// test sends through the returned sender.
#[cfg(feature = "http2")]
async fn serve_http2_pieces(
    headers: &'static [(&'static str, &'static str)],
) -> std::io::Result<(SocketAddr, tokio::sync::oneshot::Receiver<Pieces>)> {
    use hyper::{service::service_fn, Request, Response};
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (on_request, requested) = tokio::sync::oneshot::channel();
    tauri::async_runtime::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let on_request = std::sync::Mutex::new(Some(on_request));
        let service = service_fn(move |_: Request<hyper::body::Incoming>| {
            let (pieces, body) = tokio::sync::mpsc::unbounded_channel();
            if let Some(on_request) = on_request.lock().unwrap().take() {
                let _ = on_request.send(pieces);
            }
            let mut response = Response::new(ChannelBody(body));
            for (name, value) in headers {
                response
                    .headers_mut()
                    .insert(*name, HeaderValue::from_static(value));
            }
            async move { Ok::<_, hyper::Error>(response) }
        });
        let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(stream), service)
            .await;
    });
    Ok((address, requested))
}

/// Sends a GET with the shared client's settings over HTTP/2 without TLS to
/// a server answering with `headers`. Returns the response, once its headers
/// arrived, and the sender of its body.
#[cfg(feature = "http2")]
async fn get_http2_pieces(
    http: &Http,
    headers: &'static [(&'static str, &'static str)],
) -> std::result::Result<(ReqwestResponse, Pieces), TestError> {
    let (address, requested) = serve_http2_pieces(headers).await?;
    let client = http.client_builder().http2_prior_knowledge().build()?;
    let (response, pieces) =
        tokio::join!(client.get(format!("http://{address}/")).send(), requested);
    Ok((ReqwestResponse::new(response?), pieces?))
}

/// Gives the client time to receive what the server sent.
async fn let_it_arrive() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[cfg(feature = "http2")]
async fn read_now(
    response: &mut ReqwestResponse,
) -> std::result::Result<Option<Vec<u8>>, TestError> {
    Ok(tokio::time::timeout(Duration::from_secs(10), response.read(READ_BODY_CAP)).await??)
}

#[cfg(feature = "http2")]
#[test]
fn a_received_body_comes_back_in_one_read() -> TestResult {
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();
    let body = pattern(300 * 1024);

    tauri::async_runtime::block_on(async {
        let (mut response, pieces) = get_http2_pieces(&http, &[]).await?;
        for piece in body.chunks(7 * 1024) {
            pieces.send(piece.to_vec().into())?;
        }
        drop(pieces);
        let_it_arrive().await;

        assert_eq!(read_now(&mut response).await?, Some(body.clone()));
        assert_eq!(read_now(&mut response).await?, None);
        Ok(())
    })
}

#[cfg(all(feature = "http2", feature = "gzip"))]
#[test]
fn a_received_gzip_body_comes_back_in_one_read() -> TestResult {
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();
    let body = pattern(300 * 1024);

    tauri::async_runtime::block_on(async {
        let (mut response, pieces) =
            get_http2_pieces(&http, &[("content-encoding", "gzip")]).await?;
        pieces.send(gzip(&body).into())?;
        drop(pieces);
        let_it_arrive().await;

        assert_eq!(read_now(&mut response).await?, Some(body.clone()));
        assert_eq!(read_now(&mut response).await?, None);
        Ok(())
    })
}

#[cfg(feature = "http2")]
#[test]
fn a_slow_body_streams_each_piece_as_it_arrives() -> TestResult {
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();

    tauri::async_runtime::block_on(async {
        let (mut response, pieces) = get_http2_pieces(&http, &[]).await?;

        // Each read returns what has arrived, while the server holds the rest.
        pieces.send("first".into())?;
        assert_eq!(read_now(&mut response).await?, Some(b"first".to_vec()));
        pieces.send("second".into())?;
        assert_eq!(read_now(&mut response).await?, Some(b"second".to_vec()));

        // A read that starts before the next piece waits for it.
        let reading = read_now(&mut response);
        let sending_later = async {
            let_it_arrive().await;
            pieces.send("third".into())
        };
        let (read, sent) = tokio::join!(reading, sending_later);
        sent?;
        assert_eq!(read?, Some(b"third".to_vec()));

        // HTTP/2 ends a streamed body with an empty frame, which a read
        // returns as an empty chunk.
        drop(pieces);
        let (rest, error) = read_all_async(&mut response, READ_BODY_CAP).await;
        assert!(rest.concat().is_empty());
        assert!(error.is_none());
        Ok(())
    })
}

#[cfg(feature = "http2")]
#[test]
fn a_large_received_body_comes_back_a_cap_at_a_time() -> TestResult {
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();
    let body = pattern(3 * READ_BODY_CAP + 1000);

    tauri::async_runtime::block_on(async {
        let (mut response, pieces) = get_http2_pieces(&http, &[]).await?;
        for piece in body.chunks(64 * 1024) {
            pieces.send(piece.to_vec().into())?;
        }
        drop(pieces);

        let mut read = Vec::new();
        while let Some(data) = read_now(&mut response).await? {
            assert!(data.len() <= READ_BODY_CAP, "read {} bytes", data.len());
            read.extend(data);
            let_it_arrive().await;
        }
        assert_eq!(read, body);
        Ok(())
    })
}

#[test]
fn a_connection_lost_mid_body_fails_the_read_instead_of_ending_the_body() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}/", listener.local_addr()?);
    let server = std::thread::spawn(move || -> std::io::Result<()> {
        let (mut stream, _) = listener.accept()?;
        let mut request = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        while request.read_line(&mut line)? > 2 {
            line.clear();
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\npartial")?;
        stream.shutdown(Shutdown::Both)
    });
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();

    let (reads, error) = tauri::async_runtime::block_on(async {
        let (client, _) = request_client(&http, ClientOptions::default())?;
        let mut response = ReqwestResponse::new(client.get(&url).send().await?);
        let_it_arrive().await;
        Ok::<_, TestError>(read_all_async(&mut response, READ_BODY_CAP).await)
    })?;
    server.join().expect("server thread")?;

    assert_eq!(reads.concat(), b"partial");
    assert!(error.is_some());
    Ok(())
}
