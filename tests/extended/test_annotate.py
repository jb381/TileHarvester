"""Tests for annotate.py."""

from unittest.mock import patch

from tileharvester.annotate import annotate_activity


class TestAnnotateActivity:
    def test_returns_not_processed_for_pending(self, seeded_db, temp_settings, monkeypatch):
        import tileharvester.annotate as annotate_mod
        import tileharvester.config as config_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(annotate_mod, "settings", temp_settings)

        result = annotate_activity(3)  # activity 3 is 'pending'
        assert result["status"] == "not_processed"

    def test_raises_for_nonexistent_activity(self, seeded_db, temp_settings, monkeypatch):
        import tileharvester.annotate as annotate_mod
        import tileharvester.config as config_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(annotate_mod, "settings", temp_settings)

        with patch("tileharvester.annotate.get_activity") as _:
            import pytest

            with pytest.raises(ValueError, match="not found"):
                annotate_activity(999)

    def test_annotates_successfully(self, seeded_db, temp_settings, mock_strava_api, monkeypatch):
        import tileharvester.annotate as annotate_mod
        import tileharvester.config as config_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(annotate_mod, "settings", temp_settings)

        # Create a token file so _refresh_if_needed() doesn't fail
        from tileharvester.strava_client import _save_tokens

        _save_tokens({"access_token": "test", "refresh_token": "ref", "expires_at": 2_000_000_000})

        # Mock get_activity to return an existing description
        mock_response = mock_strava_api["get"].return_value
        mock_response.json.return_value = {
            "id": 1,
            "description": "Morning run",
        }

        result = annotate_activity(1)
        assert result["status"] == "annotated"
        assert "Squadrats" in result["line"]
        assert "TileHarvester" in result["line"]

        # Verify annotation_status updated in DB
        row = seeded_db.execute(
            "SELECT annotation_status, description_line FROM activities WHERE id = 1"
        ).fetchone()
        assert row["annotation_status"] == "done"

    def test_handles_annotation_failure(
        self, seeded_db, temp_settings, mock_strava_api, monkeypatch
    ):
        import tileharvester.annotate as annotate_mod
        import tileharvester.config as config_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(annotate_mod, "settings", temp_settings)

        # Create a token file for _refresh_if_needed
        from tileharvester.strava_client import _save_tokens

        _save_tokens({"access_token": "test", "refresh_token": "ref", "expires_at": 2_000_000_000})

        # Simulate an API error
        import httpx

        mock_strava_api["get"].return_value.raise_for_status.side_effect = httpx.HTTPStatusError(
            "500",
            request=mock_strava_api["get"].return_value.request,
            response=mock_strava_api["get"].return_value,
        )

        result = annotate_activity(1)
        assert result["status"] == "annotation_failed"

    def test_skips_when_description_unchanged(
        self, seeded_db, temp_settings, mock_strava_api, monkeypatch
    ):
        import tileharvester.annotate as annotate_mod
        import tileharvester.config as config_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)
        monkeypatch.setattr(annotate_mod, "settings", temp_settings)

        # Create a token file for _refresh_if_needed
        from tileharvester.strava_client import _save_tokens

        _save_tokens({"access_token": "test", "refresh_token": "ref", "expires_at": 2_000_000_000})

        # Return a description that already has the exact TileHarvester line
        # so update_description_line returns the same text
        from tileharvester.config import settings
        from tileharvester.sync import compute_period_totals, compute_total_unique_squadrats_through

        # Build what the line would be
        start_local = "2025-01-01T10:00:00"
        month_total, week_total = compute_period_totals(start_local)
        total = compute_total_unique_squadrats_through(1, start_local) + settings.squadrat_offset
        row = seeded_db.execute("SELECT * FROM activities WHERE id = 1").fetchone()
        line = (
            f"{temp_settings.description_emoji} {temp_settings.description_prefix}: {total:,} Squadrats · "
            f"+{row['new_squadrat_count']} new · +{month_total}/mo · +{week_total}/wk"
        )

        mock_response = mock_strava_api["get"].return_value
        mock_response.json.return_value = {
            "id": 1,
            "description": line,
        }

        result = annotate_activity(1)
        assert result["status"] == "annotated"
