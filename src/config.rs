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
    pub auto_refine: bool,
    pub auto_refine_per_cycle: usize,
    pub auto_refine_rate_limit_reserve_pct: u8,
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
            auto_refine: true,
            auto_refine_per_cycle: 5,
            auto_refine_rate_limit_reserve_pct: 30,
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
    /// Load settings from `<data_dir>/.env`, `./.env`, and `TH_*` environment variables.
    /// Precedence (highest to lowest): process environment > `./.env` > `<data_dir>/.env`.
    pub fn load() -> Result<Self> {
        let env_vars: HashMap<String, String> = env::vars().collect();
        Self::load_with_sources(Some(Path::new(".env")), &env_vars)
    }

    /// Load settings with explicit `.env` path and environment map (useful for isolated tests).
    pub fn load_with_sources(
        cwd_env: Option<&Path>,
        env_vars: &HashMap<String, String>,
    ) -> Result<Self> {
        let mut cwd_map: HashMap<String, String> = HashMap::new();
        if let Some(path) = cwd_env {
            load_env_file_into(path, &mut cwd_map);
        }

        let resolved_data_dir = get_ci(env_vars, "TH_DATA_DIR")
            .or_else(|| get_ci(&cwd_map, "TH_DATA_DIR"))
            .map(expand_tilde)
            .unwrap_or_else(default_data_dir);

        let mut merged: HashMap<String, String> = HashMap::new();
        let data_env = resolved_data_dir.join(".env");
        let same_as_cwd = cwd_env.is_some_and(|p| p == data_env);
        if !same_as_cwd {
            load_env_file_into(&data_env, &mut merged);
        }
        merged.extend(cwd_map);
        for (k, v) in env_vars {
            merged.insert(k.to_ascii_uppercase(), v.clone());
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
        if let Some(v) = get_ci(vars, "TH_AUTO_REFINE") {
            s.auto_refine = parse_bool("TH_AUTO_REFINE", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_AUTO_REFINE_PER_CYCLE") {
            s.auto_refine_per_cycle = parse_field("TH_AUTO_REFINE_PER_CYCLE", v)?;
        }
        if let Some(v) = get_ci(vars, "TH_AUTO_REFINE_RESERVE_PCT") {
            let pct: u8 = parse_field("TH_AUTO_REFINE_RESERVE_PCT", v)?;
            if pct > 95 {
                return Err(Error::Config(format!(
                    "Invalid value for TH_AUTO_REFINE_RESERVE_PCT: {v:?} (must be 0..=95)"
                )));
            }
            s.auto_refine_rate_limit_reserve_pct = pct;
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

    /// Return a deterministic, sorted comma-separated list of ignored sport types.
    pub fn canonical_ignored_sports(&self) -> String {
        let mut sports: Vec<String> = self.ignored_sports().into_iter().collect();
        sports.sort_unstable();
        sports.join(",")
    }

    /// Path to the persisted `.env` file inside the data directory.
    pub fn env_path(&self) -> PathBuf {
        self.data_dir.join(".env")
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

fn load_env_file_into(path: &Path, target: &mut HashMap<String, String>) {
    if let Ok(iter) = dotenvy::from_path_iter(path) {
        for item in iter.flatten() {
            target.insert(item.0.to_ascii_uppercase(), item.1);
        }
    }
}

/// Atomically create or update key-value pairs in a `.env` file while preserving
/// existing comments and unrelated settings. Files are written with `0600` permissions on Unix.
pub fn save_env_values(path: &Path, updates: &[(&str, &str)]) -> Result<()> {
    use std::io::Write;

    for &(key, value) in updates {
        if key.is_empty()
            || !key
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return Err(Error::Validation(format!("Invalid .env key: {key:?}")));
        }
        if value.contains(['\n', '\r', '\0']) {
            return Err(Error::Validation(format!(
                "Value for {key} cannot contain newlines or NUL bytes"
            )));
        }
    }

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let existing = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err.into()),
    };

    let mut applied: HashSet<&str> = HashSet::new();
    let mut lines: Vec<String> = Vec::new();
    for line in existing.lines() {
        let trimmed = line.trim_start();
        let mut replaced = false;
        if !trimmed.starts_with('#') {
            let candidate = trimmed
                .strip_prefix("export ")
                .unwrap_or(trimmed)
                .trim_start();
            if let Some((raw_key, _)) = candidate.split_once('=') {
                let raw_key = raw_key.trim();
                if let Some(&(k, v)) = updates
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(raw_key))
                {
                    if applied.insert(k) {
                        lines.push(format_env_line(k, v));
                    }
                    replaced = true;
                }
            }
        }
        if !replaced {
            lines.push(line.to_owned());
        }
    }
    for &(k, v) in updates {
        if applied.insert(k) {
            lines.push(format_env_line(k, v));
        }
    }
    let mut rendered = lines.join("\n");
    if !rendered.is_empty() {
        rendered.push('\n');
    }

    let mut temporary = tempfile::Builder::new()
        .prefix(".env-")
        .tempfile_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary.as_file_mut().write_all(rendered.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|err| err.error)?;
    Ok(())
}

fn format_env_line(key: &str, value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value.chars().any(|ch| {
            ch.is_whitespace() || matches!(ch, '#' | '"' | '\'' | '\\' | '$' | '`' | '=')
        });
    if needs_quotes {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        format!("{key}=\"{escaped}\"")
    } else {
        format!("{key}={value}")
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
        assert!(s.auto_refine);
        assert_eq!(s.auto_refine_per_cycle, 5);
        assert_eq!(s.auto_refine_rate_limit_reserve_pct, 30);
        assert_eq!(s.rate_limit_buffer, 10);
        assert_eq!(s.stream_max_points, 250_000);
        assert_eq!(s.description_prefix, "TileHarvester");
        assert_eq!(s.description_emoji, "🗺️");
        assert_eq!(s.squadrat_offset, 0);
        assert!(!s.rewrite_existing_annotations);
        assert!(s.ignored_sports().contains("VirtualRide"));
        assert!(s.ignored_sports().contains("VirtualRun"));
        assert_eq!(s.canonical_ignored_sports(), "VirtualRide,VirtualRun");
    }

    #[test]
    fn loads_from_env_map() {
        let mut map = HashMap::new();
        map.insert("TH_DATA_DIR".to_string(), "/tmp/th-test".to_string());
        map.insert("TH_STRAVA_CLIENT_ID".to_string(), "123".to_string());
        map.insert("TH_STRAVA_CLIENT_SECRET".to_string(), "sec".to_string());
        map.insert("TH_SQUADRAT_OFFSET".to_string(), "+5".to_string());
        map.insert("TH_AUTO_REFINE".to_string(), "false".to_string());
        map.insert("TH_AUTO_REFINE_PER_CYCLE".to_string(), "12".to_string());
        map.insert("TH_AUTO_REFINE_RESERVE_PCT".to_string(), "45".to_string());
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
        assert!(!s.auto_refine);
        assert_eq!(s.auto_refine_per_cycle, 12);
        assert_eq!(s.auto_refine_rate_limit_reserve_pct, 45);
        assert!(s.rewrite_existing_annotations);
        assert_eq!(
            s.ignored_sports(),
            HashSet::from(["Swim".to_string(), "VirtualRide".to_string()])
        );
        assert_eq!(s.canonical_ignored_sports(), "Swim,VirtualRide");
        s.validate_strava_credentials().unwrap();

        map.insert("TH_AUTO_REFINE_RESERVE_PCT".to_string(), "96".to_string());
        assert!(Settings::from_map(&map).is_err());
    }

    #[test]
    fn validate_credentials_errors_when_blank() {
        let s = Settings::default();
        let err = s.validate_strava_credentials().unwrap_err();
        assert!(err.to_string().contains("TH_STRAVA_CLIENT_ID"));
    }

    #[test]
    fn env_persistence_and_load_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let data_env = data_dir.join(".env");
        let cwd_env = dir.path().join("cwd.env");

        save_env_values(
            &data_env,
            &[
                ("TH_STRAVA_CLIENT_ID", "from-data"),
                ("TH_STRAVA_CLIENT_SECRET", "sec#with spaces"),
                ("TH_POLL_INTERVAL_MINUTES", "9"),
            ],
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&data_env).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // Updating preserves existing untouched entries.
        save_env_values(&data_env, &[("TH_STRAVA_CLIENT_ID", "updated-data")]).unwrap();

        fs::write(
            &cwd_env,
            format!(
                "TH_DATA_DIR={}\nTH_POLL_INTERVAL_MINUTES=15\n",
                data_dir.display()
            ),
        )
        .unwrap();

        let mut proc_env = HashMap::new();
        proc_env.insert("TH_SQUADRAT_OFFSET".to_string(), "7".to_string());
        let loaded = Settings::load_with_sources(Some(&cwd_env), &proc_env).unwrap();
        assert_eq!(loaded.data_dir, data_dir);
        assert_eq!(loaded.strava_client_id, "updated-data");
        assert_eq!(loaded.strava_client_secret, "sec#with spaces");
        assert_eq!(loaded.poll_interval_minutes, 15);
        assert_eq!(loaded.squadrat_offset, 7);

        // Process env overrides both .env files.
        proc_env.insert("TH_STRAVA_CLIENT_ID".to_string(), "from-env".to_string());
        let loaded = Settings::load_with_sources(Some(&cwd_env), &proc_env).unwrap();
        assert_eq!(loaded.strava_client_id, "from-env");

        // Newlines are rejected safely.
        assert!(save_env_values(&data_env, &[("TH_STRAVA_CLIENT_ID", "bad\nline")]).is_err());
    }
}
