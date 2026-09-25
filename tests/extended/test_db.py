"""Tests for db.py: migrations, settings, reset."""

from tileharvester.db import get_db, get_setting, migrate, reset, set_setting


class TestMigrations:
    def test_all_migrations_run_on_fresh_db(self, temp_db):
        """All tables and indexes should exist after migrate()."""
        result = temp_db.execute(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
        ).fetchall()
        tables = {r["name"] for r in result}
        assert "settings" in tables
        assert "activities" in tables
        assert "activity_tiles" in tables
        assert "global_tiles" in tables
        assert "schema_version" in tables

    def test_schema_version_tracks_all_migrations(self, temp_db):
        """schema_version should equal the number of migrations."""
        version = temp_db.execute("SELECT MAX(version) FROM schema_version").fetchone()[0]
        from tileharvester.db import MIGRATIONS

        assert version == len(MIGRATIONS)

    def test_migrate_is_idempotent(self, temp_db, temp_settings):
        """Running migrate() again should not raise errors."""
        migrate()
        migrate()
        version = temp_db.execute("SELECT MAX(version) FROM schema_version").fetchone()[0]
        from tileharvester.db import MIGRATIONS

        assert version == len(MIGRATIONS)

    def test_activities_columns_exist(self, temp_db):
        """All expected columns should exist on activities table."""
        result = temp_db.execute("PRAGMA table_info(activities)").fetchall()
        columns = {r["name"] for r in result}
        expected = {
            "id",
            "start_utc",
            "start_local",
            "timezone",
            "sport_type",
            "has_gps",
            "status",
            "annotation_status",
            "tile_engine",
            "squadrat_count",
            "squadratinho_count",
            "new_squadrat_count",
            "new_squadratinho_count",
            "description_line",
            "processed_at",
            "annotated_at",
            "last_error",
            "summary_polyline",
            "tile_source",
        }
        assert expected <= columns


class TestSettings:
    def test_get_setting_returns_default_for_missing_key(self, temp_db, temp_settings):
        assert get_setting("nonexistent") is None
        assert get_setting("nonexistent", 42) == 42

    def test_set_and_get_roundtrip_string(self, temp_db, temp_settings):
        set_setting("test_key", "hello")
        assert get_setting("test_key") == "hello"

    def test_set_and_get_roundtrip_number(self, temp_db, temp_settings):
        set_setting("count", 5)
        assert get_setting("count") == 5

    def test_set_and_get_roundtrip_list(self, temp_db, temp_settings):
        data = [1, 2, 3]
        set_setting("my_list", data)
        assert get_setting("my_list") == data

    def test_set_and_get_roundtrip_dict(self, temp_db, temp_settings):
        data = {"a": 1, "b": "two"}
        set_setting("my_dict", data)
        assert get_setting("my_dict") == data

    def test_set_overwrites_existing(self, temp_db, temp_settings):
        set_setting("key", "first")
        set_setting("key", "second")
        assert get_setting("key") == "second"

    def test_get_setting_handles_invalid_json(self, temp_db, temp_settings):
        conn = temp_db
        conn.execute("INSERT INTO settings(key, value) VALUES (?, ?)", ("bad_json", "{invalid"))
        conn.commit()
        result = get_setting("bad_json")
        assert result == "{invalid"


class TestReset:
    def test_reset_clears_all_tables(self, temp_db, temp_settings):
        temp_db.execute("INSERT INTO settings(key, value) VALUES (?, ?)", ("k", "v"))
        temp_db.execute(
            "INSERT INTO activities(id, start_utc, start_local, timezone, sport_type, has_gps, status) VALUES (?, ?, ?, ?, ?, ?, ?)",
            (1, "utc", "local", "tz", "Run", 0, "pending"),
        )
        temp_db.commit()

        # Verify data exists before reset
        assert temp_db.execute("SELECT COUNT(*) FROM activities").fetchone()[0] == 1
        assert temp_db.execute("SELECT COUNT(*) FROM settings").fetchone()[0] == 1

        reset()

        # After reset + migrate, tables exist but are empty
        # Use a fresh connection: reset() drops/recreates schema using its own
        # connection, and asserting through an already-open SQLite connection is
        # unnecessarily sensitive to locking/cache behavior.
        with get_db() as conn:
            assert conn.execute("SELECT COUNT(*) FROM activities").fetchone()[0] == 0
            assert conn.execute("SELECT COUNT(*) FROM settings").fetchone()[0] == 0
            assert conn.execute("SELECT COUNT(*) FROM activity_tiles").fetchone()[0] == 0
            assert conn.execute("SELECT COUNT(*) FROM global_tiles").fetchone()[0] == 0

    def test_reset_followed_by_migrate_works(self, temp_db, temp_settings):
        """After reset, the schema should be re-created via migrate()."""
        set_setting("x", 1)
        reset()  # reset() calls migrate() internally
        # Verify we can still insert and query
        set_setting("y", 2)
        assert get_setting("y") == 2
