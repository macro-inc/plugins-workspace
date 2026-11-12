// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Access the HTTP client written in Rust.

pub use reqwest;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Mutex, PoisonError, RwLock},
    time::{Duration, Instant, SystemTime},
};
use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};

pub use error::{Error, Result};

mod commands;
mod error;
#[cfg(feature = "cookies")]
mod reqwest_cookie_store;
mod scope;

#[cfg(all(test, feature = "cookies"))]
mod test;

/// How long a request waits without hearing from the server before the
/// connection is probed.
#[cfg(feature = "http2")]
const HTTP2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// How long the probe may go unanswered before the connection is dropped. The
/// answer queues behind data already in flight, so on a slow link a large
/// upload or a burst of responses can delay it by many seconds; a shorter
/// wait drops connections that are slow, not dead, and fails every request on
/// them. With the interval above, requests wait at most twenty-five seconds
/// on a connection that died without either end being told.
#[cfg(feature = "http2")]
const HTTP2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Unread response data a connection may hold across all of its requests.
/// Each request is already limited to 2 MB by its own window. The default
/// connection limit of 5 MB lets three responses that are never read stop
/// every other response on the connection, so this is far above what the
/// requests sharing a connection can hold between them.
#[cfg(feature = "http2")]
const HTTP2_CONNECTION_WINDOW: u32 = 1 << 30;

/// A sleep at least this long drops the pooled connections.
const SLEEP_RESET: Duration = Duration::from_secs(10);

/// Whether building a client reads the system proxy settings again. Elsewhere
/// the proxy comes from the environment, which does not change while the app
/// runs, so a new client would differ from the old one only in having lost
/// its connections.
const REREADS_SYSTEM_PROXY: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// When the shared client was last asked for.
#[derive(Clone, Copy)]
struct LastUse {
    /// By the wall clock, which keeps running while the machine sleeps.
    wall: SystemTime,
    /// By the clock the connection pool uses, which stops during sleep.
    awake: Instant,
}

impl LastUse {
    fn now() -> Self {
        Self {
            wall: SystemTime::now(),
            awake: Instant::now(),
        }
    }

    /// How long the machine slept between `self` and `later`: how far the
    /// wall clock ran ahead of the other. A wall clock set backwards counts
    /// as no sleep.
    fn slept_until(&self, later: &Self) -> Duration {
        let wall = later.wall.duration_since(self.wall).unwrap_or_default();
        let awake = later.awake.duration_since(self.awake);
        wall.saturating_sub(awake)
    }
}

pub struct Http {
    #[cfg(feature = "cookies")]
    pub cookies_jar: std::sync::Arc<crate::reqwest_cookie_store::CookieStoreMutex>,
    /// One client for every request, so requests to a host share its
    /// connections instead of each paying a TCP and TLS handshake.
    client: RwLock<reqwest::Client>,
    /// Origins the current client has connected to.
    reached: Mutex<HashSet<String>>,
    last_use: Mutex<LastUse>,
}

impl Http {
    fn new(
        #[cfg(feature = "cookies")] cookies_jar: std::sync::Arc<
            crate::reqwest_cookie_store::CookieStoreMutex,
        >,
    ) -> reqwest::Result<Self> {
        #[cfg(feature = "cookies")]
        let client = client_builder(&cookies_jar).build()?;
        #[cfg(not(feature = "cookies"))]
        let client = client_builder().build()?;

        Ok(Self {
            #[cfg(feature = "cookies")]
            cookies_jar,
            client: RwLock::new(client),
            reached: Mutex::default(),
            last_use: Mutex::new(LastUse::now()),
        })
    }

    /// A builder with the settings every request shares.
    pub(crate) fn client_builder(&self) -> reqwest::ClientBuilder {
        #[cfg(feature = "cookies")]
        return client_builder(&self.cookies_jar);
        #[cfg(not(feature = "cookies"))]
        return client_builder();
    }

