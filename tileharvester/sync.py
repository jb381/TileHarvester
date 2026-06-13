"""Core sync logic and shared utilities for activity processing."""

import math
import sqlite3
from datetime import datetime, timedelta, timezone
from typing import Any

import polyline
from rich.progress import track

from tileharvester.config import settings
from tileharvester.db import get_db
from tileharvester.strava_client import (
    classify_strava_error,
    get_activities,
    get_activity_streams,
)
from tileharvester.tile_engine import make_engine


def _week_start(dt: datetime) -> datetime:
    """ISO week start (Monday)."""
    return dt - timedelta(days=dt.weekday())


def _month_start(dt: datetime) -> datetime:
    return dt.replace(day=1)


def _parse_local(local_str: str) -> datetime:
    """Parse Strava's start_date_local into a naive datetime."""
    if local_str.endswith("Z"):
        local_str = local_str[:-1]
    idx = local_str.rfind("+")
    if idx >= 0:
        local_str = local_str[:idx]
    if ":" in local_str and local_str.count("-") > 2:
        local_str = local_str[: local_str.rfind("-", 0, local_str.rfind(":"))]
    return datetime.fromisoformat(local_str)


def _summary_polyline(activity: dict[str, Any]) -> str | None:
    summary = (activity.get("map") or {}).get("summary_polyline")
    return summary or None


def _is_ignored_sport(sport_type: str | None) -> bool:
    return bool(sport_type and sport_type in settings.ignored_sports)


def _pending_status(has_gps: bool, sport_type: str | None) -> str:
    if _is_ignored_sport(sport_type):
        return "skipped_ignored_sport"
    return "pending" if has_gps else "skipped_no_gps"


def _distance_meters(a: tuple[float, float], b: tuple[float, float]) -> float:
    lat1, lon1 = a
    lat2, lon2 = b
    radius = 6371000.0
    dlat = math.radians(lat2 - lat1)
    dlon = math.radians(lon2 - lon1)
    lat1 = math.radians(lat1)
    lat2 = math.radians(lat2)
    h = math.sin(dlat / 2) ** 2 + math.cos(lat1) * math.cos(lat2) * math.sin(dlon / 2) ** 2
    return 2 * radius * math.asin(min(1.0, math.sqrt(h)))


def clean_stream_segments(
    streams: dict[str, Any],
) -> tuple[list[list[tuple[float, float]]], dict[str, Any]]:
    """Build route segments from Strava streams, splitting implausible GPS jumps."""
    latlng = streams.get("latlng", {}).get("data", [])
    times = streams.get("time", {}).get("data", [])

    # Edge case: empty stream
    if not latlng:
        return [], {"points": 0, "segments": 0, "splits": 0, "truncated": False}

    # Edge case: excessively long stream, truncate with warning
    truncated = False
    if len(latlng) > settings.stream_max_points:
        print(
            f"Warning: stream has {len(latlng)} points, truncating to {settings.stream_max_points}"
        )
        latlng = latlng[: settings.stream_max_points]
        if len(times) > settings.stream_max_points:
            times = times[: settings.stream_max_points]
        truncated = True

    has_times = len(times) == len(latlng)
    segments: list[list[tuple[float, float]]] = []
    current: list[tuple[float, float]] = []
    split_count = 0

    for i, point in enumerate(latlng):
        current_point = (point[0], point[1])
        if not current:
            current.append(current_point)
            continue

        previous_point = current[-1]
        distance = _distance_meters(previous_point, current_point)
        dt = None
        speed = None
        if has_times and i > 0:
            dt = times[i] - times[i - 1]
            if dt and dt > 0:
                speed = distance / dt

        split = distance > settings.stream_max_segment_meters
        if (
            dt is not None
            and dt > settings.stream_max_time_gap_seconds
            and distance > settings.stream_gap_min_meters
        ):
            split = True
        if (
            speed is not None
            and speed > settings.stream_max_speed_mps
            and distance > settings.stream_gap_min_meters
        ):
            split = True

        if split:
            segments.append(current)
            current = [current_point]
            split_count += 1
        else:
            current.append(current_point)

    if current:
        segments.append(current)

    # Edge case: all points were filtered into single-point segments
    # This means the data is unusable — treat as no usable GPS
    if segments and all(len(seg) <= 1 for seg in segments):
        return [], {
            "points": len(latlng),
            "segments": 0,
            "splits": split_count,
            "truncated": truncated,
            "warning": "all points filtered — every segment is a single point",
        }

    return segments, {
        "points": len(latlng),
        "segments": len(segments),
        "splits": split_count,
        "truncated": truncated,
    }


