"""Tests for config.py: Settings model."""

import pytest

from tileharvester.config import DEFAULT_DATA_DIR, Settings


def test_kml_module_uses_isolated_settings(temp_settings):
    import tileharvester.kml_baseline as kml_baseline_mod

    assert kml_baseline_mod.settings is temp_settings


class TestSettingsDefaults:
    def test_default_data_dir(self):
        settings = Settings(strava_client_id="id", strava_client_secret="secret")
        assert settings.data_dir == DEFAULT_DATA_DIR

    def test_default_squadrat_zoom(self):
        settings = Settings(strava_client_id="id", strava_client_secret="secret")
        assert settings.squadrat_zoom == 14
        assert settings.squadratinho_zoom == 17

    def test_default_poll_interval(self):
        settings = Settings(strava_client_id="id", strava_client_secret="secret")
        assert settings.poll_interval_minutes == 5

    def test_default_lookback_days(self):
        settings = Settings(strava_client_id="id", strava_client_secret="secret")
        assert settings.sync_lookback_days == 7
        assert settings.sync_annotation_window_days == 1

    def test_default_refine_limit(self):
        settings = Settings(strava_client_id="id", strava_client_secret="secret")
        assert settings.refine_default_limit == 80

    def test_default_rate_limit_buffer(self):
        settings = Settings(strava_client_id="id", strava_client_secret="secret")
        assert settings.rate_limit_buffer == 10

    def test_default_ignored_sports(self):
        settings = Settings(strava_client_id="id", strava_client_secret="secret")
        assert settings.ignored_sports == {"VirtualRide", "VirtualRun"}

    def test_ignored_sports_parses_custom(self):
        settings = Settings(
            strava_client_id="id",
            strava_client_secret="secret",
            ignored_sport_types="Swim,Workout",
        )
        assert settings.ignored_sports == {"Swim", "Workout"}

    def test_ignored_sports_handles_empty(self):
        settings = Settings(
            strava_client_id="id",
            strava_client_secret="secret",
            ignored_sport_types="",
        )
        assert settings.ignored_sports == set()

    def test_default_stream_thresholds(self):
        settings = Settings(strava_client_id="id", strava_client_secret="secret")
        assert settings.stream_max_segment_meters == 300.0
        assert settings.stream_max_time_gap_seconds == 60
        assert settings.stream_max_speed_mps == 35.0
        assert settings.stream_gap_min_meters == 50.0
        assert settings.stream_max_points == 50_000

    def test_default_description_settings(self):
        settings = Settings(strava_client_id="id", strava_client_secret="secret")
        assert settings.description_prefix == "TileHarvester"
        assert settings.description_emoji == "\U0001f5fa\ufe0f"
        assert settings.squadrat_offset == 0
        assert settings.rewrite_existing_annotations is False


class TestSettingsPaths:
    def test_db_path(self, tmp_path):
        data_dir = tmp_path / "test_data"
        settings = Settings(
            data_dir=data_dir,
            strava_client_id="id",
            strava_client_secret="secret",
        )
        assert settings.db_path == data_dir / "tileharvester.db"

    def test_token_path(self, tmp_path):
        data_dir = tmp_path / "test_data"
        settings = Settings(
            data_dir=data_dir,
            strava_client_id="id",
            strava_client_secret="secret",
        )
        assert settings.token_path == data_dir / "strava_tokens.json"


class TestSettingsValidation:
    def test_validate_passes_with_credentials(self):
        settings = Settings(strava_client_id="id", strava_client_secret="secret")
        settings.validate_strava_credentials()

    def test_validate_raises_without_client_id(self):
        settings = Settings(strava_client_id="", strava_client_secret="secret")
        with pytest.raises(RuntimeError, match="not configured"):
            settings.validate_strava_credentials()

    def test_validate_raises_without_client_secret(self):
        settings = Settings(strava_client_id="id", strava_client_secret="")
        with pytest.raises(RuntimeError, match="not configured"):
            settings.validate_strava_credentials()

    def test_validate_raises_with_whitespace_only(self):
        settings = Settings(strava_client_id="   ", strava_client_secret="secret")
        with pytest.raises(RuntimeError, match="not configured"):
            settings.validate_strava_credentials()


class TestEnsureDirs:
    def test_ensures_data_dir_created(self, tmp_path):
        data_dir = tmp_path / "new_data_dir"
        settings = Settings(
            data_dir=data_dir,
            strava_client_id="id",
            strava_client_secret="secret",
        )
        assert not data_dir.exists()
        settings.ensure_dirs()
        assert data_dir.exists()

    def test_ensures_data_dir_noop_if_exists(self, tmp_path):
        data_dir = tmp_path / "existing"
        data_dir.mkdir()
        settings = Settings(
            data_dir=data_dir,
            strava_client_id="id",
            strava_client_secret="secret",
        )
        settings.ensure_dirs()  # Should not raise


class TestEnvPrefix:
    def test_env_prefix_is_th(self):
        settings = Settings(strava_client_id="id", strava_client_secret="secret")
        assert settings.model_config["env_prefix"] == "TH_"
