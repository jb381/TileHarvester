"""Sync, backfill, and activity processing logic."""
import math
from datetime import datetime, timedelta
from typing import Optional

import polyline

from tileharvester.config import settings
from tileharvester.db import get_db
from tileharvester.strava_client import (
    get_activities,
    get_activity,
    get_activity_streams,
    update_activity_description,
)
from tileharvester.tile_engine import make_engine
from tileharvester.descriptions import update_description_line


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


def _summary_polyline(activity: dict) -> str | None:
    summary = (activity.get("map") or {}).get("summary_polyline")
    return summary or None


def _is_ignored_sport(sport_type: str | None) -> bool:
    return bool(sport_type and sport_type in settings.ignored_sports)


def _pending_status(has_gps: bool, sport_type: str | None) -> str:
    if _is_ignored_sport(sport_type):
        return "skipped_ignored_sport"
    return "pending" if has_gps else "skipped_no_gps"


def _print_progress(label: str, current: int, total: int) -> None:
    """Render a compact in-place progress bar."""
    if total <= 0:
        print(f"{label}: 0/0")
        return

    width = 30
    ratio = min(max(current / total, 0), 1)
    filled = round(width * ratio)
    bar = "#" * filled + "-" * (width - filled)
    percent = ratio * 100
    print(f"\r{label}: [{bar}] {current}/{total} {percent:5.1f}%", end="", flush=True)
    if current >= total:
        print()


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


def clean_stream_segments(streams: dict) -> tuple[list[list[tuple[float, float]]], dict]:
    """Build route segments from Strava streams, splitting implausible GPS jumps."""
    latlng = streams.get("latlng", {}).get("data", [])
    times = streams.get("time", {}).get("data", [])
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
        if dt is not None and dt > settings.stream_max_time_gap_seconds and distance > settings.stream_gap_min_meters:
            split = True
        if speed is not None and speed > settings.stream_max_speed_mps and distance > settings.stream_gap_min_meters:
            split = True

        if split:
            segments.append(current)
            current = [current_point]
            split_count += 1
        else:
            current.append(current_point)

    if current:
        segments.append(current)

    return segments, {"points": len(latlng), "segments": len(segments), "splits": split_count}


def fetch_and_store_summaries(page: int = 1, per_page: int = 200) -> dict:
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
            existing = conn.execute("SELECT id, status, has_gps FROM activities WHERE id = ?", (aid,)).fetchone()
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
    return {"fetched": len(activities), "stored": stored, "updated": updated, "skipped": skipped, "ignored": ignored}


def _activity_row(activity_id: int):
    with get_db() as conn:
        return conn.execute("SELECT * FROM activities WHERE id = ?", (activity_id,)).fetchone()


def _set_activity_status(activity_id: int, status: str, error: str | None = None) -> None:
    with get_db() as conn:
        if error is None:
            conn.execute("UPDATE activities SET status = ?, last_error = NULL WHERE id = ?", (status, activity_id))
        else:
            conn.execute(
                "UPDATE activities SET status = ?, last_error = ? WHERE id = ?",
                (status, error, activity_id),
            )
        conn.commit()


def _prior_activity_tiles(conn, tile_kind: str, tiles: set[str], start_local: str, activity_id: int) -> set[str]:
    seen = set()
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
) -> dict:
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
        seen_squadrats = _prior_activity_tiles(conn, "squadrat", squadrats, start_local, activity_id)
        seen_squadratinhos = _prior_activity_tiles(conn, "squadratinho", squadratinhos, start_local, activity_id)

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
    row,
    points: list[tuple[float, float]],
    source: str,
    segments: list[list[tuple[float, float]]] | None = None,
) -> dict:
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
        existing_squadrats = _prior_activity_tiles(conn, "squadrat", squadrats, start_local, activity_id)
        existing_squadratinhos = _prior_activity_tiles(conn, "squadratinho", squadratinhos, start_local, activity_id)

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
                datetime.utcnow().isoformat(),
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