def fetch_and_store_summaries(page: int = 1, per_page: int = 200) -> dict[str, Any]:
    """Fetch activity summaries and store new ones."""
    activities = get_activities(page=page, per_page=per_page)
    with get_db() as conn:
        stored = 0
        updated = 0
        skipped = 0
        ignored = 0
        for a in activities:
            aid = a["id"]
            summary = _summary_polyline(a)
            sport_type = a.get("sport_type", "")
            has_gps = bool(summary)
            status = _pending_status(has_gps, sport_type)
            existing = conn.execute(
                "SELECT id, status, has_gps FROM activities WHERE id = ?", (aid,)
            ).fetchone()
            if existing:
                if summary:
                    cur = conn.execute(
                        """
                        UPDATE activities
                        SET sport_type = ?,
                            summary_polyline = CASE
                                WHEN summary_polyline IS NULL OR summary_polyline = '' THEN ?
                                ELSE summary_polyline
                            END,
                            has_gps = 1,
                            status = CASE
                                WHEN ? = 'skipped_ignored_sport' THEN 'skipped_ignored_sport'
                                WHEN status IN ('skipped_no_gps', 'skipped_ignored_sport') THEN 'pending'
                                ELSE status
                            END
                        WHERE id = ?
                        """,
                        (sport_type, summary, status, aid),
                    )
                    updated += cur.rowcount
                    if status == "skipped_ignored_sport":
                        ignored += cur.rowcount
                elif _is_ignored_sport(sport_type):
                    cur = conn.execute(
                        "UPDATE activities SET sport_type = ?, status = 'skipped_ignored_sport' WHERE id = ?",
                        (sport_type, aid),
                    )
                    ignored += cur.rowcount
                elif existing["status"] == "pending" and not existing["has_gps"]:
                    cur = conn.execute(
                        "UPDATE activities SET status = 'skipped_no_gps' WHERE id = ?",
                        (aid,),
                    )
                    skipped += cur.rowcount
                continue
            conn.execute(
                """
                INSERT INTO activities
                (id, start_utc, start_local, timezone, sport_type, summary_polyline, has_gps, status, tile_engine)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    aid,
                    a["start_date"],
                    a["start_date_local"],
                    a.get("timezone", ""),
                    sport_type,
                    summary,
                    int(has_gps),
                    status,
                    make_engine().id,
                ),
            )
            if status == "skipped_ignored_sport":
                ignored += 1
            elif not has_gps:
                skipped += 1
            stored += 1
        conn.commit()
    return {
        "fetched": len(activities),
        "stored": stored,
        "updated": updated,
        "skipped": skipped,
        "ignored": ignored,
    }


def _activity_row(activity_id: int) -> sqlite3.Row | None:
    with get_db() as conn:
        return conn.execute("SELECT * FROM activities WHERE id = ?", (activity_id,)).fetchone()  # type: ignore[no-any-return]


def _set_activity_status(activity_id: int, status: str, error: str | None = None) -> None:
    with get_db() as conn:
        if error is None:
            conn.execute(
                "UPDATE activities SET status = ?, last_error = NULL WHERE id = ?",
                (status, activity_id),
            )
        else:
            conn.execute(
                "UPDATE activities SET status = ?, last_error = ? WHERE id = ?",
                (status, error, activity_id),
            )
        conn.commit()


def _prior_activity_tiles(
    conn: sqlite3.Connection,
    tile_kind: str,
    tiles: set[str],
    start_local: str,
    activity_id: int,
) -> set[str]:
    seen: set[str] = set()
    tile_list = list(tiles)
    for i in range(0, len(tile_list), 500):
        chunk = tile_list[i : i + 500]
        placeholders = ",".join("?" for _ in chunk)
        rows = conn.execute(
            f"""
            SELECT DISTINCT at.tile_id
            FROM activity_tiles at
            JOIN activities a ON a.id = at.activity_id
            WHERE at.tile_kind = ?
              AND a.status = 'processed'
              AND a.id != ?
              AND a.start_local < ?
              AND at.tile_id IN ({placeholders})
            """,
            (tile_kind, activity_id, start_local, *chunk),
        ).fetchall()
        seen.update(r["tile_id"] for r in rows)
    return seen


def compute_historical_novelty(
    activity_id: int,
    start_local: str,
    squadrats: set[str],
    squadratinhos: set[str],
) -> dict[str, Any]:
    """Compare activity tiles with processed local history before this activity."""
    with get_db() as conn:
        processed_before = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE status = 'processed' AND start_local < ?",
            (start_local,),
        ).fetchone()[0]
        total_squadrats_before = conn.execute(
            """
            SELECT COUNT(DISTINCT at.tile_id)
            FROM activity_tiles at
            JOIN activities a ON a.id = at.activity_id
            WHERE at.tile_kind = 'squadrat'
              AND a.status = 'processed'
              AND a.start_local < ?
            """,
            (start_local,),
        ).fetchone()[0]
        total_squadratinhos_before = conn.execute(
            """
            SELECT COUNT(DISTINCT at.tile_id)
            FROM activity_tiles at
            JOIN activities a ON a.id = at.activity_id
            WHERE at.tile_kind = 'squadratinho'
              AND a.status = 'processed'
              AND a.start_local < ?
            """,
            (start_local,),
        ).fetchone()[0]
        seen_squadrats = _prior_activity_tiles(
            conn, "squadrat", squadrats, start_local, activity_id
        )
        seen_squadratinhos = _prior_activity_tiles(
            conn, "squadratinho", squadratinhos, start_local, activity_id
        )

    new_squadrats = squadrats - seen_squadrats
    new_squadratinhos = squadratinhos - seen_squadratinhos
    return {
        "processed_before": processed_before,
        "total_squadrats_before": total_squadrats_before,
        "total_squadratinhos_before": total_squadratinhos_before,
        "seen_squadrats": len(seen_squadrats),
        "seen_squadratinhos": len(seen_squadratinhos),
        "new_squadrats": len(new_squadrats),
        "new_squadratinhos": len(new_squadratinhos),
        "total_squadrats_after": total_squadrats_before + len(new_squadrats),
        "total_squadratinhos_after": total_squadratinhos_before + len(new_squadratinhos),
    }


def _store_activity_tiles(
    row: sqlite3.Row,
    points: list[tuple[float, float]],
    source: str,
    segments: list[list[tuple[float, float]]] | None = None,
) -> dict[str, Any]:
    """Compute tiles from coordinates and persist the result."""
    engine = make_engine()
    activity_id = row["id"]
    if segments is None:
        segments = [points] if points else []

    if not any(segments):
        _set_activity_status(activity_id, "skipped_no_gps")
        return {"status": "skipped_no_gps", "activity_id": activity_id, "source": source}

    squadrats, squadratinhos = engine.tiles_for_segments(segments)

    with get_db() as conn:
        start_local = row["start_local"]
        existing_squadrats = _prior_activity_tiles(
            conn, "squadrat", squadrats, start_local, activity_id
        )
        existing_squadratinhos = _prior_activity_tiles(
            conn, "squadratinho", squadratinhos, start_local, activity_id
        )

        new_squadrats = squadrats - existing_squadrats
        new_squadratinhos = squadratinhos - existing_squadratinhos

        conn.execute("DELETE FROM activity_tiles WHERE activity_id = ?", (activity_id,))
        conn.executemany(
            "INSERT INTO activity_tiles (activity_id, tile_kind, tile_id, is_new) VALUES (?, 'squadrat', ?, ?)",
            [(activity_id, t, 1 if t in new_squadrats else 0) for t in squadrats],
        )
        conn.executemany(
            "INSERT INTO activity_tiles (activity_id, tile_kind, tile_id, is_new) VALUES (?, 'squadratinho', ?, ?)",
            [(activity_id, t, 1 if t in new_squadratinhos else 0) for t in squadratinhos],
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
            [("squadrat", t, activity_id, start_local) for t in new_squadrats],
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
            [("squadratinho", t, activity_id, start_local) for t in new_squadratinhos],
        )

        conn.execute(
            """
            UPDATE activities
            SET status = ?, squadrat_count = ?, squadratinho_count = ?,
                new_squadrat_count = ?, new_squadratinho_count = ?,
                processed_at = ?, tile_engine = ?, tile_source = ?, last_error = NULL
            WHERE id = ?
            """,
            (
                "processed",
                len(squadrats),
                len(squadratinhos),
                len(new_squadrats),
                len(new_squadratinhos),
                datetime.now(tz=timezone.utc).isoformat(),
                engine.id,
                source,
                activity_id,
            ),
        )
        conn.commit()

    return {
        "status": "processed",
        "activity_id": activity_id,
        "new_squadrats": len(new_squadrats),
        "new_squadratinhos": len(new_squadratinhos),
        "squadrats": len(squadrats),
        "squadratinhos": len(squadratinhos),
        "source": source,
    }


def compute_activity_tiles(activity_id: int) -> dict[str, Any]:
    """Compute and store tiles from full Strava streams. Returns counts."""
    row = _activity_row(activity_id)
    if row is None:
        raise ValueError(f"Activity {activity_id} not found")

    # Fetch full GPS stream regardless of has_gps flag
    try:
        streams = get_activity_streams(activity_id, keys="latlng,time")
    except Exception as e:
        error_msg = str(classify_strava_error(e))
        _set_activity_status(activity_id, "failed", error_msg)
        return {"status": "failed", "activity_id": activity_id, "error": error_msg}

    segments, stream_stats = clean_stream_segments(streams)
    if not any(segments):
        _set_activity_status(activity_id, "skipped_no_gps")
        return {"status": "skipped_no_gps", "activity_id": activity_id, "source": "streams_clean"}

    result = _store_activity_tiles(row, [], "streams_clean", segments=segments)
    result.update(stream_stats)
    return result


def compute_activity_tiles_from_summary(activity_id: int) -> dict[str, Any]:
    """Compute tiles from stored summary polyline, falling back to streams if needed."""
    row = _activity_row(activity_id)
    if row is None:
        raise ValueError(f"Activity {activity_id} not found")

    summary = row["summary_polyline"]
    if summary:
        try:
            points = polyline.decode(summary)
        except Exception:
            points = []
        if points:
            return _store_activity_tiles(row, points, "summary_polyline")

    return compute_activity_tiles(activity_id)


def compute_period_totals(start_local: str) -> tuple[int, int]:
    """Return (month_total, week_total) for new squadrats up to and including this activity."""
    dt = _parse_local(start_local)
    week_start = _week_start(dt)
    month_start = _month_start(dt)

    with get_db() as conn:
        month_total = conn.execute(
            """
            SELECT COALESCE(SUM(new_squadrat_count), 0)
            FROM activities
            WHERE status = 'processed'
              AND start_local >= ?
              AND start_local <= ?
            """,
            (month_start.isoformat(), start_local),
        ).fetchone()[0]

        week_total = conn.execute(
            """
            SELECT COALESCE(SUM(new_squadrat_count), 0)
            FROM activities
            WHERE status = 'processed'
              AND start_local >= ?
              AND start_local <= ?
            """,
            (week_start.isoformat(), start_local),
        ).fetchone()[0]

    return int(month_total), int(week_total)


def compute_total_unique_squadrats_through(activity_id: int, start_local: str) -> int:
    """Return total unique Squadrats seen before this activity plus this activity."""
    with get_db() as conn:
        total = conn.execute(
            """
            SELECT COUNT(DISTINCT at.tile_id)
            FROM activity_tiles at
            JOIN activities a ON a.id = at.activity_id
            WHERE at.tile_kind = 'squadrat'
              AND a.status = 'processed'
              AND (a.start_local < ? OR a.id = ?)
            """,
            (start_local, activity_id),
        ).fetchone()[0]
    return int(total)


def sync_once() -> dict[str, Any]:
    """Incremental sync: fetch recent, compute tiles, annotate new activities."""
    after = int((datetime.now(tz=timezone.utc) - timedelta(days=settings.sync_lookback_days)).timestamp())
    activities = get_activities(per_page=50, after=after)

    new_count = 0
    skipped = 0
    with get_db() as conn:
        for a in activities:
            aid = a["id"]
            summary = _summary_polyline(a)
            sport_type = a.get("sport_type", "")
            has_gps = bool(summary)
            status = _pending_status(has_gps, sport_type)
            existing = conn.execute(
                "SELECT id, status, has_gps FROM activities WHERE id = ?", (aid,)
            ).fetchone()
            if existing:
                if summary:
                    conn.execute(
                        """
                        UPDATE activities
                        SET sport_type = ?,
                            summary_polyline = CASE
                                WHEN summary_polyline IS NULL OR summary_polyline = '' THEN ?
                                ELSE summary_polyline
                            END,
                            has_gps = 1,
                            status = CASE
                                WHEN ? = 'skipped_ignored_sport' THEN 'skipped_ignored_sport'
                                WHEN status IN ('skipped_no_gps', 'skipped_ignored_sport') THEN 'pending'
                                ELSE status
                            END
                        WHERE id = ?
                        """,
                        (sport_type, summary, status, aid),
                    )
                elif _is_ignored_sport(sport_type):
                    conn.execute(
                        "UPDATE activities SET sport_type = ?, status = 'skipped_ignored_sport' WHERE id = ?",
                        (sport_type, aid),
                    )
                elif existing["status"] == "pending" and not existing["has_gps"]:
                    cur = conn.execute(
                        "UPDATE activities SET status = 'skipped_no_gps' WHERE id = ?",
                        (aid,),
                    )
                    skipped += cur.rowcount
                continue
            if not existing:
                conn.execute(
                    """
                    INSERT INTO activities
                    (id, start_utc, start_local, timezone, sport_type, summary_polyline, has_gps, status, tile_engine)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                    """,
                    (
                        aid,
                        a["start_date"],
                        a["start_date_local"],
                        a.get("timezone", ""),
                        sport_type,
                        summary,
                        int(has_gps),
                        status,
                        make_engine().id,
                    ),
                )
                new_count += 1
        conn.commit()

    # Process all pending (tile computation)
    with get_db() as conn:
        rows = conn.execute(
            "SELECT id FROM activities WHERE status = 'pending' ORDER BY start_local"
        ).fetchall()

    processed = 0
    for row in track(rows, description="Processing tiles", disable=not rows):
        result = compute_activity_tiles(row["id"])
        if result["status"] == "processed":
            processed += 1

    # Annotate only recent unannotated activities (not old ones)
    annotation_cutoff = (
        datetime.now(tz=timezone.utc) - timedelta(days=settings.sync_annotation_window_days)  # noqa: UP017
    ).isoformat()
    with get_db() as conn:
        rows = conn.execute(
            "SELECT id FROM activities WHERE status = 'processed' AND (annotation_status IS NULL OR annotation_status = 'none') AND start_local >= ? ORDER BY start_local",
            (annotation_cutoff,),
        ).fetchall()

    annotated = 0
    for row in track(rows, description="Annotating", disable=not rows):
        result = annotate_activity(row["id"])
        if result["status"] == "annotated":
            annotated += 1
            print(f"Annotated {row['id']}: {result['line']}")

    return {
        "new_activities": new_count,
        "processed": processed,
        "annotated": annotated,
        "skipped": skipped,
    }


def retry_failed() -> dict[str, Any]:
    """Retry failed processing or annotation."""
    from rich.console import Console

    console = Console()

    with get_db() as conn:
        rows = conn.execute(
            "SELECT id FROM activities WHERE status = 'failed' ORDER BY start_local"
        ).fetchall()

    total_failed = len(rows)
    retried = 0
    success = 0
    for row in track(rows, description="Retrying failed processing", disable=not total_failed):
        result = compute_activity_tiles(row["id"])
        retried += 1
        if result["status"] == "processed":
            success += 1
        elif result.get("error"):
            console.log(f"Activity {row['id']}: {result.get('error', '')[:120]}")

    # Also retry failed annotations
    with get_db() as conn:
        rows = conn.execute(
            "SELECT id FROM activities WHERE annotation_status = 'failed' ORDER BY start_local"
        ).fetchall()

    total_anno_failed = len(rows)
    for row in track(
        rows, description="Retrying failed annotations", disable=not total_anno_failed
    ):
        result = annotate_activity(row["id"])
        retried += 1
        if result["status"] == "annotated":
            success += 1

    return {"retried": retried, "success": success}


def annotate_activity(activity_id: int) -> dict[str, Any]:
    """Backward-compatible wrapper for :func:`tileharvester.annotate.annotate_activity`."""
    from tileharvester.annotate import annotate_activity as _annotate_activity

    return _annotate_activity(activity_id)


def backfill(limit: int | None = None) -> dict[str, Any]:
    """Backward-compatible wrapper for :func:`tileharvester.backfill.backfill`."""
    from tileharvester.backfill import backfill as _backfill

    return _backfill(limit=limit)


def recompute_novelty_from_stored_tiles() -> dict[str, Any]:
    """Backward-compatible wrapper for recomputing novelty from stored tiles."""
    from tileharvester.recompute import (
        recompute_novelty_from_stored_tiles as _recompute_novelty_from_stored_tiles,
    )

    return _recompute_novelty_from_stored_tiles()


def refine_streams(limit: int | None = None, force: bool = False) -> dict[str, Any]:
    """Backward-compatible wrapper for :func:`tileharvester.refine.refine_streams`."""
    from tileharvester.refine import refine_streams as _refine_streams

    return _refine_streams(limit=limit, force=force)


def recompute_all() -> dict[str, Any]:
    """Backward-compatible wrapper for :func:`tileharvester.recompute.recompute_all`."""
    from tileharvester.recompute import recompute_all as _recompute_all

    return _recompute_all()
