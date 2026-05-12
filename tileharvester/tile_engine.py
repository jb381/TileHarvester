"""Tile engine for Squadrats-compatible Mapbox tile computation."""

import math
import re
from typing import Any, Protocol

from tileharvester.config import settings

# Web Mercator constants
EARTH_RADIUS = 6378137.0  # meters
MAX_EXTENT = math.pi * EARTH_RADIUS  # half the world width in meters
MAX_LATITUDE = 85.05112878

_TILE_ID_RE = re.compile(r"^\d+:\d+:\d+$")
_ZOOM_TILE_ID_RE = re.compile(r"^\d+:\d+$")


def validate_tile_id(tile_id: str) -> str:
    """Validate and return a tile_id string.

    Accepts both formats: ``zoom:x:y`` and ``x:y``.
    Raises ValueError if the format is invalid.
    """
    if _TILE_ID_RE.match(tile_id) or _ZOOM_TILE_ID_RE.match(tile_id):
        return tile_id
    raise ValueError(f"Invalid tile_id format: {tile_id!r}")


def _latlon_to_meters(lat: float, lon: float) -> tuple[float, float]:
    """Convert lat/lon to Web Mercator meters."""
    x = EARTH_RADIUS * math.radians(lon)
    y = EARTH_RADIUS * math.log(math.tan(math.pi / 4 + math.radians(lat) / 2))
    return x, y


def _meters_to_tile(x: float, y: float, tile_size: float) -> str:
    """Convert mercator meters to tile id string."""
    tx = math.floor((x + MAX_EXTENT) / tile_size)
    ty = math.floor((y + MAX_EXTENT) / tile_size)
    return validate_tile_id(f"{tx}:{ty}")


class TileEngine(Protocol):
    id: str
    version: str

    def tiles_for_points(self, points: list[tuple[float, float]]) -> tuple[set[str], set[str]]: ...


class SquadratsEngine:
    """Squadrats-compatible tile engine using Mapbox/XYZ tile coordinates."""

    id = "mapbox-z14-z17"
    version = "2"

    def __init__(self, squadrat_zoom: int = 14, squadratinho_zoom: int = 17):
        self.squadrat_zoom = squadrat_zoom
        self.squadratinho_zoom = squadratinho_zoom

    def _continuous_tile(self, lat: float, lon: float, zoom: int) -> tuple[float, float]:
        lat = max(min(lat, MAX_LATITUDE), -MAX_LATITUDE)
        n = 2**zoom
        x = (lon + 180.0) / 360.0 * n
        lat_rad = math.radians(lat)
        y = (1.0 - math.log(math.tan(lat_rad) + (1.0 / math.cos(lat_rad))) / math.pi) / 2.0 * n
        return x, y

    def _tile_id(self, x: int, y: int, zoom: int) -> str:
        n = 2**zoom
        x = min(max(x, 0), n - 1)
        y = min(max(y, 0), n - 1)
        return validate_tile_id(f"{zoom}:{x}:{y}")

    def _tiles_for_segment(
        self, start: tuple[float, float], end: tuple[float, float], zoom: int
    ) -> set[str]:
        x0, y0 = start
        x1, y1 = end
        n = 2**zoom

        # --- Antimeridian crossing detection ---
        # If the x-coordinate delta exceeds half the world width, the segment
        # crosses the ±180° line the short way. Split into two sub-segments
        # that don't cross the boundary.
        if abs(x1 - x0) > n / 2:
            epsilon = 1e-10
            if x0 < x1:
                # Crossing through x=0 (westward wrap): (x0,y0) → 0, then n → x1
                frac = x0 / (x0 + (n - x1)) if (x0 + (n - x1)) else 0.0
                y_mid = y0 + frac * (y1 - y0)
                tiles: set[str] = set()
                tiles.update(self._tiles_for_segment(start, (epsilon, y_mid), zoom))
                tiles.update(
                    self._tiles_for_segment((float(n), y_mid), end, zoom)
                )
            else:
                # Crossing through x=n (eastward wrap): (x0,y0) → n, then 0 → x1
                frac = (n - x0) / ((n - x0) + x1) if ((n - x0) + x1) else 0.0
                y_mid = y0 + frac * (y1 - y0)
                tiles = set()
                tiles.update(
                    self._tiles_for_segment(start, (float(n - epsilon), y_mid), zoom)
                )
                tiles.update(self._tiles_for_segment((0.0, y_mid), end, zoom))
            return tiles

        tx = math.floor(x0)
        ty = math.floor(y0)
        end_tx = math.floor(x1)
        end_ty = math.floor(y1)
        tiles = {self._tile_id(tx, ty, zoom)}

        dx = x1 - x0
        dy = y1 - y0
        if dx == 0 and dy == 0:
            return tiles

        step_x = 1 if dx > 0 else -1
        step_y = 1 if dy > 0 else -1
        t_delta_x = abs(1 / dx) if dx else math.inf
        t_delta_y = abs(1 / dy) if dy else math.inf
        next_x = tx + 1 if dx > 0 else tx
        next_y = ty + 1 if dy > 0 else ty
        t_max_x = (next_x - x0) / dx if dx else math.inf
        t_max_y = (next_y - y0) / dy if dy else math.inf

        while tx != end_tx or ty != end_ty:
            if t_max_x < t_max_y:
                tx += step_x
                t_max_x += t_delta_x
            elif t_max_y < t_max_x:
                ty += step_y
                t_max_y += t_delta_y
            else:
                tx += step_x
                ty += step_y
                t_max_x += t_delta_x
                t_max_y += t_delta_y
            tiles.add(self._tile_id(tx, ty, zoom))
        return tiles

    def _compute_segment_tiles(self, points: list[tuple[float, float]], zoom: int) -> set[str]:
        tiles: set[str] = set()
        previous: tuple[float, float] | None = None
        for point in points:
            current = self._continuous_tile(point[0], point[1], zoom)
            if previous is None:
                tiles.add(self._tile_id(math.floor(current[0]), math.floor(current[1]), zoom))
            else:
                tiles.update(self._tiles_for_segment(previous, current, zoom))
            previous = current
        return tiles

    def _compute_tiles(self, segments: list[list[tuple[float, float]]], zoom: int) -> set[str]:
        tiles: set[str] = set()
        for segment in segments:
            tiles.update(self._compute_segment_tiles(segment, zoom))
        return tiles

    def tiles_for_points(self, points: list[tuple[float, float]]) -> tuple[set[str], set[str]]:
        """Return (squadrats, squadratinhos) for a list of lat/lon points."""
        return self.tiles_for_segments([points] if points else [])

    def tiles_for_segments(
        self, segments: list[list[tuple[float, float]]]
    ) -> tuple[set[str], set[str]]:
        """Return (squadrats, squadratinhos) for one or more track segments."""
        squadrats = self._compute_tiles(segments, self.squadrat_zoom)
        squadratinhos = self._compute_tiles(segments, self.squadratinho_zoom)
        return squadrats, squadratinhos


def make_engine(config: dict[str, Any] | None = None) -> SquadratsEngine:  # noqa: ARG001
    return SquadratsEngine(
        squadrat_zoom=settings.squadrat_zoom,
        squadratinho_zoom=settings.squadratinho_zoom,
    )
