//! SQLite connection and schema migrations.

use std::{fs, io, path::Path, time::Duration};

use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use serde_json::Value;

use crate::models::{ActivityStatus, AnnotationStatus, TileSource};
use crate::{
    config::Settings,
    error::{Error, Result},
};

pub const MIGRATIONS: &[&str] = &[
    r#"
    CREATE TABLE IF NOT EXISTS settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS activities (
        id INTEGER PRIMARY KEY,
        start_utc TEXT NOT NULL,
        start_local TEXT NOT NULL,
        timezone TEXT,
        sport_type TEXT,
        has_gps INTEGER NOT NULL DEFAULT 0,
        status TEXT NOT NULL DEFAULT 'pending',
        annotation_status TEXT NOT NULL DEFAULT 'none',
        tile_engine TEXT NOT NULL DEFAULT 'squadrats',
        squadrat_count INTEGER NOT NULL DEFAULT 0,
        squadratinho_count INTEGER NOT NULL DEFAULT 0,
        new_squadrat_count INTEGER NOT NULL DEFAULT 0,
        new_squadratinho_count INTEGER NOT NULL DEFAULT 0,
        description_line TEXT,
        processed_at TEXT,
        annotated_at TEXT,
        last_error TEXT
    );
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_activities_status ON activities(status);
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_activities_start_local ON activities(start_local);
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_activities_annotation ON activities(annotation_status);
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS activity_tiles (
        activity_id INTEGER NOT NULL,
        tile_kind TEXT NOT NULL,
        tile_id TEXT NOT NULL,
        is_new INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (activity_id, tile_kind, tile_id)
    );
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_activity_tiles_kind ON activity_tiles(tile_kind, tile_id);
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS global_tiles (
        tile_kind TEXT NOT NULL,
        tile_id TEXT NOT NULL,
        first_activity_id INTEGER NOT NULL,
        first_seen_local TEXT NOT NULL,
        PRIMARY KEY (tile_kind, tile_id)
    );
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_global_tiles_kind ON global_tiles(tile_kind);
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS schema_version (
        version INTEGER PRIMARY KEY
    );
    "#,
    r#"
    ALTER TABLE activities ADD COLUMN summary_polyline TEXT;
    "#,
    r#"
    ALTER TABLE activities ADD COLUMN tile_source TEXT;
    "#,
    r#"
    UPDATE activities
    SET status = 'pending'
    WHERE status = 'skipped_no_gps'
      AND COALESCE(summary_polyline, '') = '';

    UPDATE activities
    SET annotation_status = 'done'
    WHERE annotation_status = 'updated';
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS baseline_imports (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        source_name TEXT NOT NULL,
        sha256 TEXT NOT NULL UNIQUE,
        imported_at TEXT NOT NULL,
        as_of_utc TEXT NOT NULL,
        squadrat_count INTEGER NOT NULL,
        squadratinho_count INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS baseline_tiles (
        import_id INTEGER NOT NULL DEFAULT 1,
        tile_kind TEXT NOT NULL CHECK (tile_kind IN ('squadrat', 'squadratinho')),
        tile_id TEXT NOT NULL,
        PRIMARY KEY (import_id, tile_kind, tile_id),
        FOREIGN KEY (import_id) REFERENCES baseline_imports(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_baseline_tiles_kind
        ON baseline_tiles(tile_kind, tile_id);
    "#,
    r#"
    ALTER TABLE activities
    ADD COLUMN baseline_covered INTEGER NOT NULL DEFAULT 0;
    "#,
    r#"
    ALTER TABLE activities
    ADD COLUMN stream_refresh_pending INTEGER NOT NULL DEFAULT 0;
    "#,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    pub id: i64,
    pub squadrat_count: i64,
    pub squadratinho_count: i64,
    pub new_squadrat_count: i64,
    pub new_squadratinho_count: i64,
    pub start_utc: String,
    pub start_local: String,
    pub status: ActivityStatus,
    pub annotation_status: AnnotationStatus,
    pub tile_engine: String,
    pub has_gps: bool,
    pub baseline_covered: bool,
    pub stream_refresh_pending: bool,
    pub timezone: Option<String>,
    pub sport_type: Option<String>,
    pub description_line: Option<String>,
    pub processed_at: Option<String>,
    pub annotated_at: Option<String>,
    pub last_error: Option<String>,
    pub summary_polyline: Option<String>,
    pub tile_source: Option<TileSource>,
}

impl ActivityRow {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            squadrat_count: row.get("squadrat_count")?,
            squadratinho_count: row.get("squadratinho_count")?,
            new_squadrat_count: row.get("new_squadrat_count")?,
            new_squadratinho_count: row.get("new_squadratinho_count")?,
            start_utc: row.get("start_utc")?,
            start_local: row.get("start_local")?,
            status: row.get("status")?,
            annotation_status: row
                .get::<_, Option<_>>("annotation_status")?
                .unwrap_or(AnnotationStatus::None),
            tile_engine: row.get("tile_engine")?,
            has_gps: row.get("has_gps")?,
            baseline_covered: row.get("baseline_covered")?,
            stream_refresh_pending: row.get("stream_refresh_pending")?,
            timezone: row.get("timezone")?,
            sport_type: row.get("sport_type")?,
            description_line: row.get("description_line")?,
            processed_at: row.get("processed_at")?,
            annotated_at: row.get("annotated_at")?,
            last_error: row.get("last_error")?,
            summary_polyline: row.get("summary_polyline")?,
            tile_source: row.get("tile_source")?,
        })
    }
}

