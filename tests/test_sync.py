"""Tests for sync orchestration and activity processing."""

from datetime import datetime, timezone
from typing import Any

import tileharvester.sync as sync_mod
from tileharvester.config import settings
from tileharvester.db import get_db, get_setting, set_setting


def _insert_activity(
    activity_id: int,
    *,
    start_utc: str = "2026-07-20T10:00:00Z",
    start_local: str = "2026-07-20T12:00:00",
    status: str = "pending",
    annotation_status: str = "none",
    has_gps: int = 0,
    new_squadrat_count: int = 0,
) -> None:
    with get_db() as conn:
        conn.execute(
            """
            INSERT INTO activities
                (id, start_utc, start_local, has_gps, status, annotation_status,
                 new_squadrat_count)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            """,
            (
                activity_id,
                start_utc,
                start_local,
                has_gps,
                status,
                annotation_status,
                new_squadrat_count,
            ),
        )
        conn.commit()


def test_period_boundaries_start_at_midnight() -> None:
    activity_time = datetime(2026, 7, 22, 18, 45, 12, 123456)

    assert sync_mod._week_start(activity_time) == datetime(2026, 7, 20)
    assert sync_mod._month_start(activity_time) == datetime(2026, 7, 1)


def test_period_totals_include_earlier_activity_on_first_day(isolated_db) -> None:
    del isolated_db
    _insert_activity(
        1,
        start_local="2026-07-20T08:00:00",
        status="processed",
        new_squadrat_count=5,
    )
    _insert_activity(
        2,
        start_local="2026-07-20T12:00:00",
        status="processed",
        new_squadrat_count=3,
    )

    assert sync_mod.compute_period_totals("2026-07-20T12:00:00") == (8, 8)


def test_sync_boundary_falls_back_to_latest_stored_activity(isolated_db, monkeypatch) -> None:
    del isolated_db
    monkeypatch.setattr(settings, "sync_lookback_days", 7)
    _insert_activity(1, start_utc="2026-05-10T10:00:00Z")

    after = sync_mod._sync_after_timestamp(datetime(2026, 7, 26, 18, 0, tzinfo=timezone.utc))

    assert after == int(datetime(2026, 5, 3, 10, 0, tzinfo=timezone.utc).timestamp())


def test_sync_boundary_prefers_successful_poll_cursor(isolated_db, monkeypatch) -> None:
    del isolated_db
    monkeypatch.setattr(settings, "sync_lookback_days", 7)
    _insert_activity(1, start_utc="2026-05-10T10:00:00Z")
    set_setting("last_successful_sync_at", "2026-07-25T09:30:00+00:00")

    after = sync_mod._sync_after_timestamp(datetime(2026, 7, 26, 18, 0, tzinfo=timezone.utc))

    assert after == int(datetime(2026, 7, 18, 9, 30, tzinfo=timezone.utc).timestamp())


def test_activity_without_summary_is_queued_for_stream_check() -> None:
    assert sync_mod._pending_status("Ride") == "pending"
    assert sync_mod._pending_status("VirtualRide") == "skipped_ignored_sport"


def test_recent_activity_fetch_follows_pagination(monkeypatch) -> None:
    calls: list[tuple[int, int, int | None]] = []

    def fake_get_activities(
        page: int = 1,
        per_page: int = 200,
        after: int | None = None,
        before: int | None = None,
    ) -> list[dict[str, Any]]:
        del before
        calls.append((page, per_page, after))
        return [{"id": 1}, {"id": 2}] if page == 1 else [{"id": 3}]

    monkeypatch.setattr(sync_mod, "get_activities", fake_get_activities)

    activities = sync_mod._fetch_recent_activities(after=123, per_page=2)

    assert [activity["id"] for activity in activities] == [1, 2, 3]
    assert calls == [(1, 2, 123), (2, 2, 123)]


def test_stream_fallback_marks_activity_as_gps(isolated_db, monkeypatch) -> None:
    del isolated_db
    _insert_activity(1)

    def fake_streams(activity_id: int, keys: str):
        del activity_id, keys
        return {
            "latlng": {"data": [[52.0, 5.0], [52.0001, 5.0001]]},
            "time": {"data": [0, 10]},
        }

    monkeypatch.setattr(
        sync_mod,
        "get_activity_streams",
        fake_streams,
    )

    result = sync_mod.compute_activity_tiles(1)

    assert result["status"] == "processed"
    with get_db() as conn:
        row = conn.execute("SELECT status, has_gps FROM activities WHERE id = 1").fetchone()
    assert row["status"] == "processed"
    assert row["has_gps"] == 1


def test_sync_uses_utc_timestamp_for_annotation_eligibility(isolated_db, monkeypatch) -> None:
    del isolated_db
    current_utc = datetime.now(tz=timezone.utc)
    _insert_activity(
        1,
        start_utc=current_utc.strftime("%Y-%m-%dT%H:%M:%SZ"),
        start_local="2000-01-01T00:00:00",
        status="processed",
        has_gps=1,
    )
    annotated: list[int] = []
    monkeypatch.setattr(sync_mod, "_fetch_recent_activities", lambda _after: [])
    monkeypatch.setattr(
        sync_mod,
        "annotate_activity",
        lambda activity_id: (
            annotated.append(activity_id) or {"status": "annotated", "line": "updated"}
        ),
    )

    result = sync_mod.sync_once()

    assert result["annotated"] == 1
    assert annotated == [1]
    assert get_setting("last_successful_sync_at") is not None


def test_sync_rewrites_old_annotations_when_enabled(isolated_db, monkeypatch) -> None:
    del isolated_db
    _insert_activity(
        1,
        start_utc="2000-01-01T00:00:00Z",
        start_local="2000-01-01T01:00:00",
        status="processed",
        has_gps=1,
    )
    annotated: list[int] = []
    monkeypatch.setattr(settings, "rewrite_existing_annotations", True)
    monkeypatch.setattr(sync_mod, "_fetch_recent_activities", lambda _after: [])
    monkeypatch.setattr(
        sync_mod,
        "annotate_activity",
        lambda activity_id: (
            annotated.append(activity_id) or {"status": "annotated", "line": "updated"}
        ),
    )

    sync_mod.sync_once()

    assert annotated == [1]
