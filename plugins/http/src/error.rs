// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use serde::{Serialize, Serializer};
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Network(#[from] reqwest::Error),
    #[error(transparent)]
    Http(#[from] http::Error),
    #[error(transparent)]
    HttpInvalidHeaderName(#[from] http::header::InvalidHeaderName),
    #[error(transparent)]
    HttpInvalidHeaderValue(#[from] http::header::InvalidHeaderValue),
    /// URL not allowed by the scope.
    #[error("url not allowed on the configured scope: {0}")]
    UrlNotAllowed(Url),
    #[error(transparent)]
    UrlParseError(#[from] url::ParseError),
    /// HTTP method error.
    #[error(transparent)]
    HttpMethod(#[from] http::method::InvalidMethod),
    #[error("scheme {0} not supported")]
    SchemeNotSupport(String),
    #[error("Request canceled")]
    RequestCanceled,
    #[error(transparent)]
    FsError(#[from] tauri_plugin_fs::Error),
    #[error("failed to process data url")]
    DataUrlError,
    #[error("failed to decode data url into bytes")]
    DataUrlDecodeError,
    #[error(transparent)]
    Tauri(#[from] tauri::Error),
    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("dangerous settings used but are not enabled")]
    DangerousSettings,
}

impl Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.message_with_causes())
    }
}

impl Error {
    /// The message followed by each underlying cause. reqwest's own message
    /// only says that sending failed; the DNS, connect or TLS reason that
    /// tells a caller why is in its sources, e.g. `error sending request for
    /// url (…): client error (Connect): invalid peer certificate: UnknownIssuer`.
    pub(crate) fn message_with_causes(&self) -> String {
        let mut message = self.to_string();
        let mut source = std::error::Error::source(self);
        while let Some(cause) = source {
            let text = cause.to_string();
            if !message.contains(&text) {
                message.push_str(": ");
                message.push_str(&text);
            }
            source = cause.source();
        }
        message
    }
}

pub type Result<T> = std::result::Result<T, Error>;
