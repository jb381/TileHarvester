"""Tests for importing and applying Squadrats KML baselines."""

import math
from pathlib import Path

import pytest
from typer.testing import CliRunner

import tileharvester.sync as sync_mod
from tileharvester.cli import app
from tileharvester.db import get_db
from tileharvester.kml_baseline import (
    activity_uses_baseline,
    compare_kml,
    effective_tile_count,
    import_baseline,
    parse_as_of_utc,
    parse_squadrats_kml,
)
from tileharvester.recompute import recompute_novelty_from_stored_tiles


def _tile_corner(tile_x: float, tile_y: float, zoom: int) -> tuple[float, float]:
    n = 2**zoom
    lon = tile_x / n * 360.0 - 180.0
    lat = math.degrees(math.atan(math.sinh(math.pi * (1 - 2 * tile_y / n))))
    return lon, lat


def _ring(left: int, top: int, right: int, bottom: int, zoom: int) -> str:
    corners = [
        _tile_corner(left, top, zoom),
        _tile_corner(right, top, zoom),
        _tile_corner(right, bottom, zoom),
        _tile_corner(left, bottom, zoom),
        _tile_corner(left, top, zoom),
    ]
    return " ".join(f"{lon:.15f},{lat:.15f},0" for lon, lat in corners)


def _write_fixture(path: Path) -> Path:
    squadrat_outer = _ring(8000, 6000, 8002, 6002, 14)
    squadrat_hole = _ring(8001, 6001, 8002, 6002, 14)
    squadratinho = _ring(64000, 48000, 64001, 48001, 17)
    ignored_uber = _ring(7999, 5999, 8003, 6003, 14)
    content = f"""<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2">
  <Document>
    <Placemark><name>squadrats</name><MultiGeometry><Polygon>
      <outerBoundaryIs><LinearRing><coordinates>{squadrat_outer}</coordinates></LinearRing></outerBoundaryIs>
      <innerBoundaryIs><LinearRing><coordinates>{squadrat_hole}</coordinates></LinearRing></innerBoundaryIs>
    </Polygon></MultiGeometry></Placemark>
    <Placemark><name>squadratinhos</name><MultiGeometry><Polygon>
      <outerBoundaryIs><LinearRing><coordinates>{squadratinho}</coordinates></LinearRing></outerBoundaryIs>
    </Polygon></MultiGeometry></Placemark>
    <Placemark><name>ubersquadrat</name><Polygon>
      <outerBoundaryIs><LinearRing><coordinates>{ignored_uber}</coordinates></LinearRing></outerBoundaryIs>
    </Polygon></Placemark>
  </Document>
</kml>
"""
    path.write_text(content)
    return path


def _insert_activity(
    activity_id: int,
    start_utc: str,
    *,
    status: str = "pending",
    baseline_covered: int = 0,
) -> None:
    with get_db() as conn:
        conn.execute(
            """
            INSERT INTO activities
                (id, start_utc, start_local, status, baseline_covered)
            VALUES (?, ?, ?, ?, ?)
            """,
            (activity_id, start_utc, start_utc.replace("Z", ""), status, baseline_covered),
        )
        conn.commit()


def _tile_center(tile_x: int, tile_y: int, zoom: int) -> tuple[float, float]:
    lon, lat = _tile_corner(tile_x + 0.5, tile_y + 0.5, zoom)
    return lat, lon


def test_parser_rasterizes_dissolved_polygons_and_holes(tmp_path: Path) -> None:
    path = _write_fixture(tmp_path / "squadrats.kml")

    result = parse_squadrats_kml(path)

    assert result.squadrats == {
        "14:8000:6000",
        "14:8001:6000",
        "14:8000:6001",
    }
    assert result.squadratinhos == {"17:64000:48000"}
    assert len(result.sha256) == 64


def test_parser_rejects_unrelated_or_malformed_files(tmp_path: Path) -> None:
    unrelated = tmp_path / "unrelated.kml"
    unrelated.write_text("<kml><Placemark><name>yard</name></Placemark></kml>")
    malformed = tmp_path / "malformed.kml"
    malformed.write_text("<kml>")

    with pytest.raises(ValueError, match="neither"):
        parse_squadrats_kml(unrelated)
    with pytest.raises(ValueError, match="Invalid KML XML"):
        parse_squadrats_kml(malformed)


