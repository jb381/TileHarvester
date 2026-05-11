"""Recompute and novelty rebuild logic."""

from typing import Any

import polyline

from tileharvester.config import settings
from tileharvester.db import get_db
from tileharvester.sync import (
    _print_progress,
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
        conn.commit()

        rows = conn.execute(
            "SELECT id, start_local FROM activities WHERE status = 'processed' ORDER BY start_local"
        ).fetchall()

    rebuilt = 0
    for row in rows:
        aid = row["id"]
        start_local = row["start_local"]
        with get_db() as conn:
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
                conn, "squadrat", squadrats, start_local, aid
            )
            new_squadratinhos = squadratinhos - _prior_activity_tiles(
                conn, "squadratinho", squadratinhos, start_local, aid
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
            conn.commit()
            rebuilt += 1

    return {"rebuilt": rebuilt}


def recompute_all() -> dict[str, Any]:
    """Recompute activity tiles and novelty from stored summary polylines.

    Preserves stream-refined activities — only recomputes activities that were
    processed from summary polylines.
    """
    with get_db() as conn:
        conn.execute("DELETE FROM global_tiles")
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

        # Delete tiles only for non-refined activities
        conn.execute(
            """
            DELETE FROM activity_tiles
            WHERE activity_id IN (
                SELECT id FROM activities
                WHERE COALESCE(tile_source, '') != 'streams_clean'
            )
            """
        )
        # Reset counts only for non-refined activities
        conn.execute(
            """
            UPDATE activities
            SET squadrat_count = 0,
                squadratinho_count = 0,
                new_squadrat_count = 0,
                new_squadratinho_count = 0
            WHERE COALESCE(tile_source, '') != 'streams_clean'
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
    refined = sum(1 for r in rows if r.get("tile_source") == "streams_clean")
    to_recompute = total - refined
    print(f"Recomputing {to_recompute} activities ({refined} stream-refined preserved)...")
    rebuilt = 0
    skipped = 0
    if to_recompute:
        _print_progress("Recomputing", 0, to_recompute)
    for _i, row in enumerate(rows, 1):
        if row.get("tile_source") == "streams_clean":
            continue  # Don't touch refined data

        summary = row["summary_polyline"]
        if not summary:
            skipped += 1
            _print_progress("Recomputing", rebuilt + skipped, to_recompute)
            continue

        try:
            points = polyline.decode(summary)
        except Exception:
            points = []
        if not points:
            skipped += 1
            _print_progress("Recomputing", rebuilt + skipped, to_recompute)
            continue

        _store_activity_tiles(row, points, "summary_polyline")
        rebuilt += 1
        _print_progress("Recomputing", rebuilt + skipped, to_recompute)

    print("Rebuilding global totals from all stored tiles...")
    recompute_novelty_from_stored_tiles()

    print(f"Recompute complete: {rebuilt} rebuilt, {refined} preserved, {skipped} skipped.")
    return {"rebuilt": rebuilt, "preserved": refined, "skipped": skipped}
