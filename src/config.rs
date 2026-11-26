//! Configuration and settings management.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Default data directory: `~/.local/share/tileharvester`.
pub fn default_data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("share")
        .join("tileharvester")
}

/// Runtime settings loaded from environment variables (`TH_*`) and `.env`.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub data_dir: PathBuf,
    pub strava_client_id: String,
    pub strava_client_secret: String,
    pub strava_redirect_uri: String,

    /// Squadrats uses Mapbox/XYZ tile cells: z14 for Squadrats, z17 for Squadratinhos.
    pub squadrat_zoom: u8,
    pub squadratinho_zoom: u8,

    /// Sync behavior
    pub poll_interval_minutes: u64,
    pub sync_lookback_days: i64,
    pub sync_annotation_window_days: i64,
    pub refine_default_limit: usize,
    pub rate_limit_buffer: i64,
    pub ignored_sport_types: String,
    pub stream_max_segment_meters: f64,
    pub stream_max_time_gap_seconds: f64,
    pub stream_max_speed_mps: f64,
    pub stream_gap_min_meters: f64,
    pub stream_max_points: usize,

    /// Description
    pub description_prefix: String,
    pub description_emoji: String,
    pub squadrat_offset: i64,
    pub rewrite_existing_annotations: bool,
    pub include_squadratinhos_in_description: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            strava_client_id: String::new(),
            strava_client_secret: String::new(),
            strava_redirect_uri: "http://localhost:8000/callback".to_string(),
            squadrat_zoom: 14,
            squadratinho_zoom: 17,
            poll_interval_minutes: 5,
            sync_lookback_days: 7,
            sync_annotation_window_days: 1,
            refine_default_limit: 80,
            rate_limit_buffer: 10,
            ignored_sport_types: "VirtualRide,VirtualRun".to_string(),
            stream_max_segment_meters: 300.0,
            stream_max_time_gap_seconds: 60.0,
            stream_max_speed_mps: 35.0,
            stream_gap_min_meters: 50.0,
            stream_max_points: 250_000,
            description_prefix: "TileHarvester".to_string(),
            description_emoji: "🗺️".to_string(),
            squadrat_offset: 0,
            rewrite_existing_annotations: false,
            include_squadratinhos_in_description: false,
        }
    }
}

impl Settings {
    /// Load settings from `.env` (if present in current directory) and `TH_*` environment variables.
    /// Process environment variables take precedence over `.env` values.
    pub fn load() -> Result<Self> {
        let mut merged: HashMap<String, String> = HashMap::new();
        if let Ok(iter) = dotenvy::from_filename_iter(".env") {
            for item in iter.flatten() {
                merged.insert(item.0.to_ascii_uppercase(), item.1);
            }
        }
        for (k, v) in env::vars() {
            merged.insert(k.to_ascii_uppercase(), v);
        }
        Self::from_map(&merged)
    }