def test_import_is_immutable_and_preserves_processed_history(
    isolated_db: Path, tmp_path: Path
) -> None:
    del isolated_db
    path = _write_fixture(tmp_path / "squadrats.kml")
    _insert_activity(1, "2026-07-01T10:00:00Z", status="processed")
    _insert_activity(2, "2026-07-02T10:00:00Z")

    result = import_baseline(path, as_of="2026-08-01T18:43:00Z")

    assert result["squadrats"] == 3
    assert result["squadratinhos"] == 1
    with get_db() as conn:
        metadata = conn.execute("SELECT * FROM baseline_imports").fetchone()
        activities = conn.execute(
            "SELECT id, baseline_covered FROM activities ORDER BY id"
        ).fetchall()
        assert metadata["source_name"] == "squadrats.kml"
        assert conn.execute("SELECT COUNT(*) FROM baseline_tiles").fetchone()[0] == 4
    assert [(row["id"], row["baseline_covered"]) for row in activities] == [(1, 0), (2, 1)]

    with pytest.raises(ValueError, match="already installed"):
        import_baseline(path, as_of="2026-08-01T18:43:00Z")


def test_baseline_suppresses_seen_tiles_and_adds_future_tiles(
    isolated_db: Path, tmp_path: Path
) -> None:
    del isolated_db
    path = _write_fixture(tmp_path / "squadrats.kml")
    import_baseline(path, as_of="2026-08-01T18:43:00Z")
    _insert_activity(10, "2026-08-02T10:00:00Z")
    _insert_activity(11, "2026-08-03T10:00:00Z")

    with get_db() as conn:
        first = conn.execute("SELECT * FROM activities WHERE id = 10").fetchone()
        second = conn.execute("SELECT * FROM activities WHERE id = 11").fetchone()
        assert activity_uses_baseline(conn, first)

    seen_result = sync_mod._store_activity_tiles(first, [_tile_center(8000, 6000, 14)], "test")
    new_result = sync_mod._store_activity_tiles(second, [_tile_center(8001, 6001, 14)], "test")

    assert seen_result["new_squadrats"] == 0
    assert new_result["new_squadrats"] == 1
    assert effective_tile_count("squadrat") == 4
    assert sync_mod.compute_total_unique_squadrats_through(11, second["start_local"]) == 4

    recompute_novelty_from_stored_tiles()
    with get_db() as conn:
        counts = conn.execute(
            "SELECT id, new_squadrat_count FROM activities ORDER BY id"
        ).fetchall()
        baseline_count = conn.execute("SELECT COUNT(*) FROM baseline_tiles").fetchone()[0]
    assert [(row["id"], row["new_squadrat_count"]) for row in counts] == [(10, 0), (11, 1)]
    assert baseline_count == 4


def test_newly_discovered_snapshot_activity_is_marked_covered(
    isolated_db: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    del isolated_db
    path = _write_fixture(tmp_path / "squadrats.kml")
    import_baseline(path, as_of="2026-08-01T18:43:00Z")
    monkeypatch.setattr(
        sync_mod,
        "get_activities",
        lambda **_kwargs: [
            {
                "id": 20,
                "start_date": "2026-07-31T10:00:00Z",
                "start_date_local": "2026-07-31T12:00:00",
                "timezone": "Europe/Amsterdam",
                "sport_type": "Ride",
                "map": {"summary_polyline": "abc"},
            }
        ],
    )

    sync_mod.fetch_and_store_summaries()

    with get_db() as conn:
        covered = conn.execute("SELECT baseline_covered FROM activities WHERE id = 20").fetchone()[
            0
        ]
    assert covered == 1


def test_effective_counts_keep_legacy_global_tiles_without_baseline(isolated_db: Path) -> None:
    del isolated_db
    _insert_activity(1, "2026-07-01T10:00:00Z", status="processed")
    with get_db() as conn:
        conn.execute("INSERT INTO global_tiles VALUES ('squadrat', '14:1:1', 1, '2026-07-01')")
        conn.commit()

    assert effective_tile_count("squadrat") == 1


def test_compare_is_read_only(isolated_db: Path, tmp_path: Path) -> None:
    del isolated_db
    path = _write_fixture(tmp_path / "squadrats.kml")
    import_baseline(path, as_of="2026-08-01T18:43:00Z")

    result = compare_kml(path)

    assert result["squadrat"] == {
        "kml": 3,
        "local": 3,
        "shared": 3,
        "kml_only": 0,
        "local_only": 0,
    }


def test_cli_import_and_compare(isolated_db: Path, tmp_path: Path) -> None:
    del isolated_db
    path = _write_fixture(tmp_path / "squadrats.kml")
    runner = CliRunner()

    imported = runner.invoke(app, ["import-kml", str(path), "--as-of", "2026-08-01T18:43:00Z"])
    compared = runner.invoke(app, ["compare-kml", str(path)])

    assert imported.exit_code == 0, imported.output
    assert "Imported 3 Squadrats and 1 Squadratinhos" in imported.output
    assert compared.exit_code == 0, compared.output
    assert "KML-only" in compared.output


def test_as_of_requires_explicit_timezone() -> None:
    with pytest.raises(ValueError, match="timezone"):
        parse_as_of_utc("2026-08-01T18:43:00")
