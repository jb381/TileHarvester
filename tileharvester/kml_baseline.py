"""Import and compare authoritative Squadrats KML tile snapshots."""

from __future__ import annotations

import hashlib
import math
import sqlite3
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from tileharvester.config import settings
from tileharvester.db import get_db
from tileharvester.history import rebuild_tile_history
from tileharvester.tile_engine import MAX_LATITUDE, validate_tile_id

MAX_KML_BYTES = 100 * 1024 * 1024
MAX_RASTER_TILES = 2_000_000
MAX_SCAN_WORK = 50_000_000
_LAYER_ZOOMS = {"squadrats": 14, "squadratinhos": 17}


@dataclass(frozen=True)
class KmlTileSet:
    """Exact tile sets parsed from a Squadrats KML export."""

    squadrats: frozenset[str]
    squadratinhos: frozenset[str]
    sha256: str

    def for_kind(self, tile_kind: str) -> frozenset[str]:
        if tile_kind == "squadrat":
            return self.squadrats
        if tile_kind == "squadratinho":
            return self.squadratinhos
        raise ValueError(f"Unsupported tile kind: {tile_kind}")


def _local_name(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def _child(element: ET.Element, name: str) -> ET.Element | None:
    return next((child for child in element if _local_name(child.tag) == name), None)


def _descendants(element: ET.Element, name: str) -> list[ET.Element]:
    return [descendant for descendant in element.iter() if _local_name(descendant.tag) == name]


def _continuous_tile(lat: float, lon: float, zoom: int) -> tuple[float, float]:
    if not math.isfinite(lat) or not math.isfinite(lon):
        raise ValueError("KML contains a non-finite coordinate")
    if not -90 <= lat <= 90 or not -180 <= lon <= 180:
        raise ValueError(f"KML coordinate is outside valid longitude/latitude bounds: {lon},{lat}")
    lat = max(min(lat, MAX_LATITUDE), -MAX_LATITUDE)
    n = 2**zoom
    x = (lon + 180.0) / 360.0 * n
    lat_rad = math.radians(lat)
    y = (1.0 - math.asinh(math.tan(lat_rad)) / math.pi) / 2.0 * n
    return x, y


def _parse_ring(text: str, zoom: int) -> list[tuple[float, float]]:
    points: list[tuple[float, float]] = []
    for token in text.split():
        parts = token.split(",")
        if len(parts) < 2:
            raise ValueError(f"Invalid KML coordinate tuple: {token!r}")
        try:
            lon = float(parts[0])
            lat = float(parts[1])
        except ValueError as exc:
            raise ValueError(f"Invalid KML coordinate tuple: {token!r}") from exc
        points.append(_continuous_tile(lat, lon, zoom))

    if len(points) < 4:
        raise ValueError("KML polygon ring contains fewer than four coordinates")
    if points[0] != points[-1]:
        points.append(points[0])

    # Keep antimeridian polygons compact while scanning, then wrap tile x below.
    world_width = 2**zoom
    xs = [point[0] for point in points]
    if max(xs) - min(xs) > world_width / 2:
        points = [(x + world_width if x < world_width / 2 else x, y) for x, y in points]
    return points


def _tiles_inside_ring(text: str, zoom: int) -> set[str]:
    """Rasterize a tile-grid polygon by testing tile centers with scanlines."""
    points = _parse_ring(text, zoom)
    world_width = 2**zoom
    minimum_y = max(0, math.floor(min(y for _, y in points)))
    maximum_y = min(world_width, math.ceil(max(y for _, y in points)))
    tiles: set[str] = set()
    if (maximum_y - minimum_y) * len(points) > MAX_SCAN_WORK:
        raise ValueError("KML polygon exceeds the rasterization work limit")

    for tile_y in range(minimum_y, maximum_y):
        scan_y = tile_y + 0.5
        intersections: list[float] = []
        for (x1, y1), (x2, y2) in zip(points, points[1:], strict=False):
            if (y1 > scan_y) != (y2 > scan_y):
                intersections.append(x1 + (scan_y - y1) * (x2 - x1) / (y2 - y1))
        intersections.sort()
        if len(intersections) % 2:
            raise ValueError("KML polygon has an invalid self-intersecting ring")

        for left, right in zip(intersections[::2], intersections[1::2], strict=True):
            first_x = math.ceil(left - 0.5 - 1e-9)
            last_x = math.floor(right - 0.5 + 1e-9)
            if len(tiles) + max(0, last_x - first_x + 1) > MAX_RASTER_TILES:
                raise ValueError("KML polygon exceeds the rasterized tile limit")
            for tile_x in range(first_x, last_x + 1):
                wrapped_x = tile_x % world_width
                tiles.add(validate_tile_id(f"{zoom}:{wrapped_x}:{tile_y}"))
    return tiles


def _ring_coordinates(boundary: ET.Element, boundary_name: str) -> str:
    ring = _child(boundary, "LinearRing")
    coordinates = _child(ring, "coordinates") if ring is not None else None
    text = coordinates.text.strip() if coordinates is not None and coordinates.text else ""
    if not text:
        raise ValueError(f"KML {boundary_name} has no coordinates")
    return text


def _tiles_inside_polygon(polygon: ET.Element, zoom: int) -> set[str]:
    outer = _child(polygon, "outerBoundaryIs")
    if outer is None:
        raise ValueError("KML polygon has no outer boundary")
    tiles = _tiles_inside_ring(_ring_coordinates(outer, "outer boundary"), zoom)
    for inner in (child for child in polygon if _local_name(child.tag) == "innerBoundaryIs"):
        tiles.difference_update(
            _tiles_inside_ring(_ring_coordinates(inner, "inner boundary"), zoom)
        )
    return tiles


def parse_squadrats_kml(path: Path) -> KmlTileSet:
    """Parse dissolved Squadrats polygons into exact z14 and z17 tile IDs."""
    path = path.expanduser()
    if not path.is_file():
        raise ValueError(f"KML file does not exist: {path}")
    if path.stat().st_size > MAX_KML_BYTES:
        raise ValueError(f"KML file exceeds the {MAX_KML_BYTES // (1024 * 1024)} MiB safety limit")

    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    try:
        root = ET.parse(path).getroot()
    except ET.ParseError as exc:
        raise ValueError(f"Invalid KML XML: {exc}") from exc

    parsed: dict[str, set[str]] = {"squadrats": set(), "squadratinhos": set()}
    found_layers: set[str] = set()
    for placemark in _descendants(root, "Placemark"):
        name_element = _child(placemark, "name")
        name = (name_element.text or "").strip().casefold() if name_element is not None else ""
        if name not in _LAYER_ZOOMS:
            continue
        found_layers.add(name)
        polygons = _descendants(placemark, "Polygon")
        if not polygons:
            raise ValueError(f"Squadrats KML layer {name!r} contains no polygons")
        for polygon in polygons:
            parsed[name].update(_tiles_inside_polygon(polygon, _LAYER_ZOOMS[name]))
            if len(parsed[name]) > MAX_RASTER_TILES:
                raise ValueError("KML layer exceeds the rasterized tile limit")

    if not found_layers:
        raise ValueError("KML contains neither a 'squadrats' nor a 'squadratinhos' layer")
    if found_layers != set(_LAYER_ZOOMS):
        raise ValueError("KML must include both squadrats and squadratinhos layers")
    if "squadrats" not in found_layers or not parsed["squadrats"]:
        raise ValueError("KML contains no exact Squadrats tiles")

    return KmlTileSet(
        squadrats=frozenset(parsed["squadrats"]),
        squadratinhos=frozenset(parsed["squadratinhos"]),
        sha256=digest,
    )


def parse_as_of_utc(value: str | None) -> str:
    """Normalize an optional ISO-8601 baseline timestamp to UTC."""
    if value is None:
        parsed = datetime.now(tz=timezone.utc)
    else:
        normalized = value.strip().replace("Z", "+00:00")
        try:
            parsed = datetime.fromisoformat(normalized)
        except ValueError as exc:
            raise ValueError(
                "--as-of must be an ISO-8601 timestamp, for example 2026-08-01T18:43:00Z"
            ) from exc
        if parsed.tzinfo is None:
            raise ValueError("--as-of must include a timezone, for example Z or +02:00")
        parsed = parsed.astimezone(timezone.utc)
    return parsed.isoformat().replace("+00:00", "Z")


def import_baseline(path: Path, as_of: str | None = None) -> dict[str, Any]:
    """Store one immutable KML baseline and mark unprocessed covered activities."""
    if (settings.squadrat_zoom, settings.squadratinho_zoom) != (14, 17):
        raise ValueError("Squadrats KML baselines require zoom levels 14 and 17")
    tiles = parse_squadrats_kml(path)
    as_of_utc = parse_as_of_utc(as_of)
    imported_at = datetime.now(tz=timezone.utc).isoformat().replace("+00:00", "Z")

    with get_db() as conn:
        existing = conn.execute("SELECT source_name, imported_at FROM baseline_imports").fetchone()
        if existing is not None:
            raise ValueError(
                "A KML baseline is already installed "
                f"from {existing['source_name']} at {existing['imported_at']}; "
                "use 'compare-kml' for newer exports"
            )
        conn.execute(
            """
            INSERT INTO baseline_imports
                (id, source_name, sha256, imported_at, as_of_utc,
                 squadrat_count, squadratinho_count)
            VALUES (1, ?, ?, ?, ?, ?, ?)
            """,
            (
                path.name,
                tiles.sha256,
                imported_at,
                as_of_utc,
                len(tiles.squadrats),
                len(tiles.squadratinhos),
            ),
        )
        conn.executemany(
            "INSERT INTO baseline_tiles (import_id, tile_kind, tile_id) VALUES (1, 'squadrat', ?)",
            [(tile_id,) for tile_id in tiles.squadrats],
        )
        conn.executemany(
            "INSERT INTO baseline_tiles (import_id, tile_kind, tile_id) VALUES (1, 'squadratinho', ?)",
            [(tile_id,) for tile_id in tiles.squadratinhos],
        )
        # Existing processed history retains its old semantics. Pending/failed
        # activities covered by the snapshot use the baseline when later processed.
        conn.execute(
            """
            UPDATE activities
            SET baseline_covered = 1
            WHERE status != 'processed'
              AND julianday(start_utc) <= julianday(?)
            """,
            (as_of_utc,),
        )
        rebuilt = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE status = 'processed'"
        ).fetchone()[0]
        rebuild_tile_history(conn)
        conn.commit()

    return {
        "rebuilt": rebuilt,
        "source_name": path.name,
        "sha256": tiles.sha256,
        "imported_at": imported_at,
        "as_of_utc": as_of_utc,
        "squadrats": len(tiles.squadrats),
        "squadratinhos": len(tiles.squadratinhos),
    }


