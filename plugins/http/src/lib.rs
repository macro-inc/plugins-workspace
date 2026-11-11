// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Access the HTTP client written in Rust.

pub use reqwest;
use std::{
    path::PathBuf,
    sync::{PoisonError, RwLock},
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
const HTTP2_KEEP_ALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// How long the probe may go unanswered. A congested mobile link can take
/// seconds to answer while it is still working, so this is generous.
#[cfg(feature = "http2")]
const HTTP2_KEEP_ALIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

pub struct Http {
    #[cfg(feature = "cookies")]
    pub cookies_jar: std::sync::Arc<crate::reqwest_cookie_store::CookieStoreMutex>,
    /// One client for every request, so requests to a host share its
    /// connections instead of each paying a TCP and TLS handshake.
    client: RwLock<reqwest::Client>,
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
    pub(crate) fn client(&self) -> reqwest::Client {
        self.client
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stops reusing the open connections, so the next request to each host
    /// opens a new one. Requests already in flight finish on theirs.
    ///
    /// Call this when connections may have died without either end being
    /// told, such as when a mobile app returns from the background. It also
    /// reads the system proxy settings again.
    pub fn reset_connections(&self) -> Result<()> {
        let client = self.client_builder().build()?;
        *self.client.write().unwrap_or_else(PoisonError::into_inner) = client;
        Ok(())
    }

    /// Replaces the client after a request could not connect at all. The
    /// client reads the system proxy settings when it is built, so failing
    /// to connect can mean they have changed since.
    pub(crate) fn refresh_after_connect_error(&self, error: &reqwest::Error) {
        if !error.is_connect() {
            return;
        }

        // A client that cannot be built leaves the current one in place; the
        // request that got here already reports its own failure.
        let _ = self.reset_connections();
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
        .http2_keep_alive_timeout(HTTP2_KEEP_ALIVE_TIMEOUT);

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
