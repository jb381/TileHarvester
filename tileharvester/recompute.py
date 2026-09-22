"""Recompute and novelty rebuild logic."""

from typing import Any

import polyline
from rich.progress import track

from tileharvester.config import settings
from tileharvester.db import get_db
from tileharvester.history import rebuild_tile_history
from tileharvester.sync import (
    _store_activity_tiles,
)


def recompute_novelty_from_stored_tiles() -> dict[str, Any]:
    """Rebuild global tiles and per-activity novelty from existing activity_tiles."""
    with get_db() as conn:
        rebuilt = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE status = 'processed'"
        ).fetchone()[0]
        rebuild_tile_history(conn)

        if settings.rewrite_existing_annotations:
            conn.execute(
                """
                UPDATE activities
                SET annotation_status = 'none'
                WHERE status = 'processed'
                  AND annotation_status = 'done'
                """
            )

        conn.commit()

    return {"rebuilt": rebuilt}


def recompute_all() -> dict[str, Any]:
    """Recompute activity tiles and novelty from stored summary polylines.

    Preserves stream-refined activities — only recomputes activities that were
    processed from summary polylines.
    """
    with get_db() as conn:
        ignored_sports = settings.ignored_sports
        if ignored_sports:
            placeholders = ",".join("?" for _ in ignored_sports)
            conn.execute(
                f"""
                UPDATE activities
                SET status = 'skipped_ignored_sport'
                WHERE sport_type IN ({placeholders})
                  AND status IN ('pending', 'processed', 'failed')
                """,
                tuple(ignored_sports),
            )
            conn.execute(
                f"""
                UPDATE activities
                SET status = CASE WHEN tile_source IS NOT NULL THEN 'processed' ELSE 'pending' END
                WHERE status = 'skipped_ignored_sport'
                  AND COALESCE(sport_type, '') NOT IN ({placeholders})
                """,
                tuple(ignored_sports),
            )
        else:
            conn.execute(
                """
                UPDATE activities
                SET status = CASE WHEN tile_source IS NOT NULL THEN 'processed' ELSE 'pending' END
                WHERE status = 'skipped_ignored_sport'
                """
            )

        # Eligibility changes and their counts must become visible together,
        # even if later route recomputation fails or the process is interrupted.
        rebuild_tile_history(conn)
        conn.commit()

        rows = conn.execute(
            """
            SELECT * FROM activities
            WHERE status = 'processed'
              AND has_gps = 1
            ORDER BY start_local
            """
        ).fetchall()

    total = len(rows)
    refined = sum(1 for row in rows if row["tile_source"] == "streams_clean")
    rows_to_recompute = [row for row in rows if row["tile_source"] != "streams_clean"]
    to_recompute = total - refined
    print(f"Recomputing {to_recompute} activities ({refined} stream-refined preserved)...")
    rebuilt = 0
    skipped = 0
    for row in track(rows_to_recompute, description="Recomputing", disable=not rows_to_recompute):
        summary = row["summary_polyline"]
        if not summary:
            skipped += 1
            continue

        try:
            points = polyline.decode(summary)
        except Exception:
            points = []
        if not points:
            skipped += 1
            continue

        _store_activity_tiles(row, points, "summary_polyline")
        rebuilt += 1

    print("Rebuilding global totals from all stored tiles...")
    recompute_novelty_from_stored_tiles()

    print(f"Recompute complete: {rebuilt} rebuilt, {refined} preserved, {skipped} skipped.")
    return {"rebuilt": rebuilt, "preserved": refined, "skipped": skipped}