def get_baseline(conn: sqlite3.Connection) -> sqlite3.Row | None:
    """Return the installed baseline metadata, if any."""
    row = conn.execute("SELECT * FROM baseline_imports WHERE id = 1").fetchone()
    if row is not None and (settings.squadrat_zoom, settings.squadratinho_zoom) != (14, 17):
        raise ValueError("Installed Squadrats KML baseline requires zoom levels 14 and 17")
    return row  # type: ignore[no-any-return]


def activity_is_covered(conn: sqlite3.Connection, start_utc: str) -> bool:
    """Return whether a newly discovered activity belongs to the snapshot period."""
    baseline = get_baseline(conn)
    if baseline is None:
        return False
    row = conn.execute(
        "SELECT julianday(?) <= julianday(?)", (start_utc, baseline["as_of_utc"])
    ).fetchone()
    return bool(row[0])


def activity_uses_baseline(conn: sqlite3.Connection, row: sqlite3.Row) -> bool:
    """Return whether novelty for an activity must include the imported snapshot."""
    baseline = get_baseline(conn)
    if baseline is None:
        return False
    if bool(row["baseline_covered"]):
        return True
    result = conn.execute(
        "SELECT julianday(?) > julianday(?)", (row["start_utc"], baseline["as_of_utc"])
    ).fetchone()
    return bool(result[0])


