"""Exercise recovery and command flows with isolated storage and mocked Strava."""

from datetime import datetime, timezone

from typer.testing import CliRunner

import tileharvester.annotate as annotate
import tileharvester.cli as cli
import tileharvester.refine as refine
import tileharvester.sync as sync
from tests.test_kml_baseline import _insert_activity, _tile_center, _write_fixture
from tests.test_review_regressions import store
from tileharvester.db import get_db
from tileharvester.kml_baseline import effective_tile_count, import_baseline


def test_refine_keeps_failed_and_successful_history(isolated_db, monkeypatch):
    _insert_activity(1, "2026-08-01T10:00:00Z")
    _insert_activity(2, "2026-08-02T10:00:00Z")
    store(1, _tile_center(8000, 6000, 14))
    store(2, _tile_center(8010, 6000, 14))

    def streams(aid, **_kwargs):
        if aid == 2:
            raise RuntimeError("temporary outage")
        point = _tile_center(8020, 6000, 14)
        return {"latlng": {"data": [point, point]}}

    monkeypatch.setattr(sync, "get_activity_streams", streams)
    result = refine.refine_streams(limit=0)
    assert result == {"refined": 1, "failed": 1, "selected": 2, "rebuilt": 2, "splits": 0}
    assert effective_tile_count("squadrat") == 2
    with get_db() as conn:
        assert [
            tuple(r) for r in conn.execute("SELECT status, tile_source FROM activities ORDER BY id")
        ] == [("processed", "streams_clean"), ("processed", "summary_polyline")]
    # Default mode retries only the activity that still needs refinement.
    assert refine.refine_streams()["selected"] == 1


def test_sync_retries_annotation_failure_without_reprocessing(isolated_db, monkeypatch):
    _insert_activity(1, datetime.now(timezone.utc).isoformat())
    store(1, _tile_center(8000, 6000, 14))
    monkeypatch.setattr(sync, "_fetch_recent_activities", lambda _after: [])

    def fail(_aid):
        raise RuntimeError("temporary outage")

    monkeypatch.setattr(annotate, "get_activity", fail)
    assert sync.sync_once()["failed"] == 1
    monkeypatch.setattr(annotate, "get_activity", lambda _aid: {"description": "Morning ride"})
    writes = []
    monkeypatch.setattr(
        annotate,
        "update_activity_description",
        lambda aid, description: writes.append((aid, description)),
    )
    result = sync.sync_once()
    assert result["failed"] == 0
    assert result["processed"] == 0
    assert result["annotated"] == 1
    assert len(writes) == 1
    assert sync.sync_once()["annotated"] == 0


def test_cli_validate_uses_baseline_for_unstored_activity(isolated_db, tmp_path, monkeypatch):
    import_baseline(_write_fixture(tmp_path / "tiles.kml"), as_of="2026-08-01T00:00:00Z")
    point = _tile_center(8000, 6000, 14)
    monkeypatch.setattr(cli, "is_authenticated", lambda: True)
    monkeypatch.setattr(
        cli,
        "get_activity",
        lambda _aid: {
            "start_date": "2026-08-02T10:00:00Z",
            "start_date_local": "2026-08-02T12:00:00",
        },
    )
    monkeypatch.setattr(
        cli, "get_activity_streams", lambda *_args, **_kwargs: {"latlng": {"data": [point, point]}}
    )
    result = CliRunner().invoke(cli.app, ["validate", "42"])
    assert result.exit_code == 0, result.output
    assert "Unique Squadrats before:           3" in result.output
    assert "New Squadrats from this activity:  0" in result.output


def test_cli_auth_extracts_code_without_opening_browser(isolated_db, monkeypatch):
    monkeypatch.setattr(cli, "is_authenticated", lambda: False)
    monkeypatch.setattr(cli, "build_auth_url", lambda: "https://www.strava.com/oauth/authorize")
    codes = []
    monkeypatch.setattr(
        cli, "exchange_code", lambda code: codes.append(code) or {"athlete": {"id": 123}}
    )
    result = CliRunner().invoke(
        cli.app,
        ["auth", "--no-open-browser"],
        input="http://localhost:8000/callback?code=test-code&scope=activity:write\n",
    )
    assert result.exit_code == 0, result.output
    assert codes == ["test-code"]
    assert "Athlete ID: 123" in result.output


def test_cursor_does_not_advance_when_fetch_fails(isolated_db, monkeypatch):
    import pytest

    from tileharvester.db import get_setting, set_setting

    cursor = "2026-07-01T10:00:00Z"
    set_setting("last_successful_sync_at", cursor)

    def fail(_after):
        raise RuntimeError("fetch failure")

    monkeypatch.setattr(sync, "_fetch_recent_activities", fail)
    with pytest.raises(RuntimeError, match="fetch failure"):
        sync.sync_once()
    assert get_setting("last_successful_sync_at") == cursor
