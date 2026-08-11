// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

// taken from https://github.com/pfernie/reqwest_cookie_store/blob/2ec4afabcd55e24d3afe3f0626ee6dc97bed938d/src/lib.rs

use std::{
    path::{Path, PathBuf},
    sync::{
        mpsc::{Receiver, Sender},
        Arc, Mutex,
    },
};

use cookie_store::{CookieStore, RawCookie, RawCookieParseError};
use reqwest::header::HeaderValue;
use tokio::io::AsyncWriteExt;

#[cfg(test)]
mod test;

fn set_cookies(
    cookie_store: &mut CookieStore,
    cookie_headers: &mut dyn Iterator<Item = &HeaderValue>,
    url: &url::Url,
) {
    let cookies = cookie_headers.filter_map(|val| {
        std::str::from_utf8(val.as_bytes())
            .map_err(RawCookieParseError::from)
            .and_then(RawCookie::parse)
            .map(|c| c.into_owned())
            .ok()
    });
    cookie_store.store_response_cookies(cookies, url);
}

fn cookies(cookie_store: &CookieStore, url: &url::Url) -> Option<HeaderValue> {
    let s = cookie_store
        .get_request_values(url)
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");

    if s.is_empty() {
        return None;
    }

    HeaderValue::from_maybe_shared(bytes::Bytes::from(s)).ok()
}

/// A [`cookie_store::CookieStore`] wrapped internally by a [`std::sync::Mutex`], suitable for use in
/// async/concurrent contexts.
///
/// The store persists itself to `path`. Writes are atomic (temp file + rename
/// in the same directory) and serialized through a single writer task, so a
/// save can never truncate the file or race an earlier save.
#[derive(Debug)]
pub struct CookieStoreMutex {
    pub path: PathBuf,
    store: Mutex<CookieStore>,
    save_state: Arc<Mutex<SaveState>>,
}

impl CookieStoreMutex {
    /// Create a new [`CookieStoreMutex`] from an existing [`cookie_store::CookieStore`].
    pub fn new(path: PathBuf, cookie_store: CookieStore) -> CookieStoreMutex {
        CookieStoreMutex {
            path,
            store: Mutex::new(cookie_store),
            save_state: Arc::default(),
        }
    }

    pub fn load<R: std::io::BufRead>(
        path: PathBuf,
        reader: R,
    ) -> cookie_store::Result<CookieStoreMutex> {
        cookie_store::serde::load(reader, |c| serde_json::from_str(c))
            .map(|store| CookieStoreMutex::new(path, store))
    }

    /// Load the cookie store persisted at `path`.
    ///
    /// A missing or empty file yields an empty store. A file that cannot be
    /// read or parsed is moved aside to `<file>.corrupt` so its contents are
    /// preserved instead of being clobbered by the next save, and an empty
    /// store is returned.
    pub fn load_or_quarantine(path: PathBuf) -> CookieStoreMutex {
        match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                CookieStoreMutex::new(path, CookieStore::default())
            }
            Err(_e) => {
                #[cfg(feature = "tracing")]
                tracing::error!("failed to read cookie store file: {_e}");
                quarantine_corrupt_file(&path);
                CookieStoreMutex::new(path, CookieStore::default())
            }
            Ok(bytes) if bytes.is_empty() => CookieStoreMutex::new(path, CookieStore::default()),
            Ok(bytes) => {
                CookieStoreMutex::load(path.clone(), bytes.as_slice()).unwrap_or_else(|_e| {
                    #[cfg(feature = "tracing")]
                    tracing::error!("failed to parse cookie store file: {_e}");
                    quarantine_corrupt_file(&path);
                    CookieStoreMutex::new(path, CookieStore::default())
                })
            }
        }
    }

    fn cookies_to_str(&self) -> Result<String, serde_json::Error> {
        let mut cookies = Vec::new();
        for cookie in self
            .store
            .lock()
            .expect("poisoned cookie jar mutex")
            .iter_unexpired()
        {
            if cookie.is_persistent() {
                cookies.push(cookie.clone());
            }
        }
        serde_json::to_string(&cookies)
    }

    /// Serialize the current store and queue an asynchronous, atomic write to
    /// `self.path`.
    ///
    /// Saves are serialized: at most one write is in flight at a time, a new
    /// request never cancels a write already in progress, and queued payloads
    /// are latest-wins.
    ///
    /// The returned channel receives `()` once state at least as new as the
    /// requested snapshot is on disk; if the write fails the sender is dropped,
    /// so a blocking `recv()` unblocks either way.
    pub fn request_save(&self) -> cookie_store::Result<Receiver<()>> {
        let (tx, rx) = std::sync::mpsc::channel();

        let mut state = self
            .save_state
            .lock()
            .expect("poisoned cookie save state mutex");
        // serialize while holding the save-state lock so an older snapshot can
        // never replace a newer one in `pending`
        let cookie_str = self.cookies_to_str()?;
        state.pending = Some(cookie_str);
        state.waiters.push(tx);
        if !state.writer_running {
            state.writer_running = true;
            spawn_save_writer(self.path.clone(), Arc::clone(&self.save_state));
        }
        Ok(rx)
    }
}