def baseline_tiles_on_route(conn: sqlite3.Connection, tile_kind: str, tiles: set[str]) -> set[str]:
    """Return route tiles already present in the imported KML baseline."""
    seen: set[str] = set()
    tile_list = list(tiles)
    for index in range(0, len(tile_list), 500):
        chunk = tile_list[index : index + 500]
        placeholders = ",".join("?" for _ in chunk)
        rows = conn.execute(
            f"""
            SELECT tile_id
            FROM baseline_tiles
            WHERE tile_kind = ?
              AND tile_id IN ({placeholders})
            """,
            (tile_kind, *chunk),
        ).fetchall()
        seen.update(row["tile_id"] for row in rows)
    return seen


def effective_tiles(conn: sqlite3.Connection, tile_kind: str) -> set[str]:
    """Return the authoritative baseline union locally observed later tiles."""
    baseline = get_baseline(conn)
    if baseline is None:
        rows = conn.execute(
            "SELECT tile_id FROM global_tiles WHERE tile_kind = ?", (tile_kind,)
        ).fetchall()
        return {row["tile_id"] for row in rows}

    rows = conn.execute(
        """
        SELECT tile_id FROM baseline_tiles WHERE tile_kind = ?
        UNION
        SELECT at.tile_id
        FROM activity_tiles at
        JOIN activities a ON a.id = at.activity_id
        WHERE at.tile_kind = ?
          AND a.status = 'processed'
          AND (a.baseline_covered = 1 OR julianday(a.start_utc) > julianday(?))
        """,
        (tile_kind, tile_kind, baseline["as_of_utc"]),
    ).fetchall()
    return {row["tile_id"] for row in rows}


