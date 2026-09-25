"""Integration tests: full flow from summary to tiles to annotation."""

from unittest.mock import patch

from tileharvester.annotate import annotate_activity
from tileharvester.sync import (
    _store_activity_tiles,
    compute_activity_tiles,
    compute_activity_tiles_from_summary,
    fetch_and_store_summaries,
)


class TestEndToEndSummaryToTiles:
    """Full integration: store summary → compute tiles from summary → verify DB state."""

    def test_full_flow_summary_tiles(self, temp_db, temp_settings, mock_strava_api, monkeypatch):
        import tileharvester.config as config_mod
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(sync_mod, "settings", temp_settings)

        # Create token file for auth
        from tileharvester.strava_client import _save_tokens

        _save_tokens({"access_token": "test", "refresh_token": "ref", "expires_at": 2_000_000_000})

        # Step 1: Fetch and store activity summary
        summaries = [
            {
                "id": 500,
                "start_date": "2025-03-01T08:00:00Z",
                "start_date_local": "2025-03-01T08:00:00",
                "timezone": "UTC",
                "sport_type": "Run",
                "map": {"summary_polyline": "_p~iF~ps|U_ulLnnqC_mqNvxq`@"},
            }
        ]
        mock_strava_api["get"].return_value.json.return_value = summaries
        result = fetch_and_store_summaries()
        assert result["stored"] == 1

        # Step 2: Compute tiles from summary polyline
        with patch("tileharvester.sync.get_activity_streams") as mock_streams:
            mock_streams.return_value = {
                "latlng": {
                    "data": [
                        [52.0, 4.5],
                        [52.001, 4.501],
                        [52.002, 4.502],
                        [52.003, 4.503],
                        [52.004, 4.504],
                    ],
                },
                "time": {"data": [0, 10, 20, 30, 40]},
            }
            result = compute_activity_tiles_from_summary(500)

        assert result["status"] == "processed"
        assert result["source"] == "summary_polyline"

        # Verify activity_tiles populated
        tile_count = temp_db.execute(
            "SELECT COUNT(*) FROM activity_tiles WHERE activity_id = 500"
        ).fetchone()[0]
        assert tile_count > 0

        # Verify activities row updated
        row = temp_db.execute("SELECT * FROM activities WHERE id = 500").fetchone()
        assert row["status"] == "processed"
        assert row["tile_source"] == "summary_polyline"


class TestEndToEndFullStream:
    """Full integration: fetch streams → compute tiles → verify DB state."""

    def test_full_flow_stream_tiles(self, temp_db, temp_settings, mock_strava_api, monkeypatch):
        import tileharvester.config as config_mod
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(sync_mod, "settings", temp_settings)

        # Insert an activity that has GPS but needs processing
        temp_db.execute(
            """
            INSERT INTO activities (id, start_utc, start_local, timezone, sport_type, has_gps, status)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            """,
            (600, "2025-03-01T08:00:00Z", "2025-03-01T08:00:00", "UTC", "Run", 1, "pending"),
        )
        temp_db.commit()

        # Mock get_activity_streams
        with patch("tileharvester.sync.get_activity_streams") as mock_streams:
            mock_streams.return_value = {
                "latlng": {
                    "data": [
                        [52.0, 4.5],
                        [52.001, 4.501],
                        [52.002, 4.502],
                    ],
                },
                "time": {"data": [0, 10, 20]},
            }
            result = compute_activity_tiles(600)

        assert result["status"] == "processed"
        assert result["source"] == "streams_clean"

        row = temp_db.execute("SELECT * FROM activities WHERE id = 600").fetchone()
        assert row["status"] == "processed"
        assert row["tile_source"] == "streams_clean"
        assert row["squadrat_count"] > 0
        assert row["squadratinho_count"] > 0


class TestEndToEndAnnotation:
    """Full integration: process tiles → annotate Strava description."""

    def test_full_flow_annotate(self, temp_db, temp_settings, mock_strava_api, monkeypatch):
        import tileharvester.annotate as annotate_mod
        import tileharvester.config as config_mod
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(sync_mod, "settings", temp_settings)
        monkeypatch.setattr(annotate_mod, "settings", temp_settings)

        # Create token file for auth
        from tileharvester.strava_client import _save_tokens

        _save_tokens({"access_token": "test", "refresh_token": "ref", "expires_at": 2_000_000_000})

        # Add processed activity
        temp_db.execute(
            """
            INSERT INTO activities (id, start_utc, start_local, timezone, sport_type, has_gps, status,
                new_squadrat_count, squadrat_count, squadratinho_count, new_squadratinho_count)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                700,
                "2025-03-01T08:00:00Z",
                "2025-03-01T08:00:00",
                "UTC",
                "Run",
                1,
                "processed",
                3,
                10,
                0,
                8,
            ),
        )
        temp_db.execute(
            "INSERT INTO activity_tiles (activity_id, tile_kind, tile_id, is_new) VALUES (?, 'squadrat', ?, 1)",
            (700, "14:9000:9000"),
        )
        temp_db.execute(
            "INSERT INTO activity_tiles (activity_id, tile_kind, tile_id, is_new) VALUES (?, 'squadratinho', ?, 1)",
            (700, "17:72000:72000"),
        )
        temp_db.commit()

        # Mock Strava API response for get_activity
        mock_response = mock_strava_api["get"].return_value
        mock_response.json.return_value = {
            "id": 700,
            "description": "A great run",
        }

        result = annotate_activity(700)
        assert result["status"] == "annotated"
        assert "TileHarvester" in result["line"]
        assert "Squadrats" in result["line"]
        assert "+3 new" in result["line"]

        # Verify DB updated
        row = temp_db.execute(
            "SELECT annotation_status, description_line FROM activities WHERE id = 700"
        ).fetchone()
        assert row["annotation_status"] == "done"


class TestStoreActivityTiles:
    """Test _store_activity_tiles with seeded data."""

    def test_stores_tiles_and_updates_activity(self, seeded_db, temp_settings, monkeypatch):
        import tileharvester.config as config_mod
        import tileharvester.sync as sync_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(sync_mod, "settings", temp_settings)

        # Process activity 3 (pending) from streams
        row = sync_mod._activity_row(3)
        points = [(52.0, 4.5), (52.001, 4.501), (52.002, 4.502)]
        result = _store_activity_tiles(row, points, "test_source")

        assert result["status"] == "processed"
        assert result["activity_id"] == 3
        assert result["source"] == "test_source"

        # Verify DB state
        tiles = seeded_db.execute(
            "SELECT COUNT(*) FROM activity_tiles WHERE activity_id = 3"
        ).fetchone()[0]
        assert tiles > 0

        updated_row = seeded_db.execute("SELECT * FROM activities WHERE id = 3").fetchone()
        assert updated_row["status"] == "processed"
