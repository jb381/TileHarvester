"""One-time backfill: fetch and process historical activities."""

from typing import Any

from rich.console import Console
from rich.progress import track

from tileharvester.db import get_db
from tileharvester.sync import (
    compute_activity_tiles_from_summary,
    fetch_and_store_summaries,
)

console = Console()


def backfill(limit: int | None = None) -> dict[str, Any]:
    """Fetch and process historical activities. Does NOT annotate old descriptions."""
    print("Fetching activity summaries...")
    total_fetched = 0
    total_stored = 0
    total_updated = 0
    total_skipped = 0
    total_ignored = 0
    page = 1
    per_page = 200
    while True:
        remaining = None if limit is None else max(limit - total_fetched, 0)
        if remaining == 0:
            break
        result = fetch_and_store_summaries(page=page, per_page=per_page, max_items=remaining)
        fetched = result["fetched"]
        total_fetched += fetched
        total_stored += result["stored"]
        total_updated += result["updated"]
        total_skipped += result["skipped"]
        total_ignored += result["ignored"]
        if fetched == 0:
            break
        print(
            f"  page {page}: fetched {fetched}, stored {result['stored']}, "
            f"updated {result['updated']}, skipped {result['skipped']}, ignored {result['ignored']} "
            f"({total_fetched} fetched total)..."
        )
        page += 1
        if fetched < per_page or (limit is not None and total_fetched >= limit):
            break

    print(
        f"Stored {total_stored} new activities, updated {total_updated} existing activities, "
        f"and marked {total_ignored} ignored sports."
    )

    with get_db() as conn:
        rows = conn.execute(
            "SELECT id FROM activities WHERE status = 'pending' ORDER BY start_local"
        ).fetchall()

    total = len(rows)
    print(f"Processing {total} activities with GPS...")
    processed = 0
    summary_processed = 0
    stream_fallbacks = 0
    skipped_no_gps = 0
    failed = 0
    for row in track(rows, description="Processing"):
        result = compute_activity_tiles_from_summary(row["id"])
        if result["status"] == "processed":
            processed += 1
            if result.get("source") == "summary_polyline":
                summary_processed += 1
            elif result.get("source") == "streams_clean":
                stream_fallbacks += 1
        elif result["status"] == "skipped_no_gps":
            skipped_no_gps += 1
        else:
            failed += 1
            console.log(f"Activity {row['id']}: {result['status']} - {result.get('error', '')}")

    print(
        f"Backfill complete: {processed}/{total} processed "
        f"({summary_processed} from summary polylines, {stream_fallbacks} stream fallbacks, "
        f"{skipped_no_gps} without GPS, {failed} failed)."
    )
    return {
        "stored": total_stored,
        "processed": processed,
        "summary_processed": summary_processed,
        "stream_fallbacks": stream_fallbacks,
        "failed": failed,
        "skipped": total_skipped + skipped_no_gps,
        "ignored": total_ignored,
    }