def compute_activity_tiles(activity_id: int) -> dict:
    """Compute and store tiles from full Strava streams. Returns counts."""
    row = _activity_row(activity_id)
    if row is None:
        raise ValueError(f"Activity {activity_id} not found")

    if not row["has_gps"]:
        _set_activity_status(activity_id, "skipped_no_gps")
        return {"status": "skipped_no_gps", "activity_id": activity_id, "source": "streams_clean"}

    # Fetch full streams so suspicious gaps can be split before tile traversal.
    try:
        streams = get_activity_streams(activity_id, keys="latlng,time")
    except Exception as e:
        _set_activity_status(activity_id, "failed", str(e))
        return {"status": "failed", "activity_id": activity_id, "error": str(e)}

    segments, stream_stats = clean_stream_segments(streams)
    if not any(segments):
        _set_activity_status(activity_id, "skipped_no_gps")
        return {"status": "skipped_no_gps", "activity_id": activity_id, "source": "streams_clean"}

    result = _store_activity_tiles(row, [], "streams_clean", segments=segments)
    result.update(stream_stats)
    return result


def compute_activity_tiles_from_summary(activity_id: int) -> dict:
    """Compute tiles from stored summary polyline, falling back to streams if needed."""
    row = _activity_row(activity_id)
    if row is None:
        raise ValueError(f"Activity {activity_id} not found")

    if not row["has_gps"]:
        _set_activity_status(activity_id, "skipped_no_gps")
        return {"status": "skipped_no_gps", "activity_id": activity_id, "source": "summary_polyline"}

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


def annotate_activity(activity_id: int) -> dict:
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
    total_unique = compute_total_unique_squadrats_through(activity_id, start_local) + settings.squadrat_offset
    emoji = settings.description_emoji
    line = (
        f"{emoji} TileHarvester: {total_unique:,} Squadrats · "
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
                (annotation_status, line, datetime.utcnow().isoformat(), activity_id),
            )
            conn.commit()
        return {
            "status": "annotated",
            "activity_id": activity_id,
            "line": line,
        }
    except Exception as e:
        with get_db() as conn:
            conn.execute(
                "UPDATE activities SET annotation_status = ?, last_error = ? WHERE id = ?",
                ("failed", str(e), activity_id),
            )
            conn.commit()
        return {"status": "annotation_failed", "activity_id": activity_id, "error": str(e)}


def backfill(limit: Optional[int] = None) -> dict:
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
        result = fetch_and_store_summaries(page=page, per_page=per_page)
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
        if fetched < per_page or (limit and total_fetched >= limit):
            break

    print(
        f"Stored {total_stored} new summaries, updated {total_updated} existing summaries, "
        f"marked {total_skipped} non-GPS skipped, {total_ignored} ignored sports."
    )

    with get_db() as conn:
        rows = conn.execute(
            "SELECT id FROM activities WHERE status = 'pending' AND has_gps = 1 ORDER BY start_local"
        ).fetchall()

    total = len(rows)
    print(f"Processing {total} activities with GPS...")
    processed = 0
    summary_processed = 0
    stream_fallbacks = 0
    failed = 0
    if total:
        _print_progress("Processing", 0, total)
    for i, row in enumerate(rows, 1):
        result = compute_activity_tiles_from_summary(row["id"])
        if result["status"] in ("processed", "skipped_no_gps"):
            processed += 1
            if result.get("source") == "summary_polyline":
                summary_processed += 1
            elif result.get("source") == "streams_clean":
                stream_fallbacks += 1
        else:
            failed += 1
            print()
            print(f"Activity {row['id']}: {result['status']} - {result.get('error', '')}")
        _print_progress("Processing", i, total)

    print(
        f"Backfill complete: {processed}/{total} processed "
        f"({summary_processed} from summary polylines, {stream_fallbacks} stream fallbacks, {failed} failed)."
    )
    return {
        "stored": total_stored,
        "processed": processed,
        "summary_processed": summary_processed,
        "stream_fallbacks": stream_fallbacks,
        "failed": failed,
        "skipped": total_skipped,
        "ignored": total_ignored,
    }


