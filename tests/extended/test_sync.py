"""Tests for sync.py: core sync logic."""

from datetime import datetime

import pytest

from tileharvester.sync import (
    _distance_meters,
    _is_ignored_sport,
    _month_start,
    _parse_local,
    _pending_status,
    _summary_polyline,
    _week_start,
    clean_stream_segments,
    compute_historical_novelty,
    compute_period_totals,
    compute_total_unique_squadrats_through,
    fetch_and_store_summaries,
)


class TestWeekStart:
    def test_monday_returns_same_day(self):
        dt = datetime(2025, 1, 6)  # Monday
        result = _week_start(dt)
        assert result == dt

    def test_wednesday_returns_monday(self):
        dt = datetime(2025, 1, 8)  # Wednesday
        result = _week_start(dt)
        assert result == datetime(2025, 1, 6)

    def test_sunday_returns_monday(self):
        dt = datetime(2025, 1, 12)  # Sunday
        result = _week_start(dt)
        assert result == datetime(2025, 1, 6)


class TestMonthStart:
    def test_returns_first_of_month(self):
        dt = datetime(2025, 3, 15)
        result = _month_start(dt)
        assert result == datetime(2025, 3, 1)

    def test_already_first_of_month(self):
        dt = datetime(2025, 3, 1)
        result = _month_start(dt)
        assert result == datetime(2025, 3, 1)


class TestParseLocal:
    def test_basic_isoformat(self):
        result = _parse_local("2025-01-15T10:30:00")
        assert result == datetime(2025, 1, 15, 10, 30, 0)

    def test_strips_z_suffix(self):
        result = _parse_local("2025-01-15T10:30:00Z")
        assert result == datetime(2025, 1, 15, 10, 30, 0)

    def test_strips_timezone_offset(self):
        result = _parse_local("2025-01-15T10:30:00+01:00")
        assert result == datetime(2025, 1, 15, 10, 30, 0)


class TestSummaryPolyline:
    def test_returns_polyline_from_map(self):
        activity = {"map": {"summary_polyline": "abc123"}}
        assert _summary_polyline(activity) == "abc123"

    def test_returns_none_when_no_map(self):
        activity = {}
        assert _summary_polyline(activity) is None

    def test_returns_none_when_map_empty(self):
        activity = {"map": {}}
        assert _summary_polyline(activity) is None

    def test_returns_none_when_polyline_empty_string(self):
        activity = {"map": {"summary_polyline": ""}}
        assert _summary_polyline(activity) is None


class TestIsIgnoredSport:
    def test_known_ignored_sport(self, temp_settings, monkeypatch):
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(sync_mod, "settings", temp_settings)
        assert _is_ignored_sport("VirtualRide") is True

    def test_known_sport_not_ignored(self, temp_settings, monkeypatch):
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(sync_mod, "settings", temp_settings)
        assert _is_ignored_sport("Run") is False

    def test_none_sport(self, temp_settings, monkeypatch):
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(sync_mod, "settings", temp_settings)
        assert _is_ignored_sport(None) is False


class TestPendingStatus:
    def test_ignored_sport(self, temp_settings, monkeypatch):
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(sync_mod, "settings", temp_settings)
        assert _pending_status("VirtualRide") == "skipped_ignored_sport"

    def test_no_gps(self, temp_settings, monkeypatch):
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(sync_mod, "settings", temp_settings)
        assert _pending_status("Run") == "pending"

    def test_has_gps_not_ignored(self, temp_settings, monkeypatch):
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(sync_mod, "settings", temp_settings)
        assert _pending_status("Ride") == "pending"


class TestDistanceMeters:
    def test_same_point_is_zero(self):
        assert _distance_meters((0.0, 0.0), (0.0, 0.0)) == 0.0

    def test_small_distance(self):
        # 0.001 degree ≈ ~111m
        d = _distance_meters((0.0, 0.0), (0.0, 0.001))
        assert 100 < d < 120

    def test_larger_distance(self):
        d = _distance_meters((0.0, 0.0), (1.0, 0.0))
        assert 110000 < d < 112000


