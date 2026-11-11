// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use super::{init_with_cookie_store_dir, Http};
use reqwest::{cookie::CookieStore, header::HeaderValue};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tauri::{
    test::{mock_builder, mock_context, noop_assets, MockRuntime},
    Manager,
};

type TestResult = std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// An HTTP server that keeps its connections open and counts them.
struct Server {
    url: String,
    connections: Arc<AtomicUsize>,
}

impl Server {
    fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}/", listener.local_addr()?);
        let connections = Arc::new(AtomicUsize::new(0));
        let accepted = connections.clone();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Ok(requests) = stream.try_clone() else {
                    return;
                };
                accepted.fetch_add(1, Ordering::SeqCst);

                // Answer every request on the connection until the client closes it.
                std::thread::spawn(move || {
                    let mut lines = BufReader::new(requests).lines();
                    while let Some(Ok(line)) = lines.next() {
                        if !line.is_empty() {
                            continue;
                        }
                        let response = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
                        if stream.write_all(response).is_err() {
                            return;
                        }
                    }
                });
            }
        });

        Ok(Self { url, connections })
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// An app with the plugin, and the cookie directory it must outlive.
type TestApp = (tauri::App<MockRuntime>, tempfile::TempDir);

fn app() -> std::result::Result<TestApp, Box<dyn std::error::Error + Send + Sync>> {
    let directory = tempfile::tempdir()?;
    let app = mock_builder()
        .plugin(init_with_cookie_store_dir(directory.path()))
        .build(mock_context(noop_assets()))?;
    Ok((app, directory))
}

async fn get(http: &Http, url: &str) -> reqwest::Result<()> {
    // Reading the body returns the connection to the pool.
    http.client().get(url).send().await?.bytes().await?;
    Ok(())
}

#[test]
fn requests_share_a_connection() -> TestResult {
    let server = Server::start()?;
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();

    tauri::async_runtime::block_on(async {
        get(&http, &server.url).await?;
        get(&http, &server.url).await
    })?;

    assert_eq!(server.connections(), 1);
    Ok(())
}

#[test]
fn reset_connections_opens_a_new_connection() -> TestResult {
    let server = Server::start()?;
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();

    tauri::async_runtime::block_on(get(&http, &server.url))?;
    http.reset_connections()?;
    tauri::async_runtime::block_on(get(&http, &server.url))?;

    assert_eq!(server.connections(), 2);
    Ok(())
}

#[test]
fn only_a_failure_to_connect_replaces_the_client() -> TestResult {
    let server = Server::start()?;
    // Nothing listens on a port whose listener was dropped.
    let closed = format!(
        "http://{}/",
        TcpListener::bind("127.0.0.1:0")?.local_addr()?
    );
    let (app, _cookies) = app()?;
    let http = app.state::<Http>();

    tauri::async_runtime::block_on(get(&http, &server.url))?;

    let Err(invalid_request) = http.client().get("http://").build() else {
        return Err("a URL without a host was accepted".into());
    };
    http.refresh_after_connect_error(&invalid_request);
    tauri::async_runtime::block_on(get(&http, &server.url))?;
    assert_eq!(server.connections(), 1);

    let Err(refused) = tauri::async_runtime::block_on(get(&http, &closed)) else {
        return Err("a closed port accepted a connection".into());
    };
    http.refresh_after_connect_error(&refused);
    tauri::async_runtime::block_on(get(&http, &server.url))?;
    assert_eq!(server.connections(), 2);
    Ok(())
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
