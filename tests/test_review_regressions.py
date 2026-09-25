"""Regression cases found during the whole-project review."""

import pytest

import tileharvester.sync as sync
from tests.test_kml_baseline import _insert_activity, _tile_center, _write_fixture
from tileharvester.db import get_db
from tileharvester.kml_baseline import effective_tile_count, import_baseline
from tileharvester.recompute import recompute_novelty_from_stored_tiles


def store(aid, point):
    with get_db() as conn:
        row = conn.execute("SELECT * FROM activities WHERE id = ?", (aid,)).fetchone()
    return sync._store_activity_tiles(row, [point], "summary_polyline")


def counts():
    with get_db() as conn:
        return [
            tuple(r)
            for r in conn.execute("SELECT id, new_squadrat_count FROM activities ORDER BY id")
        ]


def test_late_history_repairs_later_novelty(isolated_db):
    del isolated_db
    _insert_activity(1, "2026-08-01T10:00:00Z")
    _insert_activity(2, "2026-08-02T10:00:00Z")
    point = _tile_center(8000, 6000, 14)
    store(2, point)
    store(1, point)
    assert counts() == [(1, 1), (2, 0)]
    assert sync.compute_period_totals("2026-08-02T10:00:00") == (1, 1)


def test_replacing_route_removes_obsolete_global_tiles(isolated_db):
    del isolated_db
    _insert_activity(1, "2026-08-01T10:00:00Z")
    store(1, _tile_center(8000, 6000, 14))
    store(1, _tile_center(8010, 6000, 14))
    assert effective_tile_count("squadrat") == 1


@pytest.mark.parametrize("empty", [False, True])
def test_failed_refinement_keeps_valid_history(isolated_db, monkeypatch, empty):
    del isolated_db
    _insert_activity(1, "2026-08-01T10:00:00Z")
    store(1, _tile_center(8000, 6000, 14))

    def streams(*_args, **_kwargs):
        if empty:
            return {}
        raise RuntimeError("temporary network outage")

    monkeypatch.setattr(sync, "get_activity_streams", streams)
    result = sync.compute_activity_tiles(1)
    assert result["status"] == "failed"
    with get_db() as conn:
        row = conn.execute("SELECT * FROM activities WHERE id = 1").fetchone()
    assert row["status"] == "processed"
    assert row["last_error"]
    recompute_novelty_from_stored_tiles()
    assert effective_tile_count("squadrat") == 1


@pytest.mark.parametrize("baseline", [False, True])
def test_same_instant_uses_id_tiebreaker(isolated_db, tmp_path, baseline):
    del isolated_db
    if baseline:
        import_baseline(_write_fixture(tmp_path / "tiles.kml"), as_of="2026-08-01T00:00:00Z")
    _insert_activity(1, "2026-08-02T10:00:00Z")
    _insert_activity(2, "2026-08-02T12:00:00+02:00")
    point = _tile_center(8010, 6000, 14)
    store(2, point)
    store(1, point)
    recompute_novelty_from_stored_tiles()
    assert counts() == [(1, 1), (2, 0)]
    assert (
        sync.compute_historical_novelty(2, "2026-08-02T12:00:00+02:00", {"14:8010:6000"}, set())[
            "new_squadrats"
        ]
        == 0
    )


@pytest.mark.parametrize(
    "start,end", [((1.5, 1.5), (1.0, 2.0)), ((1.5, 1.5), (2.0, 1.0)), ((1.0, 2.0), (1.5, 1.5))]
)
def test_grid_endpoint_traversal_terminates(start, end):
    import subprocess
    import sys

    # Keep the regression bounded even if the traversal bug is reintroduced.
    code = f"from tileharvester.tile_engine import SquadratsEngine; print(SquadratsEngine()._tiles_for_segment({start!r}, {end!r}, 14))"
    result = subprocess.run(
        [sys.executable, "-c", code], timeout=3, capture_output=True, text=True, check=True
    )
    assert "14:1:1" in result.stdout


@pytest.mark.parametrize(
    "headers, expected",
    [
        ({"X-RateLimit-Limit": "200,2000", "X-RateLimit-Usage": "195,300"}, 801),
        ({"X-ReadRateLimit-Limit": "100,1000", "X-ReadRateLimit-Usage": "95,300"}, 801),
        ({"X-RateLimit-Limit": "200,2000", "X-RateLimit-Usage": "10,1995"}, 86301),
        ({"X-RateLimit-Limit": "200,2000", "X-RateLimit-Usage": "10,300"}, None),
    ],
)
def test_rate_limits_use_correct_window(monkeypatch, headers, expected):
    import httpx

    import tileharvester.strava_client as client

    sleeps = []
    monkeypatch.setattr(client.time, "time", lambda: 100)
    monkeypatch.setattr(client.time, "sleep", sleeps.append)
    client._rate_limit_sleep(httpx.Response(200, headers=headers))
    assert sleeps == ([] if expected is None else [expected])


