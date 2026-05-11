"""Stream refinement: replace summary-polyline tiles with full GPS stream tiles."""

from typing import Any

from tileharvester.config import settings
from tileharvester.db import get_db
from tileharvester.recompute import recompute_novelty_from_stored_tiles
from tileharvester.sync import (
    _print_progress,
    compute_activity_tiles,
)


def refine_streams(limit: int | None = None, force: bool = False) -> dict[str, Any]:
    """Refine stored activity tiles by fetching full Strava GPS streams."""
    actual_limit = limit if limit is not None else settings.refine_default_limit
    filters = ["status = 'processed'", "has_gps = 1"]
    params: list[object] = []
    ignored_sports = sorted(settings.ignored_sports)
    if ignored_sports:
        filters.append(f"sport_type NOT IN ({','.join('?' for _ in ignored_sports)})")
        params.extend(ignored_sports)
    if not force:
        filters.append("COALESCE(tile_source, '') != ?")
        params.append("streams_clean")

    limit_clause = ""
    if actual_limit and actual_limit > 0:
        limit_clause = "LIMIT ?"
        params.append(actual_limit)

    with get_db() as conn:
        rows = conn.execute(
            f"""
            SELECT * FROM activities
            WHERE {" AND ".join(filters)}
            ORDER BY start_local
            {limit_clause}
            """,
            tuple(params),
        ).fetchall()

    total = len(rows)
    print(f"Refining {total} activities from full GPS streams...")
    refined = 0
    failed = 0
    splits = 0
    if total:
        _print_progress("Refining", 0, total)
    for i, row in enumerate(rows, 1):
        result = compute_activity_tiles(row["id"])
        if result["status"] == "processed":
            refined += 1
            splits += result.get("splits", 0)
        else:
            failed += 1
            print()
            print(f"Activity {row['id']}: {result['status']} - {result.get('error', '')}")
        _print_progress("Refining", i, total)

    if refined:
        print("Rebuilding global unique totals from stored activity tiles...")
        novelty = recompute_novelty_from_stored_tiles()
    else:
        novelty = {"rebuilt": 0}

    return {
        "refined": refined,
        "failed": failed,
        "selected": total,
        "rebuilt": novelty["rebuilt"],
        "splits": splits,
    }
