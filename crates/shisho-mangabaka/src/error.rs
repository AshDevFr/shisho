//! Crate error type. `thiserror` per the workspace convention for library
//! crates; `anyhow` appears only as a boxed source for the offline
//! pipeline, whose failures are operational (I/O, SQLite) rather than
//! typed protocol states.

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum MangabakaError {
    #[error("error calling MangaBaka ({endpoint})")]
    Transport {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("could not decode MangaBaka response from {endpoint}: {message}")]
    Malformed { endpoint: String, message: String },
    #[error("MangaBaka rate-limited the request (retry after {retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },
    #[error("MangaBaka rejected the API key")]
    AuthFailed,
    #[error("MangaBaka returned {status} from {endpoint}")]
    Api {
        status: reqwest::StatusCode,
        endpoint: String,
    },
    /// Offline dump pipeline or read-only store failure (I/O, SQLite).
    /// Display carries the full anyhow chain (`{:#}`): the status page and
    /// the recorded refresh attempt only keep the `to_string()`, and a bare
    /// "offline store error" is undiagnosable.
    #[error("MangaBaka offline store error: {0:#}")]
    Store(#[source] anyhow::Error),
    #[error("MangaBaka is not configured: {message}")]
    NotConfigured { message: String },
}
