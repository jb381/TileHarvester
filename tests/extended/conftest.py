"""Shared test fixtures for TileHarvester tests."""

import sqlite3
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from tileharvester.config import Settings


def _make_temp_settings(tmp_path: Path) -> Settings:
    """Create a Settings instance pointed at a temp directory."""
    data_dir = tmp_path / "data"
    return Settings(
        data_dir=data_dir,
        strava_client_id="test_client_id",
        strava_client_secret="test_client_secret",
    )


@pytest.fixture
def temp_settings(tmp_path: Path) -> Settings:
    """Settings pointed at a temporary data directory with valid credentials."""
    return _make_temp_settings(tmp_path)


@pytest.fixture(autouse=True)
def isolate_settings(temp_settings: Settings, monkeypatch) -> Settings:
    """Patch all module-level settings imports to use test-only paths.

    Several modules import ``settings`` at module scope. Patching only
    ``tileharvester.config.settings`` is not enough for those modules and can
    make tests accidentally read the user's real DB/token files. This fixture is
    autouse so every test starts isolated even when it does not request a DB.
    """
    import tileharvester.annotate as annotate_mod
    import tileharvester.cli as cli_mod
    import tileharvester.config as config_mod
    import tileharvester.db as db_mod
    import tileharvester.descriptions as descriptions_mod
    import tileharvester.kml_baseline as kml_baseline_mod
    import tileharvester.recompute as recompute_mod
    import tileharvester.refine as refine_mod
    import tileharvester.strava_client as strava_client_mod
    import tileharvester.sync as sync_mod
    import tileharvester.systemd as systemd_mod
    import tileharvester.tile_engine as tile_engine_mod

    for module in (
        annotate_mod,
        cli_mod,
        config_mod,
        db_mod,
        descriptions_mod,
        kml_baseline_mod,
        recompute_mod,
        refine_mod,
        strava_client_mod,
        sync_mod,
        systemd_mod,
        tile_engine_mod,
    ):
        monkeypatch.setattr(module, "settings", temp_settings)

    return temp_settings


@pytest.fixture
def temp_db(temp_settings: Settings) -> sqlite3.Connection:
    """SQLite database in a temp directory with all migrations applied.

    The autouse isolate_settings fixture ensures get_db() across the codebase
    resolves to this test-only database.
    """
    import tileharvester.db as db_mod

    db_mod.migrate()
    conn = db_mod.get_db()

    yield conn
    conn.close()