pub fn get_activity_row(conn: &Connection, activity_id: i64) -> Result<Option<ActivityRow>> {
    Ok(conn
        .query_row(
            "SELECT * FROM activities WHERE id = ?",
            [activity_id],
            ActivityRow::from_row,
        )
        .optional()?)
}

pub fn get_db(settings: &Settings) -> Result<Connection> {
    settings.ensure_dirs()?;
    let conn = Connection::open(settings.db_path())?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(conn)
}

/// Write a consistent snapshot of the live database.
pub fn backup(settings: &Settings, dest: &Path, force: bool) -> Result<()> {
    backup_with_conn(settings, &get_db(settings)?, dest, force)
}

pub fn backup_with_conn(
    settings: &Settings,
    conn: &Connection,
    dest: &Path,
    force: bool,
) -> Result<()> {
    let filename = dest
        .file_name()
        .filter(|_| !dest.is_dir())
        .ok_or_else(|| Error::Validation("Backup destination must be a file path.".into()))?;
    let parent = dest
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let parent = parent.canonicalize()?;
    let dest = parent.join(filename);
    let live = settings.db_path().canonicalize()?;
    let resolved_dest = dest.canonicalize().ok();
    let targets = |path: &Path| dest == path || resolved_dest.as_deref() == Some(path);
    if targets(&live) {
        return Err(Error::Validation(
            "Cannot overwrite the live database.".into(),
        ));
    }

    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = live.as_os_str().to_owned();
        sidecar.push(suffix);
        if targets(Path::new(&sidecar)) {
            return Err(Error::Validation(
                "Cannot overwrite a live database sidecar.".into(),
            ));
        }
    }

    let exists_error = || {
        Error::Validation("Backup destination already exists. Use --force to overwrite it.".into())
    };
    match dest.symlink_metadata() {
        Ok(_) if !force => return Err(exists_error()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    // Reserve a private directory on the same filesystem and leave the SQLite target absent.
    let temporary = tempfile::tempdir_in(parent)?;
    let snapshot = temporary.path().join("snapshot.db");
    let sqlite_path = snapshot.to_str().ok_or_else(|| {
        Error::Validation("Backup destination must be representable as UTF-8.".into())
    })?;
    conn.execute("VACUUM INTO ?1", [sqlite_path])?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o600))?;
    }
    if force {
        fs::rename(&snapshot, &dest)?;
    } else if let Err(error) = fs::hard_link(&snapshot, &dest) {
        // Publish atomically without replacing a file created during VACUUM.
        return Err(if error.kind() == io::ErrorKind::AlreadyExists {
            exists_error()
        } else {
            error.into()
        });
    }
    Ok(())
}

pub fn get_setting(settings: &Settings, key: &str) -> Result<Option<Value>> {
    get_setting_with_conn(&get_db(settings)?, key)
}