def sync_once() -> dict:
    """Incremental sync: fetch recent, compute tiles, annotate new activities."""
    after = int((datetime.utcnow() - timedelta(days=7)).timestamp())
    activities = get_activities(per_page=50, after=after)

    new_count = 0
    with get_db() as conn:
        for a in activities:
            aid = a["id"]
            summary = _summary_polyline(a)
            sport_type = a.get("sport_type", "")
            has_gps = bool(summary)
            status = _pending_status(has_gps, sport_type)
            existing = conn.execute("SELECT id, status, has_gps FROM activities WHERE id = ?", (aid,)).fetchone()
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
                    conn.execute(
                        "UPDATE activities SET status = 'skipped_no_gps' WHERE id = ?",
                        (aid,),
                    )
                    skipped += cur.rowcount
                elif summary and not existing["has_gps"]:
                    conn.execute(
                        "UPDATE activities SET has_gps = 1, status = 'pending' WHERE id = ?",
                        (aid,),
                    )
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
            "SELECT id FROM activities WHERE status = 'pending' AND has_gps = 1 ORDER BY start_local"
        ).fetchall()

    processed = 0
    for row in rows:
        result = compute_activity_tiles(row["id"])
        if result["status"] == "processed":
            processed += 1

    # Annotate only recent unannotated activities (not old ones)
    one_day_ago = (datetime.utcnow() - timedelta(days=1)).isoformat()
    with get_db() as conn:
        rows = conn.execute(
            "SELECT id FROM activities WHERE status = 'processed' AND (annotation_status IS NULL OR annotation_status = 'none') AND start_local >= ? ORDER BY start_local",
            (one_day_ago,),
        ).fetchall()

    annotated = 0
    for row in rows:
        result = annotate_activity(row["id"])
        if result["status"] == "annotated":
            annotated += 1
            print(f"Annotated {row['id']}: {result['line']}")

    return {"new_activities": new_count, "processed": processed, "annotated": annotated}


def retry_failed() -> dict:
    """Retry failed processing or annotation."""
    with get_db() as conn:
        rows = conn.execute(
            "SELECT id FROM activities WHERE status = 'failed' ORDER BY start_local"
        ).fetchall()

    total_failed = len(rows)
    retried = 0
    success = 0
    if total_failed:
        print(f"Retrying {total_failed} failed activities...")
    for i, row in enumerate(rows, 1):
        if i % 10 == 0 or i == 1:
            print(f"  [{i}/{total_failed}] retrying {row['id']}...")
        result = compute_activity_tiles(row["id"])
        retried += 1
        if result["status"] == "processed":
            success += 1

    # Also retry failed annotations
    with get_db() as conn:
        rows = conn.execute(
            "SELECT id FROM activities WHERE annotation_status = 'failed' ORDER BY start_local"
        ).fetchall()

    total_anno_failed = len(rows)
    if total_anno_failed:
        print(f"Retrying {total_anno_failed} failed annotations...")
    for i, row in enumerate(rows, 1):
        if i % 10 == 0 or i == 1:
            print(f"  [{i}/{total_anno_failed}] retrying annotation {row['id']}...")
        result = annotate_activity(row["id"])
        retried += 1
        if result["status"] == "annotated":
            success += 1

    return {"retried": retried, "success": success}


def recompute_novelty_from_stored_tiles() -> dict:
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

            new_squadrats = squadrats - _prior_activity_tiles(conn, "squadrat", squadrats, start_local, aid)
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


def refine_streams(limit: Optional[int] = 80, force: bool = False) -> dict:
    """Refine stored activity tiles by fetching full Strava GPS streams."""
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
    if limit and limit > 0:
        limit_clause = "LIMIT ?"
        params.append(limit)

    with get_db() as conn:
        rows = conn.execute(
            f"""
            SELECT * FROM activities
            WHERE {' AND '.join(filters)}
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

    return {"refined": refined, "failed": failed, "selected": total, "rebuilt": novelty["rebuilt"], "splits": splits}


def recompute_all() -> dict:
    """Recompute activity tiles and novelty from stored summary polylines."""
    with get_db() as conn:
        conn.execute("DELETE FROM activity_tiles")
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
        conn.execute(
            """
            UPDATE activities
            SET squadrat_count = 0,
                squadratinho_count = 0,
                new_squadrat_count = 0,
                new_squadratinho_count = 0
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
    print(f"Recomputing {total} activities...")
    rebuilt = 0
    skipped = 0
    if total:
        _print_progress("Recomputing", 0, total)
    for i, row in enumerate(rows, 1):
        summary = row["summary_polyline"]
        if not summary:
            skipped += 1
            _print_progress("Recomputing", i, total)
            continue

        try:
            points = polyline.decode(summary)
        except Exception:
            points = []
        if not points:
            skipped += 1
            _print_progress("Recomputing", i, total)
            continue

        _store_activity_tiles(row, points, "summary_polyline")
        _print_progress("Recomputing", i, total)
        rebuilt += 1

    print(f"Recompute complete: {rebuilt}/{total} rebuilt, {skipped} skipped.")
    return {"rebuilt": rebuilt, "skipped": skipped}