@pytest.fixture
def seeded_db(temp_db: sqlite3.Connection, temp_settings) -> sqlite3.Connection:
    """Database with a few seeded activities and tiles."""
    activities = [
        # id, start_utc, start_local, timezone, sport_type, has_gps, status, summary_polyline, new_squadrat_count, squadrat_count
        (
            1,
            "2025-01-01T10:00:00Z",
            "2025-01-01T10:00:00",
            "UTC",
            "Run",
            1,
            "processed",
            "poly1",
            5,
            10,
        ),
        (
            2,
            "2025-01-02T11:00:00Z",
            "2025-01-02T11:00:00",
            "UTC",
            "Ride",
            1,
            "processed",
            "poly2",
            3,
            8,
        ),
        (
            3,
            "2025-01-03T12:00:00Z",
            "2025-01-03T12:00:00",
            "UTC",
            "Run",
            1,
            "pending",
            "poly3",
            0,
            0,
        ),
        (
            4,
            "2025-01-04T13:00:00Z",
            "2025-01-04T13:00:00",
            "UTC",
            "VirtualRide",
            0,
            "skipped_ignored_sport",
            None,
            0,
            0,
        ),
        (
            5,
            "2025-01-05T14:00:00Z",
            "2025-01-05T14:00:00",
            "UTC",
            "Run",
            0,
            "skipped_no_gps",
            None,
            0,
            0,
        ),
        (
            6,
            "2025-01-06T15:00:00Z",
            "2025-01-06T15:00:00",
            "UTC",
            "Ride",
            1,
            "failed",
            "poly4",
            0,
            0,
        ),
    ]

    for a in activities:
        temp_db.execute(
            """
            INSERT INTO activities
            (id, start_utc, start_local, timezone, sport_type, has_gps, status, summary_polyline, new_squadrat_count, squadrat_count)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            a,
        )

    tiles = [
        # activity_id, tile_kind, tile_id, is_new
        (1, "squadrat", "14:8000:8000", 1),
        (1, "squadrat", "14:8001:8000", 1),
        (1, "squadrat", "14:8002:8000", 0),
        (1, "squadratinho", "17:64000:64000", 1),
        (2, "squadrat", "14:8010:8010", 1),
        (2, "squadrat", "14:8011:8010", 1),
        (2, "squadratinho", "17:64080:64080", 1),
    ]

    for t in tiles:
        temp_db.execute(
            "INSERT INTO activity_tiles (activity_id, tile_kind, tile_id, is_new) VALUES (?, ?, ?, ?)",
            t,
        )

    global_tiles = [
        ("squadrat", "14:8000:8000", 1, "2025-01-01T10:00:00"),
        ("squadrat", "14:8001:8000", 1, "2025-01-01T10:00:00"),
        ("squadrat", "14:8002:8000", 1, "2025-01-01T10:00:00"),
        ("squadrat", "14:8010:8010", 2, "2025-01-02T11:00:00"),
        ("squadrat", "14:8011:8010", 2, "2025-01-02T11:00:00"),
        ("squadratinho", "17:64000:64000", 1, "2025-01-01T10:00:00"),
        ("squadratinho", "17:64080:64080", 2, "2025-01-02T11:00:00"),
    ]

    for gt in global_tiles:
        temp_db.execute(
            "INSERT INTO global_tiles (tile_kind, tile_id, first_activity_id, first_seen_local) VALUES (?, ?, ?, ?)",
            gt,
        )

    temp_db.commit()
    return temp_db


@pytest.fixture
def mock_httpx():
    """Patch httpx.Client methods with a configurable mock.

    Returns a dict with keys: get, post, put — each is a MagicMock.
    """
    with (
        patch("httpx.get") as mock_get,
        patch("httpx.post") as mock_post,
        patch("httpx.put") as mock_put,
    ):
        yield {"get": mock_get, "post": mock_post, "put": mock_put}


@pytest.fixture
def mock_strava_api(mock_httpx):
    """Mock the Strava API with default OK responses for common endpoints.

    Returns the mock dict so tests can customize responses per endpoint.
    """
    # Default: return empty/OK
    mock_response = MagicMock()
    mock_response.status_code = 200
    mock_response.headers = {}
    mock_response.json.return_value = {}
    mock_response.raise_for_status.return_value = None

    mock_httpx["get"].return_value = mock_response
    mock_httpx["post"].return_value = mock_response
    mock_httpx["put"].return_value = mock_response

    return mock_httpx


@pytest.fixture
def sample_activity_summaries():
    """Minimal Strava activity summary list."""
    return [
        {
            "id": 100,
            "start_date": "2025-02-01T08:00:00Z",
            "start_date_local": "2025-02-01T08:00:00",
            "timezone": "UTC",
            "sport_type": "Run",
            "map": {"summary_polyline": "_p~iF~ps|U_ulLnnqC_mqNvxq`@"},
        },
        {
            "id": 101,
            "start_date": "2025-02-02T09:00:00Z",
            "start_date_local": "2025-02-02T09:00:00",
            "timezone": "UTC",
            "sport_type": "Ride",
            "map": {"summary_polyline": ""},
        },
        {
            "id": 102,
            "start_date": "2025-02-03T10:00:00Z",
            "start_date_local": "2025-02-03T10:00:00",
            "timezone": "UTC",
            "sport_type": "VirtualRide",
            "map": {},
        },
    ]


@pytest.fixture
def sample_gps_stream():
    """Typical Strava latlng stream response."""
    return {
        "latlng": {
            "data": [
                [52.0, 4.5],
                [52.001, 4.501],
                [52.002, 4.502],
                [52.003, 4.503],
                [52.004, 4.504],
            ],
        },
        "time": {
            "data": [0, 10, 20, 30, 40],
        },
    }


@pytest.fixture
def sample_gps_stream_no_times():
    """GPS stream without time data."""
    return {
        "latlng": {
            "data": [
                [52.0, 4.5],
                [52.001, 4.501],
                [52.002, 4.502],
            ],
        },
    }
