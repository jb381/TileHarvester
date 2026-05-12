"""Strava description annotation with Squadrats stats."""

from datetime import datetime
from typing import Any

from tileharvester.config import settings
from tileharvester.db import get_db
from tileharvester.descriptions import update_description_line
from tileharvester.strava_client import (
    classify_strava_error,
    get_activity,
    update_activity_description,
)
from tileharvester.sync import (
    compute_period_totals,
    compute_total_unique_squadrats_through,
)


def annotate_activity(activity_id: int) -> dict[str, Any]:
    """Update Strava description with TileHarvester line."""
    with get_db() as conn:
        row = conn.execute("SELECT * FROM activities WHERE id = ?", (activity_id,)).fetchone()
        if row is None:
            raise ValueError(f"Activity {activity_id} not found")

        if row["status"] != "processed":
            return {"status": "not_processed", "activity_id": activity_id}

        new_count = row["new_squadrat_count"]
        start_local = row["start_local"]

    month_total, week_total = compute_period_totals(start_local)
    total_unique = (
        compute_total_unique_squadrats_through(activity_id, start_local) + settings.squadrat_offset
    )
    emoji = settings.description_emoji
    prefix = settings.description_prefix
    line = (
        f"{emoji} {prefix}: {total_unique:,} Squadrats · "
        f"+{new_count} new · +{month_total}/mo · +{week_total}/wk"
    )

    try:
        current = get_activity(activity_id)
        current_desc = current.get("description") or ""
        new_desc = update_description_line(current_desc, line)

        if new_desc != current_desc:
            update_activity_description(activity_id, new_desc)
            annotation_status = "updated"
        else:
            annotation_status = "skipped"

        with get_db() as conn:
            conn.execute(
                """
                UPDATE activities
                SET annotation_status = ?, description_line = ?, annotated_at = ?
                WHERE id = ?
                """,
                (annotation_status, line, datetime.utcnow().isoformat(), activity_id),  # noqa: UP017
            )
            conn.commit()
        return {
            "status": "annotated",
            "activity_id": activity_id,
            "line": line,
        }
    except Exception as e:
        error_msg = str(classify_strava_error(e))
        with get_db() as conn:
            conn.execute(
                "UPDATE activities SET annotation_status = ?, last_error = ? WHERE id = ?",
                ("failed", error_msg, activity_id),
            )
            conn.commit()
        return {"status": "annotation_failed", "activity_id": activity_id, "error": error_msg}
