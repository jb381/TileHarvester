"""Recompute and novelty rebuild logic."""

from typing import Any

import polyline
from rich.progress import track

from tileharvester.config import settings
from tileharvester.db import get_db
from tileharvester.kml_baseline import activity_uses_baseline
from tileharvester.sync import (
    _prior_activity_tiles,
    _store_activity_tiles,
)


def recompute_novelty_from_stored_tiles() -> dict[str, Any]:
    """Rebuild global tiles and per-activity novelty from existing activity_tiles."""
    with get_db() as conn:
        conn.execute("DELETE FROM global_tiles")
        conn.execute("UPDATE activity_tiles SET is_new = 0")
        conn.execute(
            """
            UPDATE activities
            SET new_squadrat_count = 0,
                new_squadratinho_count = 0
            WHERE status = 'processed'
            """
        )
        rows = conn.execute(
            """
            SELECT id, start_utc, start_local, baseline_covered
            FROM activities
            WHERE status = 'processed'
            ORDER BY start_local
            """
        ).fetchall()

        rebuilt = 0
        for row in rows:
            aid = row["id"]
            start_local = row["start_local"]
            start_utc = row["start_utc"]
            use_baseline = activity_uses_baseline(conn, row)
            squadrats = {
                r["tile_id"]
                for r in conn.execute(
                    "SELECT tile_id FROM activity_tiles WHERE activity_id = ? AND tile_kind = 'squadrat'",
                    (aid,),
                ).fetchall()
            }
            squadratinhos = {
                r["tile_id"]
                for r in conn.execute(
                    "SELECT tile_id FROM activity_tiles WHERE activity_id = ? AND tile_kind = 'squadratinho'",
                    (aid,),
                ).fetchall()
            }

            new_squadrats = squadrats - _prior_activity_tiles(
                conn,
                "squadrat",
                squadrats,
                start_local,
                aid,
                start_utc=start_utc,
                use_baseline=use_baseline,
            )
            new_squadratinhos = squadratinhos - _prior_activity_tiles(
                conn,
                "squadratinho",
                squadratinhos,
                start_local,
                aid,
                start_utc=start_utc,
                use_baseline=use_baseline,
            )

            conn.executemany(
                "UPDATE activity_tiles SET is_new = 1 WHERE activity_id = ? AND tile_kind = 'squadrat' AND tile_id = ?",
                [(aid, t) for t in new_squadrats],
            )
            conn.executemany(
                "UPDATE activity_tiles SET is_new = 1 WHERE activity_id = ? AND tile_kind = 'squadratinho' AND tile_id = ?",
                [(aid, t) for t in new_squadratinhos],
            )
            conn.executemany(
                """
                INSERT INTO global_tiles (tile_kind, tile_id, first_activity_id, first_seen_local)
                VALUES (?, ?, ?, ?)
                ON CONFLICT(tile_kind, tile_id) DO UPDATE SET
                    first_activity_id = excluded.first_activity_id,
                    first_seen_local = excluded.first_seen_local
                WHERE excluded.first_seen_local < global_tiles.first_seen_local
                """,
                [("squadrat", t, aid, start_local) for t in new_squadrats],
            )
            conn.executemany(
                """
                INSERT INTO global_tiles (tile_kind, tile_id, first_activity_id, first_seen_local)
                VALUES (?, ?, ?, ?)
                ON CONFLICT(tile_kind, tile_id) DO UPDATE SET
                    first_activity_id = excluded.first_activity_id,
                    first_seen_local = excluded.first_seen_local
                WHERE excluded.first_seen_local < global_tiles.first_seen_local
                """,
                [("squadratinho", t, aid, start_local) for t in new_squadratinhos],
            )
            conn.execute(
                "UPDATE activities SET new_squadrat_count = ?, new_squadratinho_count = ? WHERE id = ?",
                (len(new_squadrats), len(new_squadratinhos), aid),
            )
            rebuilt += 1

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
                SET status = 'processed'
                WHERE status = 'skipped_ignored_sport'
                  AND has_gps = 1
                  AND sport_type NOT IN ({placeholders})
                """,
                tuple(ignored_sports),
            )
        else:
            conn.execute(
                """
                UPDATE activities
                SET status = 'processed'
                WHERE status = 'skipped_ignored_sport'
                  AND has_gps = 1
                """
            )

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