def test_token_save_is_private_and_atomic(isolated_db, monkeypatch):
    del isolated_db
    import tileharvester.strava_client as client

    client._save_tokens({"refresh_token": "old"})
    path = client._token_file()
    assert path.stat().st_mode & 0o777 == 0o600

    def fail_replace(*_args):
        raise OSError("disk failure")

    monkeypatch.setattr(client.os, "replace", fail_replace)
    with pytest.raises(OSError):
        client._save_tokens({"refresh_token": "new"})
    assert client._load_tokens() == {"refresh_token": "old"}
    assert not list(path.parent.glob(".strava-tokens-*"))


def test_annotation_preserves_prose_mentioning_tool():
    from tileharvester.descriptions import update_description_line

    description = "I tried TileHarvester: it helped me explore."
    assert (
        update_description_line(description, "🗺️ TileHarvester: stats")
        == description + "\n\n🗺️ TileHarvester: stats"
    )


def test_kml_import_rolls_back_if_history_rebuild_fails(isolated_db, tmp_path, monkeypatch):
    del isolated_db
    import tileharvester.kml_baseline as kml

    def fail(conn):
        conn.execute("DELETE FROM global_tiles")
        raise RuntimeError("rebuild failure")

    monkeypatch.setattr(kml, "rebuild_tile_history", fail)
    with pytest.raises(RuntimeError):
        import_baseline(_write_fixture(tmp_path / "tiles.kml"), as_of="2026-08-01T00:00:00Z")
    with get_db() as conn:
        assert conn.execute("SELECT COUNT(*) FROM baseline_imports").fetchone()[0] == 0
        assert conn.execute("SELECT COUNT(*) FROM baseline_tiles").fetchone()[0] == 0


def test_validate_unstored_activity_includes_baseline(isolated_db, tmp_path):
    del isolated_db
    import_baseline(_write_fixture(tmp_path / "tiles.kml"), as_of="2026-08-01T00:00:00Z")
    result = sync.compute_historical_novelty(
        99, "2026-08-02T10:00:00", {"14:8000:6000"}, set(), start_utc="2026-08-02T10:00:00Z"
    )
    assert result["total_squadrats_before"] == 3
    assert result["new_squadrats"] == 0
    with get_db() as conn:
        assert conn.execute("SELECT COUNT(*) FROM activities").fetchone()[0] == 0


def test_kml_rejects_incomplete_snapshot(isolated_db, tmp_path):
    del isolated_db
    from tileharvester.kml_baseline import parse_squadrats_kml

    path = _write_fixture(tmp_path / "tiles.kml")
    path.write_text(path.read_text().replace("<name>squadratinhos</name>", "<name>other</name>"))
    with pytest.raises(ValueError, match="both"):
        parse_squadrats_kml(path)


def test_kml_rejects_excessive_raster_before_allocating(monkeypatch):
    import tileharvester.kml_baseline as kml
    from tests.test_kml_baseline import _ring

    monkeypatch.setattr(kml, "MAX_RASTER_TILES", 10)
    with pytest.raises(ValueError, match="tile limit"):
        kml._tiles_inside_ring(_ring(100, 100, 120, 120, 17), 17)


def test_kml_rejects_incompatible_zoom(isolated_db, tmp_path, monkeypatch):
    del isolated_db
    from tileharvester.config import settings

    monkeypatch.setattr(settings, "squadrat_zoom", 13)
    with pytest.raises(ValueError, match="zoom"):
        import_baseline(_write_fixture(tmp_path / "tiles.kml"))


def test_sport_changes_reconcile_history_and_reactivate_without_summary(isolated_db, monkeypatch):
    del isolated_db
    _insert_activity(1, "2026-08-01T10:00:00Z")
    _insert_activity(2, "2026-08-02T10:00:00Z")
    point = _tile_center(8000, 6000, 14)
    store(1, point)
    store(2, point)
    activity = {
        "id": 1,
        "start_date": "2026-08-01T10:00:00Z",
        "start_date_local": "2026-08-01T10:00:00",
        "sport_type": "VirtualRide",
    }
    monkeypatch.setattr(sync, "get_activities", lambda **_kwargs: [activity])
    sync.fetch_and_store_summaries()
    assert counts() == [(1, 0), (2, 1)]
    activity["sport_type"] = "Ride"
    sync.fetch_and_store_summaries()
    assert counts() == [(1, 1), (2, 0)]