    /// The client whose connections requests share.
    ///
    /// The pool expires idle connections by a clock that stops while the
    /// machine sleeps, so after a sleep it would reuse connections the server
    /// and the network gave up on long before. If the machine slept since the
    /// client was last asked for, the connections are dropped first. Where
    /// that clock runs through sleep, the pool's own idle limit covers it.
    pub(crate) fn client(&self) -> reqwest::Client {
        let now = LastUse::now();
        let previous = std::mem::replace(
            &mut *self.last_use.lock().unwrap_or_else(PoisonError::into_inner),
            now,
        );
        if previous.slept_until(&now) >= SLEEP_RESET {
            // A client that cannot be built leaves the current one in place.
            let _ = self.reset_connections();
        }

        self.client
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Moves the last use back as if the machine had been idle for `idle`,
    /// of which it slept for `asleep`.
    #[cfg(test)]
    pub(crate) fn backdate_last_use(&self, idle: Duration, asleep: Duration) {
        let mut last_use = self.last_use.lock().unwrap_or_else(PoisonError::into_inner);
        last_use.wall -= idle;
        last_use.awake -= idle - asleep;
    }

    /// Stops reusing the open connections, so the next request to each host
    /// opens a new one. Requests already in flight finish on theirs.
    ///
    /// Call this when connections may have died without either end being
    /// told, such as when a mobile app returns from the background or its
    /// network changes. On macOS and Windows it also reads the system proxy
    /// settings again.
    pub fn reset_connections(&self) -> Result<()> {
        let client = self.client_builder().build()?;
        *self.client.write().unwrap_or_else(PoisonError::into_inner) = client;
        self.reached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        Ok(())
    }

    /// Sends a request built with the shared client.
    ///
    /// Where the client reads the system proxy settings when it is built,
    /// they may have changed when it can no longer connect to an origin it
    /// reached before: the client is replaced. Failing to connect anywhere
    /// else, such as to a server that is not running, says nothing about the
    /// connections to other hosts and leaves them alone. So does any failure
    /// on a platform where a new client would read nothing new.
    pub(crate) async fn send_shared(
        &self,
        request: reqwest::RequestBuilder,
    ) -> reqwest::Result<reqwest::Response> {
        if !REREADS_SYSTEM_PROXY {
            return request.send().await;
        }

        let (client, request) = request.build_split();
        let request = request?;
        let origin = request.url().origin().ascii_serialization();

        let result = client.execute(request).await;

        let mut reached = self.reached.lock().unwrap_or_else(PoisonError::into_inner);
        match &result {
            Ok(_) => {
                reached.insert(origin);
            }
            Err(error) if error.is_connect() && reached.contains(&origin) => {
                drop(reached);
                // A client that cannot be built leaves the current one in
                // place; this request already reports its own failure.
                let _ = self.reset_connections();
            }
            Err(_) => {}
        }

        result
    }
}

fn client_builder(
    #[cfg(feature = "cookies")] cookies_jar: &std::sync::Arc<
        crate::reqwest_cookie_store::CookieStoreMutex,
    >,
) -> reqwest::ClientBuilder {
    let builder = reqwest::ClientBuilder::new();

    #[cfg(feature = "cookies")]
    let builder = builder.cookie_provider(cookies_jar.clone());

    // Every request to a host shares one HTTP/2 connection. If that connection
    // dies silently they would all wait on it, so probe it when it goes quiet.
    #[cfg(feature = "http2")]
    let builder = builder
        .http2_keep_alive_interval(HTTP2_KEEP_ALIVE_INTERVAL)
        .http2_keep_alive_timeout(HTTP2_KEEP_ALIVE_TIMEOUT)
        .http2_initial_connection_window_size(HTTP2_CONNECTION_WINDOW);

    builder
}

/// Initializes the HTTP plugin with cookies stored at `$APPCACHE/.cookies`.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    init_with_cookie_store_dir("$APPCACHE")
}

/// Initializes the HTTP plugin with a caller-selected cookie storage directory.
///
/// The plugin stores its jar in `.cookies` inside the directory. Tauri
/// base-directory variables such as `$APPLOCALDATA` are resolved during setup,
/// and the directory is created if missing. Existing cookies are not migrated
/// from the default location. The directory is unused without `cookies`.
pub fn init_with_cookie_store_dir<R: Runtime>(directory: impl Into<PathBuf>) -> TauriPlugin<R> {
    let _cookie_store_dir = directory.into();
    Builder::<R>::new("http")
        .setup(move |app, _| {
            #[cfg(feature = "cookies")]
            let cookies_jar = {
                use crate::reqwest_cookie_store::*;
                use std::fs::File;
                use std::io::BufReader;

                let directory = app.path().parse(&_cookie_store_dir)?;
                std::fs::create_dir_all(&directory)?;
                let path = directory.join(".cookies");
                let file = File::options()
                    .create(true)
                    .append(true)
                    .read(true)
                    .open(&path)?;

                let reader = BufReader::new(file);
                CookieStoreMutex::load(path.clone(), reader).unwrap_or_else(|_e| {
                    #[cfg(feature = "tracing")]
                    tracing::warn!(
                        "failed to load cookie store: {_e}, falling back to empty store"
                    );
                    CookieStoreMutex::new(path, Default::default())
                })
            };

            let state = Http::new(
                #[cfg(feature = "cookies")]
                std::sync::Arc::new(cookies_jar),
            )?;

            app.manage(state);

            Ok(())
        })
        .on_event(|app, event| {
            #[cfg(feature = "cookies")]
            if let tauri::RunEvent::Exit = event {
                let state = app.state::<Http>();

                match state.cookies_jar.request_save() {
                    Ok(rx) => {
                        let _ = rx.recv();
                    }
                    Err(_e) => {
                        #[cfg(feature = "tracing")]
                        tracing::error!("failed to save cookie jar: {_e}");
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::fetch,
            commands::fetch_cancel,
            commands::fetch_send,
            commands::fetch_read_body,
            commands::fetch_cancel_body,
        ])
        .build()
}
