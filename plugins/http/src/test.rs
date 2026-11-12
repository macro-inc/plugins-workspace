// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use super::{
    commands::{request_client, ClientOptions},
    init_with_cookie_store_dir, Http,
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

/// An HTTP/1.1 server that counts the connections it accepts.
struct Server {
    url: String,
    address: SocketAddr,
    connections: Arc<AtomicUsize>,
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
        let stopping = Arc::new(AtomicBool::new(false));
        let accepted = connections.clone();
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
                accepted.fetch_add(1, Ordering::SeqCst);

                // Answer every request on the connection until either end closes it.
                std::thread::spawn(move || {
                    let mut lines = BufReader::new(requests).lines();
                    while let Some(Ok(line)) = lines.next() {
                        if !line.is_empty() {
                            continue;
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
            stopping,
            accepting: Some(accepting),
        })
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
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
fn failing_to_connect_to_an_origin_reached_before_replaces_the_client() -> TestResult {
    let server = Server::start()?;
    let mut stopped = Server::start_closing_connections()?;
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();

    get_shared(&http, &server.url)?;
    get_shared(&http, &stopped.url)?;
    stopped.stop();
    assert!(get_shared(&http, &stopped.url).is_err());
    get_shared(&http, &server.url)?;

    assert_eq!(server.connections(), 2);

    // The replacement has not reached that origin, so failing again changes nothing.
    assert!(get_shared(&http, &stopped.url).is_err());
    get_shared(&http, &server.url)?;
    assert_eq!(server.connections(), 2);
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
