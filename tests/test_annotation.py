"""Tests for Strava description annotation."""

import tileharvester.annotate as annotate_mod
from tileharvester.db import get_db


def test_successful_annotation_uses_done_status(isolated_db, monkeypatch) -> None:
    del isolated_db
    with get_db() as conn:
        conn.execute(
            """
            INSERT INTO activities (id, start_utc, start_local, has_gps, status)
            VALUES (1, '2026-01-01T10:00:00Z', '2026-01-01T11:00:00', 1, 'processed')
            """
        )
        conn.commit()
    monkeypatch.setattr(annotate_mod, "get_activity", lambda _activity_id: {"description": "Ride"})
    monkeypatch.setattr(
        annotate_mod, "update_activity_description", lambda _activity_id, _description: {}
    )

    result = annotate_mod.annotate_activity(1)

    assert result["status"] == "annotated"
    with get_db() as conn:
        status = conn.execute("SELECT annotation_status FROM activities WHERE id = 1").fetchone()[0]
    assert status == "done"
