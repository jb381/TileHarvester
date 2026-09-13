"""Tests for cli.py: CLI commands via Typer CliRunner."""

from typer.testing import CliRunner

from tileharvester.cli import app

runner = CliRunner()


class TestVersion:
    def test_version_flag(self):
        result = runner.invoke(app, ["--version"])
        assert result.exit_code == 0
        assert "tileharvester" in result.stdout


class TestHelp:
    def test_no_args_shows_help(self):
        result = runner.invoke(app, [])
        assert "TileHarvester" in result.stdout
        assert "Commands" in result.stdout or result.exit_code != 0


class TestResetDb:
    def test_requires_force_flag(self):
        result = runner.invoke(app, ["reset-db"])
        assert result.exit_code == 1
        assert "force" in result.stdout.lower() or "destructive" in result.stdout.lower()

    def test_force_resets_db(self, temp_db, temp_settings, monkeypatch):
        import tileharvester.config as config_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)

        result = runner.invoke(app, ["reset-db", "--force"])
        assert result.exit_code == 0
        assert "reset" in result.stdout.lower()


class TestAuthRequired:
    """Commands that require authentication should fail gracefully."""

    def test_backfill_requires_auth(self, temp_db, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)
        # Ensure no token file in temp dir
        if temp_settings.token_path.exists():
            temp_settings.token_path.unlink()

        result = runner.invoke(app, ["backfill"])
        assert result.exit_code == 1
        assert "authenticated" in result.stdout.lower()

    def test_sync_requires_auth(self, temp_db, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)
        if temp_settings.token_path.exists():
            temp_settings.token_path.unlink()

        result = runner.invoke(app, ["sync", "--once"])
        assert result.exit_code == 1
        assert "authenticated" in result.stdout.lower()

    def test_retry_requires_auth(self, temp_db, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)
        if temp_settings.token_path.exists():
            temp_settings.token_path.unlink()

        result = runner.invoke(app, ["retry"])
        assert result.exit_code == 1
        assert "authenticated" in result.stdout.lower()

    def test_refine_requires_auth(self, temp_db, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)
        if temp_settings.token_path.exists():
            temp_settings.token_path.unlink()

        result = runner.invoke(app, ["refine"])
        assert result.exit_code == 1
        assert "authenticated" in result.stdout.lower()

    def test_validate_requires_auth(self, temp_db, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)
        if temp_settings.token_path.exists():
            temp_settings.token_path.unlink()

        result = runner.invoke(app, ["validate", "123"])
        assert result.exit_code == 1
        assert "authenticated" in result.stdout.lower()


class TestStatus:
    def test_status_output(self, seeded_db, temp_settings, monkeypatch):
        import tileharvester.config as config_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)

        result = runner.invoke(app, ["status"])
        assert result.exit_code == 0
        assert "Total activities" in result.stdout
        assert "Processed" in result.stdout
        assert "Squadrats" in result.stdout

    def test_status_warns_when_not_authenticated(self, temp_db, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)
        if temp_settings.token_path.exists():
            temp_settings.token_path.unlink()

        result = runner.invoke(app, ["status"])
        assert result.exit_code == 0
        assert "Not authenticated" in result.stdout


class TestStats:
    def test_stats_output(self, seeded_db, temp_settings, monkeypatch):
        import tileharvester.config as config_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)

        result = runner.invoke(app, ["stats"])
        assert result.exit_code == 0
        assert "Squadrats" in result.stdout


class TestHealth:
    def test_health_db_ok(self, temp_db, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)

        # Create a token file so auth check passes
        from tileharvester.strava_client import _save_tokens

        _save_tokens({"access_token": "test", "refresh_token": "ref", "expires_at": 2_000_000_000})

        result = runner.invoke(app, ["health", "--no-check-rate-limit"])
        assert result.exit_code == 0
        assert "Database" in result.stdout
        assert "OK" in result.stdout

    def test_health_reports_auth_missing(self, temp_db, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)
        if temp_settings.token_path.exists():
            temp_settings.token_path.unlink()

        result = runner.invoke(app, ["health", "--no-check-rate-limit"])
        assert "NOT AUTHENTICATED" in result.stdout


class TestRecompute:
    def test_recompute_novelty(self, temp_db, temp_settings, monkeypatch):
        import tileharvester.config as config_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)

        result = runner.invoke(app, ["recompute-novelty"])
        assert result.exit_code == 0
        assert "Rebuilt" in result.stdout

    def test_recompute(self, temp_db, temp_settings, monkeypatch):
        import tileharvester.config as config_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)

        result = runner.invoke(app, ["recompute"])
        assert result.exit_code == 0


class TestService:
    def test_service_print(self):
        result = runner.invoke(app, ["service", "print"])
        assert result.exit_code == 0

    def test_service_invalid_action(self):
        result = runner.invoke(app, ["service", "invalid"])
        assert result.exit_code == 1
