//! Unified error types and Strava error classification.

use thiserror::Error;

/// Convenience result alias for TileHarvester operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Classified user-friendly Strava API errors.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StravaError {
    /// Authentication or authorization error (401, 403, missing tokens).
    #[error("{0}")]
    Auth(String),

    /// Rate limit exceeded (429 or close to limit).
    #[error("{0}")]
    RateLimit(String),

    /// Network-level error (connection, timeout).
    #[error("{0}")]
    Network(String),

    /// Data/client error (4xx other than auth or rate limit).
    #[error("{0}")]
    Data(String),

    /// Strava server error (5xx).
    #[error("{0}")]
    Server(String),

    /// Any other Strava or unexpected error.
    #[error("{0}")]
    Other(String),
}

/// Top-level error type for TileHarvester.
#[derive(Debug, Error)]
pub enum Error {
    #[error("Not authenticated. Run 'tileharvester setup' (or 'tileharvester auth') first.")]
    NotAuthenticated,

    #[error(transparent)]
    Strava(#[from] StravaError),

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Config(String),

    #[error("{0}")]
    Validation(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Classify any error into a user-friendly `StravaError`, matching Python's `classify_strava_error`.
    pub fn classify_strava(&self) -> StravaError {
        match self {
            Error::Strava(err) => err.clone(),
            Error::NotAuthenticated => StravaError::Auth(self.to_string()),
            other => StravaError::Other(other.to_string()),
        }
    }
}

/// Classify an HTTP status code and response body into a user-friendly `StravaError`.
pub fn classify_http_status(status: u16, body: &str) -> StravaError {
    let snippet: String = body.chars().take(500).collect();
    match status {
        401 => StravaError::Auth(
            "Authentication failed. Your token may be expired or revoked. \
             Run 'tileharvester auth' to re-authenticate."
                .to_string(),
        ),
        403 => StravaError::Auth(
            "Access denied. Your app may not have the required permissions. \
             Re-authenticate with 'tileharvester auth' and ensure 'activity:read_all' scope."
                .to_string(),
        ),
        429 => StravaError::RateLimit(
            "Strava rate limit exceeded. Wait 15 minutes before retrying.".to_string(),
        ),
        400..=499 => StravaError::Data(format!("Strava API error ({status}): {snippet}")),
        500..=599 => StravaError::Server(format!(
            "Strava server error ({status}). The service may be temporarily unavailable. \
             Retry later or check status.strava.com."
        )),
        _ => StravaError::Other(format!("HTTP {status}: {snippet}")),
    }
}

/// Classify a `reqwest::Error` into a user-friendly `StravaError`.
pub fn classify_reqwest_error(err: &reqwest::Error) -> StravaError {
    if let Some(status) = err.status() {
        return classify_http_status(status.as_u16(), &err.to_string());
    }
    if err.is_connect() {
        return StravaError::Network(
            "Cannot connect to Strava API. Check your internet connection.".to_string(),
        );
    }
    if err.is_timeout() {
        return StravaError::Network(
            "Strava API request timed out. The server may be slow. Try again.".to_string(),
        );
    }
    StravaError::Network(format!(
        "Network error communicating with Strava API: {err}"
    ))
}
