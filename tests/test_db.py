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
        conn.execute("DELETE FROM schema_version WHERE version = ?", (len(MIGRATIONS),))
        conn.commit()

    migrate()

    with get_db() as conn:
        row = conn.execute(
            "SELECT status, annotation_status FROM activities WHERE id = 1"
        ).fetchone()
    assert row["status"] == "pending"
    assert row["annotation_status"] == "done"
