"""Tests for rebuilding stored tile data."""

import polyline
import pytest

import tileharvester.recompute as recompute_mod
from tileharvester.config import settings
from tileharvester.db import get_db


def _insert_processed_activity(
    activity_id: int,
    *,
    source: str,
    summary: str | None = None,
    annotation_status: str = "none",
) -> None:
    with get_db() as conn:
        conn.execute(
            """
            INSERT INTO activities
                (id, start_utc, start_local, has_gps, status, annotation_status,
                 summary_polyline, tile_source)
            VALUES (?, ?, ?, 1, 'processed', ?, ?, ?)
            """,
            (
                activity_id,
                f"2026-01-{activity_id:02d}T10:00:00Z",
                f"2026-01-{activity_id:02d}T11:00:00",
                annotation_status,
                summary,
                source,
            ),
        )
        conn.commit()


def test_recompute_preserves_stream_tiles_and_rebuilds_summaries(isolated_db) -> None:
    del isolated_db
    _insert_processed_activity(1, source="streams_clean")
    _insert_processed_activity(
        2,
        source="summary_polyline",
        summary=polyline.encode([(52.0, 5.0), (52.0001, 5.0001)]),
    )
    with get_db() as conn:
        conn.execute("INSERT INTO activity_tiles VALUES (1, 'squadrat', '14:1:1', 1)")
        conn.execute("INSERT INTO global_tiles VALUES ('squadrat', '14:1:1', 1, '2026-01-01')")
        conn.commit()

    result = recompute_mod.recompute_all()

    assert result == {"rebuilt": 1, "preserved": 1, "skipped": 0}
    with get_db() as conn:
        stream_tile = conn.execute(
            "SELECT COUNT(*) FROM activity_tiles WHERE activity_id = 1"
        ).fetchone()[0]
        global_tiles = conn.execute("SELECT COUNT(*) FROM global_tiles").fetchone()[0]
    assert stream_tile == 1
    assert global_tiles > 0


def test_novelty_rebuild_rolls_back_on_failure(isolated_db, monkeypatch) -> None:
    del isolated_db
    _insert_processed_activity(1, source="streams_clean")
    with get_db() as conn:
        conn.execute("INSERT INTO activity_tiles VALUES (1, 'squadrat', '14:1:1', 1)")
        conn.execute("INSERT INTO global_tiles VALUES ('squadrat', '14:1:1', 1, '2026-01-01')")
        conn.commit()
    monkeypatch.setattr(
        recompute_mod,
        "_prior_activity_tiles",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(RuntimeError("broken rebuild")),
    )

    with pytest.raises(RuntimeError, match="broken rebuild"):
        recompute_mod.recompute_novelty_from_stored_tiles()

    with get_db() as conn:
        assert conn.execute("SELECT COUNT(*) FROM global_tiles").fetchone()[0] == 1
        assert conn.execute("SELECT is_new FROM activity_tiles").fetchone()[0] == 1


def test_recompute_marks_annotations_for_rewrite(isolated_db, monkeypatch) -> None:
    del isolated_db
    _insert_processed_activity(1, source="streams_clean", annotation_status="done")
    monkeypatch.setattr(settings, "rewrite_existing_annotations", True)

    recompute_mod.recompute_novelty_from_stored_tiles()

    with get_db() as conn:
        status = conn.execute("SELECT annotation_status FROM activities WHERE id = 1").fetchone()[0]
    assert status == "none"