class TestCleanStreamSegments:
    def test_empty_stream(self):
        segments, stats = clean_stream_segments({})
        assert segments == []
        assert stats["points"] == 0

    def test_empty_latlng_list(self):
        segments, stats = clean_stream_segments({"latlng": {"data": []}})
        assert segments == []
        assert stats["points"] == 0

    def test_single_segment_no_gaps(self, sample_gps_stream):
        segments, stats = clean_stream_segments(sample_gps_stream)
        assert len(segments) == 1
        assert stats["points"] == 5
        assert stats["segments"] == 1
        assert stats["splits"] == 0

    def test_splits_on_large_distance(self):
        stream = {
            "latlng": {"data": [[0.0, 0.0], [1.0, 0.0]]},
            "time": {"data": [0, 10]},
        }
        # ~111km in 10s is >35m/s, and >300m segment → split
        segments, stats = clean_stream_segments(stream)
        assert stats["splits"] > 0 or len(segments) > 1

    def test_no_times_still_works(self, sample_gps_stream_no_times):
        segments, stats = clean_stream_segments(sample_gps_stream_no_times)
        assert len(segments) >= 1
        assert stats["points"] == 3

    def test_single_point_stream(self):
        stream = {"latlng": {"data": [[52.0, 4.5]]}}
        segments, stats = clean_stream_segments(stream)
        # Single point → single-point segment, which is filtered
        assert segments == []
        assert "warning" in stats

    def test_rejects_long_stream(self, temp_settings, monkeypatch):
        import tileharvester.sync as sync_mod

        s = temp_settings.model_copy()
        s.stream_max_points = 3
        monkeypatch.setattr(sync_mod, "settings", s)

        stream = {
            "latlng": {"data": [[52.0, 4.5 + i * 0.01] for i in range(100)]},
            "time": {"data": [i * 10 for i in range(100)]},
        }
        with pytest.raises(ValueError, match="TH_STREAM_MAX_POINTS"):
            clean_stream_segments(stream)


class TestFetchAndStoreSummaries:
    def test_stores_new_activities(
        self, temp_db, temp_settings, mock_strava_api, monkeypatch, sample_activity_summaries
    ):
        import tileharvester.config as config_mod
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(sync_mod, "settings", temp_settings)
        mock_strava_api["get"].return_value.json.return_value = sample_activity_summaries

        # Create a token file so _refresh_if_needed() doesn't fail
        from tileharvester.strava_client import _save_tokens

        _save_tokens({"access_token": "test", "refresh_token": "ref", "expires_at": 2_000_000_000})

        result = fetch_and_store_summaries()
        assert result["stored"] == 3
        assert result["skipped"] == 0  # Missing summaries stay pending for stream fetch
        assert result["ignored"] == 1  # VirtualRide

        # Verify stored in DB
        count = temp_db.execute("SELECT COUNT(*) FROM activities").fetchone()[0]
        assert count == 3
        missing_summary = temp_db.execute(
            "SELECT status FROM activities WHERE has_gps = 0 AND sport_type = 'Ride'"
        ).fetchone()
        assert missing_summary["status"] == "pending"

    def test_updates_existing_activities(
        self, temp_db, temp_settings, mock_strava_api, monkeypatch
    ):
        import tileharvester.config as config_mod
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(sync_mod, "settings", temp_settings)

        # Create a token file so _refresh_if_needed() doesn't fail
        from tileharvester.strava_client import _save_tokens

        _save_tokens({"access_token": "test", "refresh_token": "ref", "expires_at": 2_000_000_000})

        # Pre-insert an activity
        temp_db.execute(
            "INSERT INTO activities (id, start_utc, start_local, timezone, sport_type, has_gps, status) VALUES (?, ?, ?, ?, ?, ?, ?)",
            (100, "2025-02-01T08:00:00Z", "2025-02-01T08:00:00", "UTC", "Run", 0, "pending"),
        )
        temp_db.commit()

        summaries = [
            {
                "id": 100,
                "start_date": "2025-02-01T08:00:00Z",
                "start_date_local": "2025-02-01T08:00:00",
                "timezone": "UTC",
                "sport_type": "Run",
                "map": {"summary_polyline": "_p~iF~ps|U_ulLnnqC_mqNvxq`@"},
            }
        ]
        mock_strava_api["get"].return_value.json.return_value = summaries

        result = fetch_and_store_summaries()
        assert result["updated"] >= 1

        # Verify updated
        row = temp_db.execute("SELECT * FROM activities WHERE id = 100").fetchone()
        assert row["has_gps"] == 1

    def test_marks_ignored_sport_on_update(
        self, temp_db, temp_settings, mock_strava_api, monkeypatch
    ):
        import tileharvester.config as config_mod
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(sync_mod, "settings", temp_settings)

        # Create a token file so _refresh_if_needed() doesn't fail
        from tileharvester.strava_client import _save_tokens

        _save_tokens({"access_token": "test", "refresh_token": "ref", "expires_at": 2_000_000_000})

        temp_db.execute(
            "INSERT INTO activities (id, start_utc, start_local, timezone, sport_type, has_gps, status) VALUES (?, ?, ?, ?, ?, ?, ?)",
            (200, "2025-03-01T08:00:00Z", "2025-03-01T08:00:00", "UTC", "Run", 0, "pending"),
        )
        temp_db.commit()

        summaries = [
            {
                "id": 200,
                "start_date": "2025-03-01T08:00:00Z",
                "start_date_local": "2025-03-01T08:00:00",
                "timezone": "UTC",
                "sport_type": "VirtualRide",
                "map": {},
            }
        ]
        mock_strava_api["get"].return_value.json.return_value = summaries

        result = fetch_and_store_summaries()
        assert result["ignored"] >= 1


