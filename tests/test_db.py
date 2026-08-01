"""Tests for database migrations."""

from tileharvester.db import MIGRATIONS, get_db, migrate


def test_migration_repairs_legacy_activity_states(isolated_db) -> None:
    del isolated_db
    with get_db() as conn:
        conn.execute(
            """
            INSERT INTO activities
                (id, start_utc, start_local, status, annotation_status)
            VALUES (1, '2026-01-01T10:00:00Z', '2026-01-01T11:00:00',
                    'skipped_no_gps', 'updated')
            """
        )
        repair_version = next(
            index
            for index, migration in enumerate(MIGRATIONS, start=1)
            if "annotation_status IN ('updated', 'skipped')" in migration
        )
        conn.execute("DELETE FROM schema_version WHERE version >= ?", (repair_version,))
        conn.commit()

    migrate()

    with get_db() as conn:
        row = conn.execute(
            "SELECT status, annotation_status FROM activities WHERE id = 1"
        ).fetchone()
    assert row["status"] == "pending"
    assert row["annotation_status"] == "done"


def test_migration_adds_optional_kml_baseline_schema(isolated_db) -> None:
    del isolated_db
    with get_db() as conn:
        tables = {
            row["name"]
            for row in conn.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table'"
            ).fetchall()
        }
        activity_columns = {
            row["name"] for row in conn.execute("PRAGMA table_info(activities)").fetchall()
        }

    assert {"baseline_imports", "baseline_tiles"} <= tables
    assert "baseline_covered" in activity_columns


def test_migration_upgrades_database_created_before_kml_support(isolated_db) -> None:
    del isolated_db
    old_version = len(MIGRATIONS) - 2
    with get_db() as conn:
        conn.execute("DROP TABLE baseline_tiles")
        conn.execute("DROP TABLE baseline_imports")
        conn.execute("ALTER TABLE activities DROP COLUMN baseline_covered")
        conn.execute("DELETE FROM schema_version WHERE version > ?", (old_version,))
        conn.commit()

    migrate()

    with get_db() as conn:
        version = conn.execute("SELECT MAX(version) FROM schema_version").fetchone()[0]
        tables = {
            row["name"]
            for row in conn.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table'"
            ).fetchall()
        }
        columns = {row["name"] for row in conn.execute("PRAGMA table_info(activities)").fetchall()}
    assert version == len(MIGRATIONS)
    assert {"baseline_imports", "baseline_tiles"} <= tables
    assert "baseline_covered" in columns