def effective_tile_count(tile_kind: str) -> int:
    """Count current authoritative tiles, preserving legacy behavior without a baseline."""
    with get_db() as conn:
        baseline = get_baseline(conn)
        if baseline is None:
            return int(
                conn.execute(
                    "SELECT COUNT(*) FROM global_tiles WHERE tile_kind = ?", (tile_kind,)
                ).fetchone()[0]
            )
        return int(
            conn.execute(
                """
                SELECT COUNT(*)
                FROM (
                    SELECT tile_id FROM baseline_tiles WHERE tile_kind = ?
                    UNION
                    SELECT at.tile_id
                    FROM activity_tiles at
                    JOIN activities a ON a.id = at.activity_id
                    WHERE at.tile_kind = ?
                      AND a.status = 'processed'
                      AND (
                          a.baseline_covered = 1
                          OR julianday(a.start_utc) > julianday(?)
                      )
                )
                """,
                (tile_kind, tile_kind, baseline["as_of_utc"]),
            ).fetchone()[0]
        )


def effective_tile_count_through(
    conn: sqlite3.Connection,
    tile_kind: str,
    activity: sqlite3.Row,
    *,
    include_activity: bool,
) -> int:
    """Count authoritative tiles before or through one activity."""
    if not activity_uses_baseline(conn, activity):
        current_clause = "OR a.id = ?" if include_activity else ""
        params: tuple[Any, ...] = (
            tile_kind,
            activity["start_utc"],
            activity["start_utc"],
            activity["id"],
        )
        if include_activity:
            params += (activity["id"],)
        return int(
            conn.execute(
                f"""
                SELECT COUNT(DISTINCT at.tile_id)
                FROM activity_tiles at
                JOIN activities a ON a.id = at.activity_id
                WHERE at.tile_kind = ?
                  AND a.status = 'processed'
                  AND (julianday(a.start_utc) < julianday(?)
                       OR (julianday(a.start_utc) = julianday(?) AND a.id < ?) {current_clause})
                """,
                params,
            ).fetchone()[0]
        )

    baseline = get_baseline(conn)
    if baseline is None:  # Defensive: activity_uses_baseline already ruled this out.
        return 0
    current_clause = "OR a.id = ?" if include_activity else ""
    params = (
        (
            tile_kind,
            tile_kind,
            baseline["as_of_utc"],
            activity["start_utc"],
            activity["start_utc"],
            activity["id"],
            activity["id"],
        )
        if include_activity
        else (
            tile_kind,
            tile_kind,
            baseline["as_of_utc"],
            activity["start_utc"],
            activity["start_utc"],
            activity["id"],
        )
    )
    return int(
        conn.execute(
            f"""
            SELECT COUNT(*)
            FROM (
                SELECT tile_id
                FROM baseline_tiles
                WHERE tile_kind = ?
                UNION
                SELECT at.tile_id
                FROM activity_tiles at
                JOIN activities a ON a.id = at.activity_id
                WHERE at.tile_kind = ?
                  AND a.status = 'processed'
                  AND (
                      a.baseline_covered = 1
                      OR julianday(a.start_utc) > julianday(?)
                  )
                  AND (
                      julianday(a.start_utc) < julianday(?)
                      OR (julianday(a.start_utc) = julianday(?) AND a.id < ?)
                      {current_clause}
                  )
            )
            """,
            params,
        ).fetchone()[0]
    )


def compare_kml(path: Path) -> dict[str, dict[str, int]]:
    """Compare a KML export with the current effective local tile sets."""
    exported = parse_squadrats_kml(path)
    result: dict[str, dict[str, int]] = {}
    with get_db() as conn:
        for tile_kind in ("squadrat", "squadratinho"):
            kml_tiles = set(exported.for_kind(tile_kind))
            local_tiles = effective_tiles(conn, tile_kind)
            result[tile_kind] = {
                "kml": len(kml_tiles),
                "local": len(local_tiles),
                "shared": len(kml_tiles & local_tiles),
                "kml_only": len(kml_tiles - local_tiles),
                "local_only": len(local_tiles - kml_tiles),
            }
    return result
