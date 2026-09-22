"""Reproductions for all findings in Copilot's PR #9 review."""

from datetime import datetime, timezone

import pytest
from typer.testing import CliRunner

import tileharvester.cli as cli
import tileharvester.recompute as recompute
import tileharvester.sync as sync
from tests.test_kml_baseline import _insert_activity, _tile_center, _write_fixture
from tests.test_review_regressions import store
from tileharvester.db import get_db
from tileharvester.descriptions import remove_description_line, update_description_line
from tileharvester.kml_baseline import effective_tile_count, parse_squadrats_kml


@pytest.mark.parametrize(
    "payload",
    [
        None,
        [],
        {"latlng": None},
        {"latlng": {"data": None}},
        {"latlng": {"data": [{"lat": 52, "lon": 5}]}},
        {"time": None},
        {"latlng": {"data": [[52, 5], [52, 5]]}, "time": {"data": [None, {}]}},
    ],
)
def test_malformed_stream_is_failed_without_aborting(isolated_db, monkeypatch, payload):
    _insert_activity(1, "2026-08-01T10:00:00Z")
    monkeypatch.setattr(sync, "get_activity_streams", lambda *_args, **_kwargs: payload)
    assert sync.compute_activity_tiles(1)["status"] == "failed"
    with get_db() as conn:
        assert conn.execute("SELECT status FROM activities").fetchone()[0] == "failed"


def test_cli_handles_malformed_stream(isolated_db, monkeypatch):
    monkeypatch.setattr(cli, "is_authenticated", lambda: True)
    monkeypatch.setattr(
        cli, "get_activity", lambda _aid: {"start_date_local": "2026-08-01T10:00:00"}
    )
    monkeypatch.setattr(cli, "get_activity_streams", lambda *_args, **_kwargs: {"latlng": None})
    result = CliRunner().invoke(cli.app, ["validate", "123"])
    assert result.exit_code == 1
    assert "Invalid GPS stream:" in result.output


@pytest.mark.parametrize("marker", ["—", "-", "*", "...", ">"])
def test_punctuation_prefixed_prose_survives(marker):
    description = f"{marker} TileHarvester: it helped me explore."
    updated = update_description_line(description, "🗺️ TileHarvester: stats")
    assert updated.startswith(description + "\n\n")
    assert remove_description_line(description) == description


def test_kml_rejects_entities_before_expansion(tmp_path):
    path = _write_fixture(tmp_path / "entity.kml")
    xml = (
        path.read_text()
        .replace("<kml xmlns=", '<!DOCTYPE kml [<!ENTITY layer "squadrats">]><kml xmlns=')
        .replace("<name>squadrats</name>", "<name>&layer;</name>")
    )
    path.write_text(xml)
    with pytest.raises(ValueError, match="Unsafe KML XML"):
        parse_squadrats_kml(path)


def test_latest_activity_uses_utc_and_ignores_invalid_values(isolated_db):
    _insert_activity(1, "2026-08-01T12:00:00+03:00")
    _insert_activity(2, "2026-08-01T10:00:00Z")
    _insert_activity(3, "invalid")
    assert sync._latest_stored_activity_utc() == datetime(2026, 8, 1, 10, tzinfo=timezone.utc)


def test_historical_processed_count_matches_utc_order(isolated_db):
    _insert_activity(1, "2026-08-01T10:00:00Z", status="processed")
    _insert_activity(2, "2026-08-01T11:00:00Z")
    with get_db() as conn:
        conn.execute("UPDATE activities SET start_local = '2026-08-01T20:00:00' WHERE id = 1")
        conn.commit()
    result = sync.compute_historical_novelty(2, "2026-08-01T11:00:00", set(), set())
    assert result["processed_before"] == 1


def test_historical_novelty_uses_local_order_without_candidate_utc(isolated_db):
    point = _tile_center(8000, 6000, 14)
    tile_id = "14:8000:6000"
    _insert_activity(1, "2026-08-01T09:00:00Z", status="processed")
    store(1, point)
    with get_db() as conn:
        conn.execute("UPDATE activities SET start_local = '2026-08-01T12:00:00' WHERE id = 1")
        conn.commit()

    result = sync.compute_historical_novelty(
        2,
        "2026-08-01T11:00:00",
        {tile_id},
        set(),
    )

    assert result["processed_before"] == 0
    assert result["seen_squadrats"] == 0
    assert result["new_squadrats"] == 1


