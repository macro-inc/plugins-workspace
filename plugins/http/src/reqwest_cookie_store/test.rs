use std::sync::Arc;
use std::thread;

use super::*;

fn test_url() -> url::Url {
    url::Url::parse("https://example.com/").unwrap()
}

/// Insert a persistent cookie through the real `reqwest::cookie::CookieStore`
/// impl, so the production save path (request_save on every Set-Cookie) runs.
fn insert_cookie(store: &CookieStoreMutex, name_value: &str) {
    let header = HeaderValue::from_str(&format!(
        "{name_value}; Domain=example.com; Path=/; Max-Age=3600"
    ))
    .unwrap();
    reqwest::cookie::CookieStore::set_cookies(store, &mut std::iter::once(&header), &test_url());
}

/// Request a save and block until that snapshot (or newer) is on disk.
fn flush(store: &CookieStoreMutex) {
    store
        .request_save()
        .expect("failed to serialize cookie store")
        .recv()
        .expect("cookie store save failed");
}

fn cookie_count(store: &CookieStoreMutex) -> usize {
    store.store.lock().unwrap().iter_unexpired().count()
}

#[test]
fn missing_file_loads_empty_store_without_creating_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".cookies");

    let store = CookieStoreMutex::load_or_quarantine(path.clone());

    assert_eq!(cookie_count(&store), 0);
    assert!(!path.exists());
    assert!(!sibling_path(&path, "corrupt").exists());
}

#[test]
fn empty_file_loads_empty_store_without_quarantine() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".cookies");
    std::fs::write(&path, "").unwrap();

    let store = CookieStoreMutex::load_or_quarantine(path.clone());

    assert_eq!(cookie_count(&store), 0);
    assert!(path.exists());
    assert!(!sibling_path(&path, "corrupt").exists());
}

#[test]
fn saved_cookies_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".cookies");

    let store = CookieStoreMutex::new(path.clone(), CookieStore::default());
    insert_cookie(&store, "session=abc123");
    flush(&store);

    assert!(!sibling_path(&path, "tmp").exists());

    let reloaded = CookieStoreMutex::load_or_quarantine(path);
    let header = reqwest::cookie::CookieStore::cookies(&reloaded, &test_url())
        .expect("expected cookies for url after reload");
    assert_eq!(header.to_str().unwrap(), "session=abc123");
}

#[test]
fn corrupt_file_is_preserved_aside_and_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".cookies");
    let corrupt_path = sibling_path(&path, "corrupt");
    std::fs::write(&path, "definitely not json").unwrap();

    let store = CookieStoreMutex::load_or_quarantine(path.clone());

    // the unreadable file was moved aside intact, not left in place
    assert!(!path.exists());
    assert_eq!(
        std::fs::read_to_string(&corrupt_path).unwrap(),
        "definitely not json"
    );
    assert_eq!(cookie_count(&store), 0);

    // subsequent saves write the main path and leave the quarantined file alone
    insert_cookie(&store, "session=new");
    flush(&store);
    assert!(path.exists());
    assert_eq!(
        std::fs::read_to_string(&corrupt_path).unwrap(),
        "definitely not json"
    );
}

#[test]
fn migration_moves_legacy_file_to_new_location() {
    let legacy_dir = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let legacy_path = legacy_dir.path().join(".cookies");
    let path = data_dir.path().join(".cookies");

    // produce a real legacy jar by saving a store at the legacy location
    let legacy_store = CookieStoreMutex::new(legacy_path.clone(), CookieStore::default());
    insert_cookie(&legacy_store, "session=migrated");
    flush(&legacy_store);

    migrate_legacy_cookie_file(&legacy_path, &path);

    assert!(!legacy_path.exists());
    let reloaded = CookieStoreMutex::load_or_quarantine(path);
    let header = reqwest::cookie::CookieStore::cookies(&reloaded, &test_url())
        .expect("expected migrated cookies");
    assert_eq!(header.to_str().unwrap(), "session=migrated");
}

#[test]
fn migration_never_clobbers_an_existing_file() {
    let legacy_dir = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let legacy_path = legacy_dir.path().join(".cookies");
    let path = data_dir.path().join(".cookies");
    std::fs::write(&legacy_path, "legacy contents").unwrap();
    std::fs::write(&path, "current contents").unwrap();

    migrate_legacy_cookie_file(&legacy_path, &path);

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "current contents");
    assert_eq!(
        std::fs::read_to_string(&legacy_path).unwrap(),
        "legacy contents"
    );
}

#[test]
fn migration_is_a_noop_with_nothing_to_migrate() {
    let legacy_dir = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let legacy_path = legacy_dir.path().join(".cookies");
    let path = data_dir.path().join(".cookies");

    migrate_legacy_cookie_file(&legacy_path, &path);

    assert!(!legacy_path.exists());
    assert!(!path.exists());
}

#[test]
fn concurrent_saves_serialize_and_leave_a_valid_file() {
    const THREADS: usize = 8;
    const COOKIES_PER_THREAD: usize = 25;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".cookies");
    let store = Arc::new(CookieStoreMutex::new(path.clone(), CookieStore::default()));

    // every insert_cookie triggers an internal request_save, so this hammers
    // the writer with overlapping save requests from many threads
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                for i in 0..COOKIES_PER_THREAD {
                    insert_cookie(&store, &format!("cookie_t{t}_i{i}=value"));
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    // this snapshot contains all cookies; recv returns once it (or newer) is
    // durable, which is exactly the exit-flush code path
    flush(&store);

    assert!(!sibling_path(&path, "tmp").exists());
    let reloaded = CookieStoreMutex::load_or_quarantine(path);
    assert_eq!(cookie_count(&reloaded), THREADS * COOKIES_PER_THREAD);
}

#[test]
fn failed_save_unblocks_waiters_instead_of_hanging() {
    let dir = tempfile::tempdir().unwrap();
    // parent directory does not exist, so every write fails
    let path = dir.path().join("missing-subdir").join(".cookies");

    let store = CookieStoreMutex::new(path, CookieStore::default());
    insert_cookie(&store, "session=doomed");

    let rx = store.request_save().unwrap();
    // sender is dropped on write failure -> recv errors rather than hanging
    assert!(rx.recv().is_err());
}
