"""Core sync logic and shared utilities for activity processing."""

import math
import sqlite3
from datetime import datetime, timedelta, timezone
from typing import Any

import polyline
from rich.progress import track

from tileharvester.config import settings
from tileharvester.db import get_db, get_setting, set_setting
from tileharvester.kml_baseline import (
    activity_is_covered,
    activity_uses_baseline,
    baseline_tiles_on_route,
    effective_tile_count_through,
    get_baseline,
)
from tileharvester.strava_client import (
    classify_strava_error,
    get_activities,
    get_activity_streams,
)
from tileharvester.tile_engine import make_engine

_LAST_SUCCESSFUL_SYNC_KEY = "last_successful_sync_at"


def _week_start(dt: datetime) -> datetime:
    """ISO week start (Monday)."""
    return (dt - timedelta(days=dt.weekday())).replace(
        hour=0,
        minute=0,
        second=0,
        microsecond=0,
    )


def _month_start(dt: datetime) -> datetime:
    return dt.replace(day=1, hour=0, minute=0, second=0, microsecond=0)


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


def _parse_utc(utc_str: str) -> datetime:
    """Parse an ISO timestamp and normalize it to UTC."""
    normalized = utc_str[:-1] + "+00:00" if utc_str.endswith("Z") else utc_str
    parsed = datetime.fromisoformat(normalized)
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def _latest_stored_activity_utc() -> datetime | None:
    """Return the newest activity timestamp available in the local database."""
    with get_db() as conn:
        value = conn.execute("SELECT MAX(start_utc) FROM activities").fetchone()[0]
    if not value:
        return None
    try:
        return _parse_utc(value)
    except (TypeError, ValueError):
        return None


def _sync_after_timestamp(sync_started_at: datetime) -> int:
    """Choose a resilient Strava fetch boundary with an overlap window.

    Prefer the last successful poll so a server catches up after downtime.
    Existing installations without a cursor fall back to the newest locally
    stored activity. Empty databases retain the normal configured lookback.
    """
    if sync_started_at.tzinfo is None:
        sync_started_at = sync_started_at.replace(tzinfo=timezone.utc)
    else:
        sync_started_at = sync_started_at.astimezone(timezone.utc)

    anchor: datetime | None = None
    cursor = get_setting(_LAST_SUCCESSFUL_SYNC_KEY)
    if isinstance(cursor, str):
        try:
            anchor = _parse_utc(cursor)
        except ValueError:
            anchor = None
    if anchor is None:
        anchor = _latest_stored_activity_utc()

    # Do not let corrupt or future timestamps skip the current polling window.
    anchor = min(anchor or sync_started_at, sync_started_at)
    return int((anchor - timedelta(days=settings.sync_lookback_days)).timestamp())


def _summary_polyline(activity: dict[str, Any]) -> str | None:
    summary = (activity.get("map") or {}).get("summary_polyline")
    return summary or None


def _is_ignored_sport(sport_type: str | None) -> bool:
    return bool(sport_type and sport_type in settings.ignored_sports)


def _pending_status(sport_type: str | None) -> str:
    if _is_ignored_sport(sport_type):
        return "skipped_ignored_sport"
    return "pending"


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


