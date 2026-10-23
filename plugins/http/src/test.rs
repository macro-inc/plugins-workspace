// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use super::{init_with_cookie_store_dir, Http};
use reqwest::{cookie::CookieStore, header::HeaderValue};
use tauri::{
    test::{mock_builder, mock_context, noop_assets},
    Manager,
};

type TestResult = std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>;

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