def test_eligibility_and_history_rollback_together(isolated_db, monkeypatch):
    _insert_activity(1, "2026-08-01T10:00:00Z")
    store(1, _tile_center(8000, 6000, 14))
    with get_db() as conn:
        conn.execute("UPDATE activities SET sport_type = 'VirtualRide' WHERE id = 1")
        conn.commit()

    def fail(conn, *_args, **_kwargs):
        conn.execute("DELETE FROM global_tiles")
        raise RuntimeError("interrupted rebuild")

    monkeypatch.setattr(recompute, "rebuild_tile_history", fail)
    with pytest.raises(RuntimeError):
        recompute.recompute_all()
    with get_db() as conn:
        assert conn.execute("SELECT status FROM activities").fetchone()[0] == "processed"
        assert conn.execute("SELECT SUM(is_new) FROM activity_tiles").fetchone()[0] == 2
    assert effective_tile_count("squadrat") == 1


def test_changed_route_is_retried_while_old_tiles_remain_valid(isolated_db, monkeypatch):
    import polyline

    _insert_activity(1, "2026-08-01T10:00:00Z")
    point = _tile_center(8000, 6000, 14)
    replacement = _tile_center(8010, 6000, 14)
    store(1, point)
    with get_db() as conn:
        conn.execute(
            "UPDATE activities SET summary_polyline = ? WHERE id = 1",
            (polyline.encode([point, point]),),
        )
        conn.commit()
    summary = {
        "id": 1,
        "start_date": "2026-08-01T10:00:00Z",
        "start_date_local": "2026-08-01T10:00:00",
        "sport_type": "Ride",
        "map": {"summary_polyline": polyline.encode([replacement, replacement])},
    }
    monkeypatch.setattr(sync, "_fetch_recent_activities", lambda _after: [summary])
    calls = []

    def fail(*_args, **_kwargs):
        calls.append("failed")
        raise RuntimeError("temporary outage")

    monkeypatch.setattr(sync, "get_activity_streams", fail)
    assert sync.sync_once()["failed"] == 1
    assert calls == ["failed"]
    with get_db() as conn:
        assert conn.execute("SELECT status FROM activities").fetchone()[0] == "processed"
    assert effective_tile_count("squadrat") == 1
    monkeypatch.setattr(
        sync,
        "get_activity_streams",
        lambda *_args, **_kwargs: {"latlng": {"data": [replacement, replacement]}},
    )
    assert sync.sync_once()["processed"] == 1
    with get_db() as conn:
        assert (
            conn.execute(
                "SELECT tile_id FROM activity_tiles WHERE tile_kind='squadrat'"
            ).fetchone()[0]
            == "14:8010:6000"
        )
    assert sync.sync_once()["processed"] == 0


def test_later_recompute_failure_leaves_eligibility_consistent(isolated_db, monkeypatch):
    import polyline

    for aid in (1, 2):
        _insert_activity(aid, f"2026-08-0{aid}T10:00:00Z")
        store(aid, _tile_center(8000 + aid, 6000, 14))
    with get_db() as conn:
        conn.execute("UPDATE activities SET sport_type = 'VirtualRide' WHERE id = 1")
        conn.execute(
            "UPDATE activities SET summary_polyline = ? WHERE id = 2",
            (polyline.encode([(52, 5), (52.0001, 5)]),),
        )
        conn.commit()

    def fail(*_args, **_kwargs):
        raise RuntimeError("interrupted route recomputation")

    monkeypatch.setattr(recompute, "_store_activity_tiles", fail)
    with pytest.raises(RuntimeError):
        recompute.recompute_all()
    with get_db() as conn:
        assert (
            conn.execute("SELECT status FROM activities WHERE id = 1").fetchone()[0]
            == "skipped_ignored_sport"
        )
        assert (
            conn.execute("SELECT SUM(is_new) FROM activity_tiles WHERE activity_id = 1").fetchone()[
                0
            ]
            == 0
        )
    assert effective_tile_count("squadrat") == 1


def test_refresh_flag_migration_preserves_processed_history(isolated_db):
    from tileharvester.db import MIGRATIONS, migrate

    _insert_activity(1, "2026-08-01T10:00:00Z", status="processed")
    with get_db() as conn:
        conn.execute("ALTER TABLE activities DROP COLUMN stream_refresh_pending")
        conn.execute("DELETE FROM schema_version WHERE version = ?", (len(MIGRATIONS),))
        conn.commit()
    migrate()
    with get_db() as conn:
        assert tuple(
            conn.execute("SELECT status, stream_refresh_pending FROM activities").fetchone()
        ) == ("processed", 0)


def test_kml_rejects_utf16_dtd(tmp_path):
    path = tmp_path / "utf16.kml"
    path.write_bytes(
        '<?xml version="1.0" encoding="UTF-16"?><!DOCTYPE kml [<!ENTITY x "small">]><kml>&x;</kml>'.encode(
            "utf-16"
        )
    )
    with pytest.raises(ValueError, match="Unsafe KML XML"):
        parse_squadrats_kml(path)