def fetch_and_store_summaries(
    page: int = 1,
    per_page: int = 200,
    max_items: int | None = None,
) -> dict[str, Any]:
    """Fetch activity summaries and store new ones."""
    activities = get_activities(page=page, per_page=per_page)
    if max_items is not None:
        activities = activities[:max_items]
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
            status = _pending_status(sport_type)
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
                continue
            conn.execute(
                """
                INSERT INTO activities
                (id, start_utc, start_local, timezone, sport_type, summary_polyline,
                 has_gps, status, tile_engine, baseline_covered)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
                    int(activity_is_covered(conn, a["start_date"])),
                ),
            )
            if status == "skipped_ignored_sport":
                ignored += 1
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
    *,
    start_utc: str | None = None,
    use_baseline: bool = False,
) -> set[str]:
    seen: set[str] = set()
    tile_list = list(tiles)
    for i in range(0, len(tile_list), 500):
        chunk = tile_list[i : i + 500]
        placeholders = ",".join("?" for _ in chunk)
        if use_baseline:
            if start_utc is None:
                raise ValueError("start_utc is required for baseline-aware novelty")
            baseline = get_baseline(conn)
            if baseline is None:
                raise ValueError("Baseline-aware novelty requested without an installed baseline")
            rows = conn.execute(
                f"""
                SELECT DISTINCT at.tile_id
                FROM activity_tiles at
                JOIN activities a ON a.id = at.activity_id
                WHERE at.tile_kind = ?
                  AND a.status = 'processed'
                  AND a.id != ?
                  AND (a.baseline_covered = 1 OR julianday(a.start_utc) > julianday(?))
                  AND (
                      julianday(a.start_utc) < julianday(?)
                      OR (a.start_utc = ? AND a.id < ?)
                  )
                  AND at.tile_id IN ({placeholders})
                """,
                (
                    tile_kind,
                    activity_id,
                    baseline["as_of_utc"],
                    start_utc,
                    start_utc,
                    activity_id,
                    *chunk,
                ),
            ).fetchall()
        else:
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
    if use_baseline:
        seen.update(baseline_tiles_on_route(conn, tile_kind, tiles))
    return seen


def compute_historical_novelty(
    activity_id: int,
    start_local: str,
    squadrats: set[str],
    squadratinhos: set[str],
) -> dict[str, Any]:
    """Compare activity tiles with processed local history before this activity."""
    with get_db() as conn:
        activity = conn.execute("SELECT * FROM activities WHERE id = ?", (activity_id,)).fetchone()
        use_baseline = bool(activity and activity_uses_baseline(conn, activity))
        start_utc = activity["start_utc"] if activity else None
        processed_before = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE status = 'processed' AND start_local < ?",
            (start_local,),
        ).fetchone()[0]
        if activity is not None:
            total_squadrats_before = effective_tile_count_through(
                conn, "squadrat", activity, include_activity=False
            )
            total_squadratinhos_before = effective_tile_count_through(
                conn, "squadratinho", activity, include_activity=False
            )
        else:
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
            conn,
            "squadrat",
            squadrats,
            start_local,
            activity_id,
            start_utc=start_utc,
            use_baseline=use_baseline,
        )
        seen_squadratinhos = _prior_activity_tiles(
            conn,
            "squadratinho",
            squadratinhos,
            start_local,
            activity_id,
            start_utc=start_utc,
            use_baseline=use_baseline,
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
        start_utc = row["start_utc"]
        use_baseline = activity_uses_baseline(conn, row)
        existing_squadrats = _prior_activity_tiles(
            conn,
            "squadrat",
            squadrats,
            start_local,
            activity_id,
            start_utc=start_utc,
            use_baseline=use_baseline,
        )
        existing_squadratinhos = _prior_activity_tiles(
            conn,
            "squadratinho",
            squadratinhos,
            start_local,
            activity_id,
            start_utc=start_utc,
            use_baseline=use_baseline,
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
                processed_at = ?, tile_engine = ?, tile_source = ?, has_gps = 1,
                last_error = NULL
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
    del start_local  # Retained in the public helper signature for backwards compatibility.
    with get_db() as conn:
        activity = conn.execute("SELECT * FROM activities WHERE id = ?", (activity_id,)).fetchone()
        if activity is None:
            return 0
        return effective_tile_count_through(conn, "squadrat", activity, include_activity=True)


def _fetch_recent_activities(after: int, per_page: int = 200) -> list[dict[str, Any]]:
    """Fetch every activity after a timestamp, following Strava pagination."""
    activities: list[dict[str, Any]] = []
    page = 1
    while True:
        batch = get_activities(page=page, per_page=per_page, after=after)
        activities.extend(batch)
        if len(batch) < per_page:
            return activities
        page += 1


def sync_once() -> dict[str, Any]:
    """Incremental sync: fetch recent, compute tiles, annotate new activities."""
    sync_started_at = datetime.now(tz=timezone.utc)
    after = _sync_after_timestamp(sync_started_at)
    activities = _fetch_recent_activities(after)

    new_count = 0
    skipped = 0
    with get_db() as conn:
        for a in activities:
            aid = a["id"]
            summary = _summary_polyline(a)
            sport_type = a.get("sport_type", "")
            has_gps = bool(summary)
            status = _pending_status(sport_type)
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
                continue
            if not existing:
                conn.execute(
                    """
                    INSERT INTO activities
                    (id, start_utc, start_local, timezone, sport_type, summary_polyline,
                     has_gps, status, tile_engine, baseline_covered)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
                        int(activity_is_covered(conn, a["start_date"])),
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
        elif result["status"] == "skipped_no_gps":
            skipped += 1

    # Annotate only recent unannotated activities (not old ones)
    annotation_cutoff = (
        datetime.now(tz=timezone.utc) - timedelta(days=settings.sync_annotation_window_days)
    ).strftime("%Y-%m-%dT%H:%M:%SZ")
    with get_db() as conn:
        rows = conn.execute(
            """
            SELECT id FROM activities
            WHERE status = 'processed'
              AND (annotation_status IS NULL OR annotation_status = 'none')
              AND (start_utc >= ? OR ?)
            ORDER BY start_utc, id
            """,
            (annotation_cutoff, int(settings.rewrite_existing_annotations)),
        ).fetchall()

    annotated = 0
    for row in track(rows, description="Annotating", disable=not rows):
        result = annotate_activity(row["id"])
        if result["status"] == "annotated":
            annotated += 1
            print(f"Annotated {row['id']}: {result['line']}")

    set_setting(_LAST_SUCCESSFUL_SYNC_KEY, sync_started_at.isoformat())

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