impl reqwest::cookie::CookieStore for CookieStoreMutex {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &url::Url) {
        set_cookies(&mut self.store.lock().unwrap(), cookie_headers, url);

        // try to persist cookies immediately asynchronously
        if let Err(_e) = self.request_save() {
            #[cfg(feature = "tracing")]
            tracing::error!("failed to save cookie jar: {_e}");
        }
    }

    fn cookies(&self, url: &url::Url) -> Option<HeaderValue> {
        let store = self.store.lock().unwrap();
        cookies(&store, url)
    }
}

/// Bookkeeping for the single background save writer.
#[derive(Debug, Default)]
struct SaveState {
    /// Latest serialized jar not yet written to disk (newer saves replace it).
    pending: Option<String>,
    /// Requesters notified once a payload at least as new as theirs is on disk.
    /// Taken together with `pending`, so every waiter's snapshot is covered by
    /// the payload it is drained with.
    waiters: Vec<Sender<()>>,
    /// Whether a writer task is currently draining `pending`.
    writer_running: bool,
}

/// Drain `pending` payloads one at a time, writing each atomically, until none
/// are left. At most one instance runs per store; `request_save` spawns a new
/// one when a payload arrives after the previous writer exited.
fn spawn_save_writer(path: PathBuf, save_state: Arc<Mutex<SaveState>>) {
    tauri::async_runtime::spawn(async move {
        loop {
            let (payload, waiters) = {
                let mut state = save_state.lock().expect("poisoned cookie save state mutex");
                match state.pending.take() {
                    Some(payload) => (payload, std::mem::take(&mut state.waiters)),
                    None => {
                        // waiters are always taken together with a payload, so
                        // there is nothing left to notify here
                        state.writer_running = false;
                        return;
                    }
                }
            };

            match write_atomic(&path, payload.as_bytes()).await {
                Ok(()) => {
                    for waiter in waiters {
                        let _ = waiter.send(());
                    }
                }
                Err(_e) => {
                    #[cfg(feature = "tracing")]
                    tracing::error!("failed to save cookie jar: {_e}");
                    // dropping `waiters` unblocks any blocked `recv()` with an
                    // error instead of hanging it
                }
            }
        }
    });
}

/// Write `data` to `path` via a temp file in the same directory followed by a
/// rename, so the destination always holds either the old or the new contents
/// — never a truncated mix — even if the process is killed mid-write.
async fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp_path = sibling_path(path, "tmp");
    match write_and_rename(&tmp_path, path, data).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            Err(e)
        }
    }
}

async fn write_and_rename(tmp_path: &Path, path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = tokio::fs::File::create(tmp_path).await?;
    file.write_all(data).await?;
    // fsync so the rename can never land before the data does
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(tmp_path, path).await
}

/// Move an unreadable cookie file aside to `<file>.corrupt` so its contents
/// are preserved for inspection or recovery instead of being overwritten by
/// the next save.
fn quarantine_corrupt_file(path: &Path) {
    let corrupt_path = sibling_path(path, "corrupt");
    match std::fs::rename(path, &corrupt_path) {
        Ok(()) => {
            #[cfg(feature = "tracing")]
            tracing::error!(
                "cookie store file at {path:?} was unreadable; preserved it at {corrupt_path:?}, starting with an empty store"
            );
        }
        Err(_e) => {
            #[cfg(feature = "tracing")]
            tracing::error!("failed to move unreadable cookie store file {path:?} aside: {_e}");
        }
    }
}

/// One-time migration of the cookie file from `legacy_path` (the old cache-dir
/// location, which iOS purges under storage pressure) to `path`. Does nothing
/// when a file already exists at `path` or there is nothing at `legacy_path`.
pub(crate) fn migrate_legacy_cookie_file(legacy_path: &Path, path: &Path) {
    if path.exists() || !legacy_path.exists() {
        return;
    }
    if std::fs::rename(legacy_path, path).is_ok() {
        return;
    }
    // rename fails when the directories are on different filesystems; copy instead
    match std::fs::copy(legacy_path, path) {
        Ok(_) => {
            let _ = std::fs::remove_file(legacy_path);
        }
        Err(_e) => {
            #[cfg(feature = "tracing")]
            tracing::error!(
                "failed to migrate cookie store from {legacy_path:?} to {path:?}: {_e}"
            );
        }
    }
}

/// `.cookies` → `.cookies.<suffix>`, staying in the same directory (required
/// for `rename` to be atomic) and appending rather than replacing, since
/// [`Path::with_extension`] would mangle dotfile names.
fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}