    /// Load settings from an explicit key-value map (useful for isolated tests).
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self> {
        let mut s = Self::default();

        if let Some(v) = get_ci(vars, "TH_DATA_DIR") {
            s.data_dir = expand_tilde(v);
        }
        if let Some(v) = get_ci(vars, "TH_STRAVA_CLIENT_ID") {
            s.strava_client_id = v.to_string();
        }
        if let Some(v) = get_ci(vars, "TH_STRAVA_CLIENT_SECRET") {
            s.strava_client_secret = v.to_string();
        }
        if let Some(v) = get_ci(vars, "TH_STRAVA_REDIRECT_URI") {
            s.strava_redirect_uri = v.to_string();
        }
        if let Some(v) = get_ci(vars, "TH_SQUADRAT_ZOOM") {
            s.squadrat_zoom = parse_field("TH_SQUADRAT_ZOOM", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_SQUADRATINHO_ZOOM") {
            s.squadratinho_zoom = parse_field("TH_SQUADRATINHO_ZOOM", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_POLL_INTERVAL_MINUTES") {
            s.poll_interval_minutes = parse_field("TH_POLL_INTERVAL_MINUTES", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_SYNC_LOOKBACK_DAYS") {
            s.sync_lookback_days = parse_field("TH_SYNC_LOOKBACK_DAYS", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_SYNC_ANNOTATION_WINDOW_DAYS") {
            s.sync_annotation_window_days = parse_field("TH_SYNC_ANNOTATION_WINDOW_DAYS", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_REFINE_DEFAULT_LIMIT") {
            s.refine_default_limit = parse_field("TH_REFINE_DEFAULT_LIMIT", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_RATE_LIMIT_BUFFER") {
            s.rate_limit_buffer = parse_field("TH_RATE_LIMIT_BUFFER", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_IGNORED_SPORT_TYPES") {
            s.ignored_sport_types = v.to_string();
        }
        if let Some(v) = get_ci(vars, "TH_STREAM_MAX_SEGMENT_METERS") {
            s.stream_max_segment_meters = parse_field("TH_STREAM_MAX_SEGMENT_METERS", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_STREAM_MAX_TIME_GAP_SECONDS") {
            s.stream_max_time_gap_seconds = parse_field("TH_STREAM_MAX_TIME_GAP_SECONDS", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_STREAM_MAX_SPEED_MPS") {
            s.stream_max_speed_mps = parse_field("TH_STREAM_MAX_SPEED_MPS", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_STREAM_GAP_MIN_METERS") {
            s.stream_gap_min_meters = parse_field("TH_STREAM_GAP_MIN_METERS", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_STREAM_MAX_POINTS") {
            s.stream_max_points = parse_field("TH_STREAM_MAX_POINTS", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_DESCRIPTION_PREFIX") {
            s.description_prefix = v.to_string();
        }
        if let Some(v) = get_ci(vars, "TH_DESCRIPTION_EMOJI") {
            s.description_emoji = v.to_string();
        }
        if let Some(v) = get_ci(vars, "TH_SQUADRAT_OFFSET") {
            s.squadrat_offset = parse_field("TH_SQUADRAT_OFFSET", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_REWRITE_EXISTING_ANNOTATIONS") {
            s.rewrite_existing_annotations = parse_bool("TH_REWRITE_EXISTING_ANNOTATIONS", v)?;
        }

        if let Some(v) = get_ci(vars, "TH_INCLUDE_SQUADRATINHOS") {
            s.include_squadratinhos_in_description = parse_bool("TH_INCLUDE_SQUADRATINHOS", v)?;
        }

        Ok(s)
    }

    /// Create settings rooted at a specific data directory (convenient for tests).
    pub fn with_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            ..Self::default()
        }
    }

    /// Validate that Strava OAuth credentials are configured.
    pub fn validate_strava_credentials(&self) -> Result<()> {
        if self.strava_client_id.trim().is_empty() || self.strava_client_secret.trim().is_empty() {
            return Err(Error::Config(
                "Strava credentials not configured. \
                 Set TH_STRAVA_CLIENT_ID and TH_STRAVA_CLIENT_SECRET \
                 environment variables or add them to your .env file. \
                 Get your API credentials at https://www.strava.com/settings/api"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Return the set of ignored Strava sport types.
    pub fn ignored_sports(&self) -> HashSet<String> {
        self.ignored_sport_types
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Path to the SQLite database file.
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("tileharvester.db")
    }

    /// Path to the Strava OAuth token JSON file.
    pub fn token_path(&self) -> PathBuf {
        self.data_dir.join("strava_tokens.json")
    }

    /// Ensure the configured data directory exists.
    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir)?;
        Ok(())
    }
}

fn get_ci<'a>(vars: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    if let Some(v) = vars.get(key) {
        return Some(v.as_str());
    }
    vars.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

fn parse_field<T: std::str::FromStr>(name: &str, raw: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    raw.trim()
        .parse::<T>()
        .map_err(|e| Error::Config(format!("Invalid value for {name}: {raw:?} ({e})")))
}

fn parse_bool(name: &str, raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "t" | "y" => Ok(true),
        "0" | "false" | "no" | "off" | "f" | "n" => Ok(false),
        _ => Err(Error::Config(format!(
            "Invalid boolean value for {name}: {raw:?}"
        ))),
    }
}

/// Expand leading `~` or `~/` to the user's home directory.
pub fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    Path::new(raw).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let s = Settings::default();
        assert_eq!(s.squadrat_zoom, 14);
        assert_eq!(s.squadratinho_zoom, 17);
        assert_eq!(s.poll_interval_minutes, 5);
        assert_eq!(s.sync_lookback_days, 7);
        assert_eq!(s.sync_annotation_window_days, 1);
        assert_eq!(s.refine_default_limit, 80);
        assert_eq!(s.rate_limit_buffer, 10);
        assert_eq!(s.stream_max_points, 250_000);
        assert_eq!(s.description_prefix, "TileHarvester");
        assert_eq!(s.description_emoji, "🗺️");
        assert_eq!(s.squadrat_offset, 0);
        assert!(!s.rewrite_existing_annotations);
        assert!(s.ignored_sports().contains("VirtualRide"));
        assert!(s.ignored_sports().contains("VirtualRun"));
    }

    #[test]
    fn loads_from_env_map() {
        let mut map = HashMap::new();
        map.insert("TH_DATA_DIR".to_string(), "/tmp/th-test".to_string());
        map.insert("TH_STRAVA_CLIENT_ID".to_string(), "123".to_string());
        map.insert("TH_STRAVA_CLIENT_SECRET".to_string(), "sec".to_string());
        map.insert("TH_SQUADRAT_OFFSET".to_string(), "+5".to_string());
        map.insert(
            "TH_REWRITE_EXISTING_ANNOTATIONS".to_string(),
            "true".to_string(),
        );
        map.insert(
            "TH_IGNORED_SPORT_TYPES".to_string(),
            " Swim , VirtualRide , ".to_string(),
        );
        let s = Settings::from_map(&map).unwrap();
        assert_eq!(s.data_dir, PathBuf::from("/tmp/th-test"));
        assert_eq!(s.squadrat_offset, 5);
        assert!(s.rewrite_existing_annotations);
        assert_eq!(
            s.ignored_sports(),
            HashSet::from(["Swim".to_string(), "VirtualRide".to_string()])
        );
        s.validate_strava_credentials().unwrap();
    }

    #[test]
    fn validate_credentials_errors_when_blank() {
        let s = Settings::default();
        let err = s.validate_strava_credentials().unwrap_err();
        assert!(err.to_string().contains("TH_STRAVA_CLIENT_ID"));
    }
}