pub(crate) fn get_setting_with_conn(conn: &Connection, key: &str) -> Result<Option<Value>> {
    let raw: Option<String> = conn
        .query_row("SELECT value FROM settings WHERE key = ?", [key], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(raw.map(|raw| serde_json::from_str(&raw).unwrap_or(Value::String(raw))))
}

pub fn set_setting(settings: &Settings, key: &str, value: &Value) -> Result<()> {
    set_setting_with_conn(&get_db(settings)?, key, value)
}

pub(crate) fn set_setting_with_conn(conn: &Connection, key: &str, value: &Value) -> Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, serde_json::to_string(value)?],
    )?;
    Ok(())
}

pub fn migrate(settings: &Settings) -> Result<()> {
    migrate_with_conn(&mut get_db(settings)?)
}

pub(crate) fn migrate_with_conn(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY)")?;
    let current: i64 = tx.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?;
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if version <= current {
            continue;
        }
        let added_column = match version {
            11 => Some("summary_polyline"),
            12 => Some("tile_source"),
            15 => Some("baseline_covered"),
            16 => Some("stream_refresh_pending"),
            _ => None,
        };
        if let Some(column) = added_column {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('activities') WHERE name = ? COLLATE NOCASE)",
                [column],
                |row| row.get(0),
            )?;
            if !exists {
                tx.execute_batch(sql)?;
            }
        } else {
            tx.execute_batch(sql)?;
        }
        tx.execute("INSERT INTO schema_version(version) VALUES (?)", [version])?;
    }
    tx.commit()?;
    Ok(())
}

pub fn reset(settings: &Settings) -> Result<()> {
    reset_with_conn(&mut get_db(settings)?)
}

pub(crate) fn reset_with_conn(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "DROP TABLE IF EXISTS baseline_tiles;
         DROP TABLE IF EXISTS baseline_imports;
         DROP TABLE IF EXISTS activity_tiles;
         DROP TABLE IF EXISTS global_tiles;
         DROP TABLE IF EXISTS activities;
         DROP TABLE IF EXISTS settings;
         DROP TABLE IF EXISTS schema_version;",
    )?;
    tx.commit()?;
    migrate_with_conn(conn)
}

pub const SQL_CHUNK_SIZE: usize = 500;

pub fn in_placeholders(count: usize) -> String {
    vec!["?"; count].join(",")
}

pub(crate) fn baseline_tiles_in_chunk(
    conn: &Connection,
    kind: &str,
    tiles: &[&str],
) -> Result<std::collections::HashSet<String>> {
    tile_ids_in_chunk(
        conn,
        "SELECT tile_id FROM baseline_tiles WHERE tile_kind = ? AND tile_id",
        &[&kind],
        tiles,
    )
}