def test_sync_retries_transient_failures_on_next_poll(isolated_db, monkeypatch):
    del isolated_db
    from datetime import datetime, timezone

    _insert_activity(1, datetime.now(timezone.utc).isoformat(), status="failed")
    monkeypatch.setattr(sync, "_fetch_recent_activities", lambda _after: [])
    monkeypatch.setattr(
        sync,
        "get_activity_streams",
        lambda *_args, **_kwargs: {"latlng": {"data": [[52.0, 5.0], [52.0001, 5.0001]]}},
    )
    annotations = []

    def annotate(aid):
        annotations.append(aid)
        return {"status": "annotated", "line": "test"}

    monkeypatch.setattr(sync, "annotate_activity", annotate)
    assert sync.sync_once()["processed"] == 1
    assert annotations == [1]


def test_oversized_stream_is_failed_without_partial_tiles(isolated_db, monkeypatch):
    del isolated_db
    from tileharvester.config import settings

    _insert_activity(1, "2026-08-01T10:00:00Z")
    monkeypatch.setattr(settings, "stream_max_points", 2)
    monkeypatch.setattr(
        sync,
        "get_activity_streams",
        lambda *_args, **_kwargs: {"latlng": {"data": [[52, 5], [52.0001, 5], [52.0002, 5]]}},
    )
    result = sync.compute_activity_tiles(1)
    assert result["status"] == "failed"
    assert "TH_STREAM_MAX_POINTS" in result["error"]
    with get_db() as conn:
        assert conn.execute("SELECT COUNT(*) FROM activity_tiles").fetchone()[0] == 0


def test_recompute_requeues_newly_allowed_sport_without_stored_tiles(isolated_db):
    del isolated_db
    import tileharvester.recompute as recompute

    _insert_activity(1, "2026-08-01T10:00:00Z", status="skipped_ignored_sport")
    with get_db() as conn:
        conn.execute("UPDATE activities SET sport_type = 'Ride' WHERE id = 1")
        conn.commit()
    recompute.recompute_all()
    with get_db() as conn:
        assert conn.execute("SELECT status FROM activities WHERE id = 1").fetchone()[0] == "pending"


def test_failed_sync_has_nonzero_cli_exit(isolated_db, monkeypatch):
    del isolated_db
    from typer.testing import CliRunner

    import tileharvester.cli as cli

    monkeypatch.setattr(cli, "is_authenticated", lambda: True)
    monkeypatch.setattr(
        cli, "sync_once", lambda: {"new_activities": 1, "processed": 0, "annotated": 0, "failed": 1}
    )
    result = CliRunner().invoke(cli.app, ["sync", "--once"])
    assert result.exit_code == 1
    assert "1 processing or annotation failures" in result.output


@pytest.mark.parametrize("baseline", [False, True])
def test_incremental_history_matches_complete_rebuild(isolated_db, tmp_path, baseline):
    del isolated_db
    if baseline:
        import_baseline(_write_fixture(tmp_path / "tiles.kml"), as_of="2026-08-01T00:00:00Z")
    for aid in range(1, 7):
        _insert_activity(aid, f"2026-08-0{aid}T10:00:00Z")
    for aid in (6, 2, 4, 1, 5, 3):
        store(aid, _tile_center(8000 + aid % 3, 6000, 14))
    store(1, _tile_center(8010, 6000, 14))
    before = counts()
    total = effective_tile_count("squadrat")
    recompute_novelty_from_stored_tiles()
    assert counts() == before
    assert effective_tile_count("squadrat") == total
    assert before == (
        [(1, 1), (2, 1), (3, 0), (4, 0), (5, 0), (6, 0)]
        if baseline
        else [(1, 1), (2, 1), (3, 1), (4, 1), (5, 0), (6, 0)]
    )


def test_invalid_summary_route_fails_only_that_activity(isolated_db):
    import polyline

    _insert_activity(1, "2026-08-01T10:00:00Z")
    with get_db() as conn:
        conn.execute(
            "UPDATE activities SET summary_polyline = ? WHERE id = 1",
            (polyline.encode([(91, 5), (92, 5)]),),
        )
        conn.commit()
    result = sync.compute_activity_tiles_from_summary(1)
    assert result["status"] == "failed"
    assert "Invalid route" in result["error"]
