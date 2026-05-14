"""Tests for tile engine."""

import math

from tileharvester.tile_engine import SquadratsEngine, _latlon_to_meters


def test_meters_conversion():
    # At equator, 1 degree longitude ≈ 111.32 km
    x, y = _latlon_to_meters(0.0, 1.0)
    assert abs(x - 111319.49) < 100  # within 100m
    assert math.isclose(y, 0.0, abs_tol=1e-6)


def test_simple_points():
    engine = SquadratsEngine(squadrat_zoom=14, squadratinho_zoom=17)
    points = [
        (0.0, 0.0),
        (0.0, 0.0),  # duplicate
    ]
    squadrats, squadratinhos = engine.tiles_for_points(points)
    assert squadrats == {"14:8192:8192"}
    assert squadratinhos == {"17:65536:65536"}


def test_tile_size_ratio():
    engine = SquadratsEngine(squadrat_zoom=14, squadratinho_zoom=17)
    points = [(0.0, 0.0), (0.0, 0.1)]
    squadrats, squadratinhos = engine.tiles_for_points(points)
    assert len(squadrats) > 1
    assert len(squadratinhos) >= len(squadrats)
    assert len(squadratinhos) <= len(squadrats) * 12


def test_antimeridian_crossing_uses_short_path():
    engine = SquadratsEngine(squadrat_zoom=14, squadratinho_zoom=17)
    points = [(0.0, 179.9), (0.0, -179.9)]

    squadrats, squadratinhos = engine.tiles_for_points(points)

    assert len(squadrats) < 100
    assert len(squadratinhos) < 500
    assert any(tile.startswith("14:0:") for tile in squadrats)
    assert any(tile.startswith("14:16383:") for tile in squadrats)