pub(crate) fn tile_ids_in_chunk(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
    tiles: &[&str],
) -> Result<std::collections::HashSet<String>> {
    let placeholders = in_placeholders(tiles.len());
    let mut stmt = conn.prepare_cached(&format!("{sql} IN ({placeholders})"))?;
    let args = params
        .iter()
        .copied()
        .chain(tiles.iter().map(|tile| tile as &dyn rusqlite::ToSql));
    let rows = stmt.query_map(rusqlite::params_from_iter(args), |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn backup_snapshots_contents_and_requires_force() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let settings = Settings::with_data_dir(dir.path());
        migrate(&settings)?;
        let dest = dir.path().join("backups").join("snapshot.db");
        let read_snapshot = || -> Result<Option<Value>> {
            get_setting_with_conn(&Connection::open(&dest)?, "example")
        };
        set_setting(&settings, "example", &serde_json::json!(1))?;
        backup(&settings, &dest, false)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(dest.metadata()?.permissions().mode() & 0o777, 0o600);
        }
        set_setting(&settings, "example", &serde_json::json!(2))?;
        assert!(backup(&settings, &dest, false)
            .unwrap_err()
            .to_string()
            .contains("--force"));
        assert_eq!(read_snapshot()?, Some(serde_json::json!(1)));
        backup(&settings, &dest, true)?;
        assert_eq!(read_snapshot()?, Some(serde_json::json!(2)));
        assert!(backup(&settings, Path::new(""), true).is_err());
        assert!(backup(&settings, dir.path(), true).is_err());
        assert!(
            backup(&settings, &dir.path().join("tileharvester.db-wal"), true)
                .unwrap_err()
                .to_string()
                .contains("live database sidecar")
        );
        assert!(
            backup(&settings, &dir.path().join("./tileharvester.db"), true)
                .unwrap_err()
                .to_string()
                .contains("live database")
        );
        #[cfg(unix)]
        {
            let alias = dir.path().join("alias.db");
            std::os::unix::fs::symlink(settings.db_path(), &alias)?;
            assert!(backup(&settings, &alias, true)
                .unwrap_err()
                .to_string()
                .contains("live database"));
        }
        Ok(())
    }

    #[test]
    fn records_migrations_when_columns_already_exist() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let settings = Settings::with_data_dir(dir.path().to_owned());
        let conn = get_db(&settings)?;
        for sql in &MIGRATIONS[..10] {
            conn.execute_batch(sql)?;
        }
        conn.execute("INSERT INTO schema_version VALUES (10)", [])?;
        for index in [10, 11, 14, 15] {
            conn.execute_batch(MIGRATIONS[index])?;
        }
        migrate(&settings)?;
        for version in 11..=16 {
            assert!(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM schema_version WHERE version = ?)",
                [version],
                |row| row.get::<_, bool>(0)
            )?);
        }
        Ok(())
    }

    #[test]
    fn legacy_upgrade_repairs_states_and_accepts_existing_columns() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let settings = Settings::with_data_dir(dir.path().join("data"));
        let conn = get_db(&settings)?;
        conn.execute_batch(MIGRATIONS[9])?;
        for (index, sql) in MIGRATIONS[..12].iter().enumerate() {
            conn.execute_batch(sql)?;
            conn.execute(
                "INSERT INTO schema_version VALUES (?)",
                [(index + 1) as i64],
            )?;
        }
        conn.execute_batch(
            "INSERT INTO activities (id, start_utc, start_local, status, annotation_status)
             VALUES (1, 'utc', 'local', 'skipped_no_gps', 'updated'),
                    (2, 'utc', 'local', 'processed', 'skipped');
             ALTER TABLE activities ADD COLUMN stream_refresh_pending INTEGER NOT NULL DEFAULT 0;",
        )?;
        migrate(&settings)?;
        migrate(&settings)?;
        assert_eq!(
            conn.query_row("SELECT MAX(version) FROM schema_version", [], |r| r
                .get::<_, i64>(0))?,
            16
        );
        let activity = get_activity_row(&conn, 1)?.unwrap();
        assert_eq!(activity.status.as_str(), "pending");
        assert_eq!(activity.annotation_status.as_str(), "done");
        assert!(!activity.baseline_covered);
        assert!(!activity.stream_refresh_pending);
        assert_eq!(
            get_activity_row(&conn, 2)?.unwrap().status.as_str(),
            "processed"
        );
        assert_eq!(
            get_activity_row(&conn, 2)?.unwrap().annotation_status,
            AnnotationStatus::Skipped
        );
        assert!(get_activity_row(&conn, 3)?.is_none());

        assert_eq!(
            conn.pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))?,
            "wal"
        );
        assert_eq!(
            conn.pragma_query_value(None, "busy_timeout", |r| r.get::<_, i64>(0))?,
            5000
        );
        assert!(conn
            .execute(
                "INSERT INTO baseline_tiles VALUES (1, 'squadrat', '14:1:2')",
                []
            )
            .is_err());
        conn.execute_batch(
            "INSERT INTO baseline_imports VALUES (1, 'snapshot', 'hash', 'now', 'now', 1, 0);
             INSERT INTO baseline_tiles VALUES (1, 'squadrat', '14:1:2');",
        )?;
        reset(&settings)?;
        assert!(get_activity_row(&conn, 1)?.is_none());
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM baseline_tiles", [], |r| r
                .get::<_, i64>(0))?,
            0
        );
        Ok(())
    }

    #[test]
    fn settings_upsert_and_read_legacy_text() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let settings = Settings::with_data_dir(dir.path());
        migrate(&settings)?;
        assert_eq!(get_setting(&settings, "key")?, None);
        get_db(&settings)?.execute("INSERT INTO settings VALUES ('key', 'legacy')", [])?;
        assert_eq!(get_setting(&settings, "key")?, Some(json!("legacy")));
        let value = json!({"enabled": true, "list": [1, null]});
        set_setting(&settings, "key", &value)?;
        assert_eq!(get_setting(&settings, "key")?, Some(value));
        set_setting(&settings, "key", &Value::Null)?;
        assert_eq!(get_setting(&settings, "key")?, Some(Value::Null));
        Ok(())
    }
}