class TestComputeHistoricalNovelty:
    def test_reports_novelty_for_new_activity(self, seeded_db, temp_settings, monkeypatch):
        import tileharvester.config as config_mod
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(sync_mod, "settings", temp_settings)

        result = compute_historical_novelty(
            activity_id=3,
            start_local="2025-01-03T12:00:00",
            squadrats={"14:8000:8000", "14:9000:9000"},
            squadratinhos={"17:64000:64000", "17:99999:99999"},
        )
        # 14:8000:8000 already seen by activity 1, 14:9000:9000 is new
        assert result["seen_squadrats"] == 1
        assert result["new_squadrats"] == 1
        assert result["new_squadratinhos"] == 1

    def test_all_tiles_already_seen(self, seeded_db, temp_settings, monkeypatch):
        import tileharvester.config as config_mod
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(sync_mod, "settings", temp_settings)

        result = compute_historical_novelty(
            activity_id=3,
            start_local="2025-01-03T12:00:00",
            squadrats={"14:8000:8000"},
            squadratinhos={"17:64000:64000"},
        )
        assert result["new_squadrats"] == 0
        assert result["new_squadratinhos"] == 0


class TestComputePeriodTotals:
    def test_returns_month_and_week_totals(self, seeded_db, temp_settings, monkeypatch):
        import tileharvester.config as config_mod
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(sync_mod, "settings", temp_settings)

        month_total, week_total = compute_period_totals("2025-01-02T11:00:00")
        assert month_total == 8  # Both activities from month start
        assert week_total == 8  # Both activities (week starts Dec 30)

    def test_returns_zero_when_no_prior(self, seeded_db, temp_settings, monkeypatch):
        import tileharvester.config as config_mod
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(sync_mod, "settings", temp_settings)

        month_total, week_total = compute_period_totals("2025-02-01T10:00:00")
        assert month_total == 0
        assert week_total == 0


class TestComputeTotalUniqueSquadratsThrough:
    def test_counts_all_unique_squadrats(self, seeded_db, temp_settings, monkeypatch):
        import tileharvester.config as config_mod
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(sync_mod, "settings", temp_settings)

        total = compute_total_unique_squadrats_through(2, "2025-01-02T11:00:00")
        # 5 unique squadrats total (activities 1 + 2)
        assert total == 5
