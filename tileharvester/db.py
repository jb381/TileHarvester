"""SQLite database with migrations."""

import json
import sqlite3
from typing import Any

from tileharvester.config import settings

MIGRATIONS = [
    """
    CREATE TABLE IF NOT EXISTS settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    """,
    """
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
    """,
    """
    CREATE INDEX IF NOT EXISTS idx_activities_status ON activities(status);
    """,
    """
    CREATE INDEX IF NOT EXISTS idx_activities_start_local ON activities(start_local);
    """,
    """
    CREATE INDEX IF NOT EXISTS idx_activities_annotation ON activities(annotation_status);
    """,
    """
    CREATE TABLE IF NOT EXISTS activity_tiles (
        activity_id INTEGER NOT NULL,
        tile_kind TEXT NOT NULL,
        tile_id TEXT NOT NULL,
        is_new INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (activity_id, tile_kind, tile_id)
    );
    """,
    """
    CREATE INDEX IF NOT EXISTS idx_activity_tiles_kind ON activity_tiles(tile_kind, tile_id);
    """,
    """
    CREATE TABLE IF NOT EXISTS global_tiles (
        tile_kind TEXT NOT NULL,
        tile_id TEXT NOT NULL,
        first_activity_id INTEGER NOT NULL,
        first_seen_local TEXT NOT NULL,
        PRIMARY KEY (tile_kind, tile_id)
    );
    """,
    """
    CREATE INDEX IF NOT EXISTS idx_global_tiles_kind ON global_tiles(tile_kind);
    """,
    """
    CREATE TABLE IF NOT EXISTS schema_version (
        version INTEGER PRIMARY KEY
    );
    """,
    """
    ALTER TABLE activities ADD COLUMN summary_polyline TEXT;
    """,
    """
    ALTER TABLE activities ADD COLUMN tile_source TEXT;
    """,
]


def get_db() -> sqlite3.Connection:
    settings.ensure_dirs()
    conn = sqlite3.connect(str(settings.db_path), timeout=5.0)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA busy_timeout=5000")
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA foreign_keys = ON")
    return conn


def get_setting(key: str, default: Any = None) -> Any:
    with get_db() as conn:
        row = conn.execute("SELECT value FROM settings WHERE key = ?", (key,)).fetchone()
        if row is None:
            return default
        try:
            return json.loads(row["value"])
        except json.JSONDecodeError:
            return row["value"]


def set_setting(key: str, value: Any) -> None:
    with get_db() as conn:
        conn.execute(
            "INSERT INTO settings(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            (key, json.dumps(value)),
        )
        conn.commit()


def migrate() -> None:
    settings.ensure_dirs()
    with get_db() as conn:
        conn.execute("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY)")
        row = conn.execute("SELECT MAX(version) as v FROM schema_version").fetchone()
        current = row["v"] or 0

        for i, migration in enumerate(MIGRATIONS, start=1):
            if i > current:
                try:
                    conn.executescript(migration)
                except (sqlite3.OperationalError, Exception) as e:
                    if "duplicate column name" not in str(e):
                        raise
                conn.execute("INSERT INTO schema_version(version) VALUES (?)", (i,))
                conn.commit()
                current = i


def reset() -> None:
    with get_db() as conn:
        for table in ["activity_tiles", "global_tiles", "activities", "settings", "schema_version"]:
            conn.execute(f"DROP TABLE IF EXISTS {table}")
        conn.commit()
    migrate()
