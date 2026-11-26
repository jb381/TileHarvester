//! Import and compare authoritative Squadrats KML snapshots.

use std::{collections::HashSet, fs::File, io::Read, path::Path};

use chrono::{DateTime, SecondsFormat, Utc};
use encoding_rs::{UTF_16BE, UTF_16LE, UTF_8};
use roxmltree::{Document, Node, ParsingOptions};
use rusqlite::{
    params, params_from_iter, Connection, OpenFlags, OptionalExtension, TransactionBehavior,
};
use sha2::{Digest, Sha256};

use crate::{
    config::{expand_tilde, Settings},
    db::{get_db, ActivityRow},
    error::{Error, Result},
    history::rebuild_tile_history,
    models::TileKind,
    tile_engine::{project, tile_ids, TileCoord, TileSet},
};

pub const MAX_KML_BYTES: usize = 100 * 1024 * 1024;
pub const MAX_RASTER_TILES: usize = 2_000_000;
pub const MAX_SCAN_WORK: usize = 50_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmlTileSet {
    pub squadrats: HashSet<String>,
    pub squadratinhos: HashSet<String>,
    pub sha256: String,
}

impl KmlTileSet {
    pub fn for_kind(&self, tile_kind: &str) -> Result<&HashSet<String>> {
        let kind = tile_kind
            .parse()
            .map_err(|_| Error::Validation(format!("Unsupported tile kind: {tile_kind}")))?;
        Ok(self.tiles(kind))
    }

    fn tiles(&self, kind: TileKind) -> &HashSet<String> {
        match kind {
            TileKind::Squadrat => &self.squadrats,
            TileKind::Squadratinho => &self.squadratinhos,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineMetadata {
    pub id: i64,
    pub source_name: String,
    pub sha256: String,
    pub imported_at: String,
    pub as_of_utc: String,
    pub squadrat_count: i64,
    pub squadratinho_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportBaselineResult {
    pub rebuilt: i64,
    pub source_name: String,
    pub sha256: String,
    pub imported_at: String,
    pub as_of_utc: String,
    pub squadrats: usize,
    pub squadratinhos: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmlDiffCounts {
    pub kml: usize,
    pub local: usize,
    pub shared: usize,
    pub kml_only: usize,
    pub local_only: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareKmlResult {
    pub squadrat: KmlDiffCounts,
    pub squadratinho: KmlDiffCounts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailedKmlDiff {
    pub counts: KmlDiffCounts,
    pub kml_only_tiles: Vec<String>,
    pub local_only_tiles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailedCompareKmlResult {
    pub squadrat: DetailedKmlDiff,
    pub squadratinho: DetailedKmlDiff,
}

fn ring_points(text: &str, width: f64) -> Result<Vec<(f64, f64)>> {
    let mut points = Vec::new();
    let mut min_x = width;
    let mut max_x: f64 = 0.0;
    for token in text.split_whitespace() {
        let mut parts = token.split(',');
        let mut coordinate = || {
            parts
                .next()
                .and_then(|part| part.parse::<f64>().ok())
                .ok_or_else(|| {
                    Error::Validation(format!("Invalid KML coordinate tuple: {token:?}"))
                })
        };
        let lon = coordinate()?;
        let lat = coordinate()?;
        if !lon.is_finite() || !lat.is_finite() {
            return Err(Error::Validation(
                "KML contains a non-finite coordinate".into(),
            ));
        }
        if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
            return Err(Error::Validation(format!(
                "KML coordinate is outside valid longitude/latitude bounds: {lon},{lat}"
            )));
        }
        let (x, y) = project(lat, lon, width)?;
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        points.push((x, y));
    }
    if points.len() < 4 {
        return Err(Error::Validation(
            "KML polygon ring contains fewer than four coordinates".into(),
        ));
    }
    if points.first() != points.last() {
        points.push(points[0]);
    }
    if max_x - min_x > width / 2.0 {
        for (x, _) in &mut points {
            if *x < width / 2.0 {
                *x += width;
            }
        }
    }
    Ok(points)
}

/// Rasterize a ring by testing tile centers against each scanline.
pub fn tiles_inside_ring(
    text: &str,
    zoom: u8,
    max_raster_tiles: usize,
    max_scan_work: usize,
) -> Result<HashSet<String>> {
    rasterize_ring(text, zoom, max_raster_tiles, max_scan_work).map(tile_ids)
}

fn rasterize_ring(
    text: &str,
    zoom: u8,
    max_raster_tiles: usize,
    max_scan_work: usize,
) -> Result<TileSet> {
    // Coordinates and the tile-center offset must be representable as f64.
    if zoom > 51 {
        return Err(Error::Validation(format!("Unsupported KML zoom: {zoom}")));
    }
    let width = 1_i64 << zoom;
    let points = ring_points(text, width as f64)?;
    let (min_y, max_y) = points
        .iter()
        .fold((width as f64, 0.0_f64), |(min, max), &(_, y)| {
            (min.min(y), max.max(y))
        });
    let first_y = (min_y.floor() as i64).max(0);
    let end_y = (max_y.ceil() as i64).min(width);
    struct ScanEdge {
        first_row: i64,
        last_row: i64,
        from: (f64, f64),
        to: (f64, f64),
    }

    let work_limit =
        || Error::Validation("KML polygon exceeds the rasterization work limit".into());
    let mut remaining_work = max_scan_work as u64;
    let mut edges = Vec::new();
    for edge in points.windows(2) {
        let (y1, y2) = (edge[0].1, edge[1].1);
        if y1 == y2 {
            continue;
        }
        let mut first_row = ((y1.min(y2) - 0.5).floor() as i64).max(first_y);
        let mut last_row = ((y1.max(y2) - 0.5).ceil() as i64).min(end_y - 1);
        // Test the endpoints with the original half-open crossing rule.
        let crosses = |row: i64| {
            let scan_y = row as f64 + 0.5;
            (y1 > scan_y) != (y2 > scan_y)
        };
        while first_row <= last_row && !crosses(first_row) {
            first_row += 1;
        }
        while first_row <= last_row && !crosses(last_row) {
            last_row -= 1;
        }
        if first_row > last_row {
            continue;
        }
        remaining_work = remaining_work
            .checked_sub((last_row - first_row + 1) as u64)
            .ok_or_else(work_limit)?;
        edges.push(ScanEdge {
            first_row,
            last_row,
            from: edge[0],
            to: edge[1],
        });
    }

    // Group edges by their starting row and reuse a single intersection buffer.
    edges.sort_unstable_by_key(|edge| edge.first_row);
    let mut pending = edges.iter().peekable();
    let mut active: Vec<&ScanEdge> = Vec::new();
    let mut tiles = TileSet::default();
    let mut intersections = Vec::new();
    let mut y = first_y;
    loop {
        active.retain(|edge| edge.last_row >= y);
        if active.is_empty() {
            let Some(next) = pending.peek() else {
                break;
            };
            y = next.first_row;
        }
        while let Some(edge) = pending.next_if(|edge| edge.first_row == y) {
            active.push(edge);
        }
        let scan_y = y as f64 + 0.5;
        intersections.clear();
        for edge in &active {
            let (x1, y1) = edge.from;
            let (x2, y2) = edge.to;
            intersections.push(x1 + (scan_y - y1) * (x2 - x1) / (y2 - y1));
        }
        intersections.sort_unstable_by(f64::total_cmp);
        let spans = intersections.chunks_exact(2);
        if !spans.remainder().is_empty() {
            return Err(Error::Validation(
                "KML polygon has an invalid self-intersecting ring".into(),
            ));
        }
        for span in spans {
            let first_x = (span[0] - 0.5 - 1e-9).ceil() as i64;
            let last_x = (span[1] - 0.5 + 1e-9).floor() as i64;
            if (last_x - first_x + 1).max(0) as u64 > (max_raster_tiles - tiles.len()) as u64 {
                return Err(Error::Validation(
                    "KML polygon exceeds the rasterized tile limit".into(),
                ));
            }
            for x in first_x..=last_x {
                tiles.insert(TileCoord {
                    zoom,
                    x: x.rem_euclid(width) as u64,
                    y: y as u64,
                });
            }
        }
        y += 1;
    }
    Ok(tiles)
}

fn has_name(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name
}

fn child<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
    node.children().find(|&node| has_name(node, name))
}

fn declared_xml_encoding(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let header_len = bytes.len().min(256);
    let prefix = std::str::from_utf8(&bytes[..header_len]).ok()?;
    let trimmed = prefix.trim_start();
    if !trimmed.starts_with("<?xml") {
        return None;
    }
    let end = trimmed.find("?>")?;
    let decl = &trimmed[..end];
    let idx = decl.find("encoding")?;
    let after = decl[idx + "encoding".len()..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let quote = after.chars().next().filter(|&c| c == '"' || c == '\'')?;
    let rest = &after[quote.len_utf8()..];
    let label_end = rest.find(quote)?;
    let enc = encoding_rs::Encoding::for_label(rest[..label_end].trim().as_bytes())?;
    // A byte stream whose `<?xml` header is already valid ASCII/UTF-8 cannot be UTF-16.
    if enc == UTF_16LE || enc == UTF_16BE {
        return None;
    }
    Some(enc)
}

fn contains_unsafe_declaration(xml: &str) -> bool {
    let bytes = xml.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &bytes[i..];
        if rest.starts_with(b"<!--") {
            if let Some(end) = xml[i + 4..].find("-->") {
                i += 4 + end + 3;
                continue;
            }
            return false;
        }
        if rest.starts_with(b"<![CDATA[") {
            if let Some(end) = xml[i + 9..].find("]]>") {
                i += 9 + end + 3;
                continue;
            }
            return false;
        }
        if rest
            .get(..9)
            .is_some_and(|p| p.eq_ignore_ascii_case(b"<!DOCTYPE"))
            || rest
                .get(..8)
                .is_some_and(|p| p.eq_ignore_ascii_case(b"<!ENTITY"))
        {
            return true;
        }
        i += 1;
    }
    false
}

fn boundary_tiles(boundary: Node<'_, '_>, name: &str, zoom: u8) -> Result<TileSet> {
    let coordinates = child(boundary, "LinearRing")
        .and_then(|ring| child(ring, "coordinates"))
        .and_then(|coordinates| coordinates.text())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| Error::Validation(format!("KML {name} has no coordinates")))?;
    rasterize_ring(coordinates, zoom, MAX_RASTER_TILES, MAX_SCAN_WORK)
}

pub fn parse_squadrats_kml(path: &Path) -> Result<KmlTileSet> {
    let expanded;
    let path = if let Some(raw) = path.to_str().filter(|raw| raw.starts_with('~')) {
        expanded = expand_tilde(raw);
        &expanded
    } else {
        path
    };
    if !path.is_file() {
        return Err(Error::Validation(format!(
            "KML file does not exist: {}",
            path.display()
        )));
    }
    let file = File::open(path)?;
    let size_error = || Error::Validation("KML file exceeds the 100 MiB safety limit".into());
    if file.metadata()?.len() > MAX_KML_BYTES as u64 {
        return Err(size_error());
    }
    let mut bytes = Vec::new();
    file.take(MAX_KML_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_KML_BYTES {
        return Err(size_error());
    }
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let (encoding, offset) = encoding_rs::Encoding::for_bom(&bytes).unwrap_or_else(|| {
        // UTF-16 XML can be recognized by its initial code units without a BOM;
        // otherwise honor an explicit XML declaration encoding (e.g. ISO-8859-1).
        let encoding = match bytes.as_slice() {
            [0, b'<' | b' ' | b'\n' | b'\r' | b'\t', ..] => UTF_16BE,
            [b'<' | b' ' | b'\n' | b'\r' | b'\t', 0, ..] => UTF_16LE,
            _ => declared_xml_encoding(&bytes).unwrap_or(UTF_8),
        };
        (encoding, 0)
    });
    let xml = encoding
        .decode_without_bom_handling_and_without_replacement(&bytes[offset..])
        .ok_or_else(|| Error::Validation("Invalid KML XML: invalid XML encoding".into()))?;
    if contains_unsafe_declaration(&xml) {
        return Err(Error::Validation(
            "Unsafe KML XML: DTD and entity declarations are forbidden".into(),
        ));
    }
    let document = Document::parse_with_options(
        &xml,
        ParsingOptions {
            allow_dtd: false,
            ..ParsingOptions::default()
        },
    )
    .map_err(|error| {
        let prefix = match error {
            roxmltree::Error::DtdDetected
            | roxmltree::Error::UnexpectedEntityCloseTag(_)
            | roxmltree::Error::UnknownEntityReference(..)
            | roxmltree::Error::MalformedEntityReference(_)
            | roxmltree::Error::EntityReferenceLoop(_) => "Unsafe",
            _ => "Invalid",
        };
        Error::Validation(format!("{prefix} KML XML: {error}"))
    })?;

    let [mut squadrats, mut squadratinhos] = [TileSet::default(), TileSet::default()];
    let mut found = [false; 2];
    for placemark in document
        .descendants()
        .filter(|&node| has_name(node, "Placemark"))
    {
        let name = child(placemark, "name")
            .and_then(|name| name.text())
            .unwrap_or("")
            .trim();
        let (index, zoom, layer) = if name.eq_ignore_ascii_case("squadrats") {
            (0, 14, &mut squadrats)
        } else if name.eq_ignore_ascii_case("squadratinhos") {
            (1, 17, &mut squadratinhos)
        } else {
            continue;
        };
        found[index] = true;
        let mut polygons = placemark
            .descendants()
            .filter(|&node| has_name(node, "Polygon"))
            .peekable();
        if polygons.peek().is_none() {
            return Err(Error::Validation(format!(
                "Squadrats KML layer {name:?} contains no polygons"
            )));
        }
        for polygon in polygons {
            let outer = child(polygon, "outerBoundaryIs")
                .ok_or_else(|| Error::Validation("KML polygon has no outer boundary".into()))?;
            let mut tiles = boundary_tiles(outer, "outer boundary", zoom)?;
            for hole in polygon
                .children()
                .filter(|&node| has_name(node, "innerBoundaryIs"))
            {
                for tile in boundary_tiles(hole, "inner boundary", zoom)? {
                    tiles.remove(&tile);
                }
            }
            if layer.is_empty() {
                *layer = tiles;
                continue;
            }
            for tile in tiles {
                if layer.len() == MAX_RASTER_TILES && !layer.contains(&tile) {
                    return Err(Error::Validation(
                        "KML layer exceeds the rasterized tile limit".into(),
                    ));
                }
                layer.insert(tile);
            }
        }
    }
    match found {
        [false, false] => {
            return Err(Error::Validation(
                "KML contains neither a 'squadrats' nor a 'squadratinhos' layer".into(),
            ))
        }
        [true, true] => {}
        _ => {
            return Err(Error::Validation(
                "KML must include both squadrats and squadratinhos layers".into(),
            ))
        }
    }
    if squadrats.is_empty() {
        return Err(Error::Validation(
            "KML contains no exact Squadrats tiles".into(),
        ));
    }
    Ok(KmlTileSet {
        squadrats: tile_ids(squadrats),
        squadratinhos: tile_ids(squadratinhos),
        sha256,
    })
}

pub fn parse_as_of_utc(value: Option<&str>) -> Result<String> {
    let Some(value) = value else {
        return Ok(Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true));
    };
    let value = value.trim();
    let timestamp = DateTime::parse_from_rfc3339(value).or_else(|_| {
        let normalized = value.strip_suffix('Z').map_or_else(|| value.to_owned(), |v| format!("{v}+00:00"));
        ["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S%.f%z", "%Y-%m-%dT%H:%M%z", "%Y-%m-%d %H:%M%z",
         "%Y-%m-%dT%H%z", "%Y-%m-%d %H%z", "%Y%m%dT%H%M%S%.f%z", "%Y%m%dT%H%M%z", "%Y%m%dT%H%z"]
            .iter()
            .find_map(|format| DateTime::parse_from_str(&normalized, format).ok())
            .ok_or_else(|| Error::Validation("--as-of must be an ISO-8601 timestamp with a timezone, for example 2026-08-01T18:43:00Z".into()))
    })?;
    let precision = if timestamp.timestamp_subsec_micros() == 0 {
        SecondsFormat::Secs
    } else {
        SecondsFormat::Micros
    };
    Ok(timestamp
        .with_timezone(&Utc)
        .to_rfc3339_opts(precision, true))
}

fn require_zoom(settings: &Settings, context: &str) -> Result<()> {
    if (settings.squadrat_zoom, settings.squadratinho_zoom) != (14, 17) {
        return Err(Error::Validation(format!(
            "{context} requires zoom levels 14 and 17"
        )));
    }
    Ok(())
}

pub fn import_baseline(
    settings: &Settings,
    path: &Path,
    as_of: Option<&str>,
) -> Result<ImportBaselineResult> {
    require_zoom(settings, "Squadrats KML import")?;
    let tiles = parse_squadrats_kml(path)?;
    let as_of_utc = parse_as_of_utc(as_of)?;
    let imported_at = parse_as_of_utc(None)?;
    let source_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut conn = get_db(settings)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(existing) = get_baseline(settings, &tx)? {
        return Err(Error::Validation(format!(
            "A KML baseline is already installed from {} at {}; use 'compare-kml' for newer exports",
            existing.source_name, existing.imported_at
        )));
    }
    tx.execute(
        "INSERT INTO baseline_imports
            (id, source_name, sha256, imported_at, as_of_utc, squadrat_count, squadratinho_count)
         VALUES (1, ?, ?, ?, ?, ?, ?)",
        params![
            source_name,
            tiles.sha256,
            imported_at,
            as_of_utc,
            tiles.squadrats.len() as i64,
            tiles.squadratinhos.len() as i64
        ],
    )?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO baseline_tiles (import_id, tile_kind, tile_id) VALUES (1, ?, ?)",
        )?;
        for (kind, layer) in [
            (TileKind::Squadrat, &tiles.squadrats),
            (TileKind::Squadratinho, &tiles.squadratinhos),
        ] {
            for tile in layer {
                insert.execute(params![kind, tile])?;
            }
        }
    }
    tx.execute(
        "UPDATE activities SET baseline_covered = 1
         WHERE status != 'processed' AND julianday(start_utc) <= julianday(?)",
        [&as_of_utc],
    )?;
    let rebuilt = tx.query_row(
        "SELECT COUNT(*) FROM activities WHERE status = 'processed'",
        [],
        |row| row.get(0),
    )?;
    rebuild_tile_history(&tx, None)?;
    tx.commit()?;
    Ok(ImportBaselineResult {
        rebuilt,
        source_name,
        sha256: tiles.sha256,
        imported_at,
        as_of_utc,
        squadrats: tiles.squadrats.len(),
        squadratinhos: tiles.squadratinhos.len(),
    })
}

pub fn get_baseline(settings: &Settings, conn: &Connection) -> Result<Option<BaselineMetadata>> {
    let baseline = conn.query_row(
        "SELECT id, source_name, sha256, imported_at, as_of_utc, squadrat_count, squadratinho_count
         FROM baseline_imports WHERE id = 1",
        [],
        |row| Ok(BaselineMetadata {
            id: row.get(0)?,
            source_name: row.get(1)?,
            sha256: row.get(2)?,
            imported_at: row.get(3)?,
            as_of_utc: row.get(4)?,
            squadrat_count: row.get(5)?,
            squadratinho_count: row.get(6)?,
        }),
    ).optional()?;
    if baseline.is_some() {
        require_zoom(settings, "Installed Squadrats KML baseline")?;
    }
    Ok(baseline)
}

pub fn activity_is_covered(
    settings: &Settings,
    conn: &Connection,
    start_utc: &str,
) -> Result<bool> {
    let Some(baseline) = get_baseline(settings, conn)? else {
        return Ok(false);
    };
    Ok(conn.query_row(
        "SELECT COALESCE(julianday(?) <= julianday(?), 0)",
        params![start_utc, baseline.as_of_utc],
        |row| row.get(0),
    )?)
}

pub fn activity_uses_baseline(
    settings: &Settings,
    conn: &Connection,
    start_utc: &str,
    baseline_covered: bool,
) -> Result<bool> {
    let Some(baseline) = get_baseline(settings, conn)? else {
        return Ok(false);
    };
    uses_baseline(conn, start_utc, baseline_covered, &baseline)
}

fn uses_baseline(
    conn: &Connection,
    start_utc: &str,
    baseline_covered: bool,
    baseline: &BaselineMetadata,
) -> Result<bool> {
    if baseline_covered {
        return Ok(true);
    }
    Ok(conn.query_row(
        "SELECT COALESCE(julianday(?) > julianday(?), 0)",
        params![start_utc, baseline.as_of_utc],
        |row| row.get(0),
    )?)
}

pub fn activity_row_uses_baseline(
    settings: &Settings,
    conn: &Connection,
    row: &ActivityRow,
) -> Result<bool> {
    activity_uses_baseline(settings, conn, &row.start_utc, row.baseline_covered)
}

pub fn baseline_tiles_on_route(
    conn: &Connection,
    tile_kind: &str,
    tiles: &HashSet<String>,
) -> Result<HashSet<String>> {
    let tiles: Vec<_> = tiles.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    for chunk in tiles.chunks(crate::db::SQL_CHUNK_SIZE) {
        seen.extend(crate::db::baseline_tiles_in_chunk(conn, tile_kind, chunk)?);
    }
    Ok(seen)
}

const EFFECTIVE_TILES_SQL: &str = "SELECT tile_id FROM baseline_tiles WHERE tile_kind = ?1
     UNION
     SELECT at.tile_id FROM activity_tiles at JOIN activities a ON a.id = at.activity_id
     WHERE at.tile_kind = ?1 AND a.status = 'processed'
       AND (a.baseline_covered = 1 OR julianday(a.start_utc) > julianday(?2))";

pub fn effective_tiles(
    settings: &Settings,
    conn: &Connection,
    tile_kind: &str,
) -> Result<HashSet<String>> {
    let baseline = get_baseline(settings, conn)?;
    let (sql, args) = match &baseline {
        Some(baseline) => (EFFECTIVE_TILES_SQL, vec![tile_kind, &baseline.as_of_utc]),
        None => (
            "SELECT tile_id FROM global_tiles WHERE tile_kind = ?",
            vec![tile_kind],
        ),
    };
    let mut query = conn.prepare(sql)?;
    let rows = query.query_map(params_from_iter(args), |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn effective_tile_count(settings: &Settings, tile_kind: &str) -> Result<i64> {
    effective_tile_count_with_conn(settings, &get_db(settings)?, tile_kind)
}

pub(crate) fn effective_tile_count_with_conn(
    settings: &Settings,
    conn: &Connection,
    tile_kind: &str,
) -> Result<i64> {
    if let Some(baseline) = get_baseline(settings, conn)? {
        Ok(conn.query_row(
            &format!("SELECT COUNT(*) FROM ({EFFECTIVE_TILES_SQL})"),
            params![tile_kind, baseline.as_of_utc],
            |row| row.get(0),
        )?)
    } else {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM global_tiles WHERE tile_kind = ?",
            [tile_kind],
            |row| row.get(0),
        )?)
    }
}

pub fn effective_tile_count_through(
    settings: &Settings,
    conn: &Connection,
    tile_kind: &str,
    activity_id: i64,
    start_utc: &str,
    baseline_covered: bool,
    include_activity: bool,
) -> Result<i64> {
    const THROUGH: &str = "AND (
          julianday(a.start_utc) < julianday(:start)
          OR (julianday(a.start_utc) = julianday(:start) AND a.id < :id)
          OR (:include AND a.id = :id)
      )";
    if let Some(baseline) = get_baseline(settings, conn)? {
        if uses_baseline(conn, start_utc, baseline_covered, &baseline)? {
            return Ok(conn.query_row(
                &format!("SELECT COUNT(*) FROM ({EFFECTIVE_TILES_SQL} {THROUGH})"),
                params![
                    tile_kind,
                    baseline.as_of_utc,
                    start_utc,
                    activity_id,
                    include_activity
                ],
                |row| row.get(0),
            )?);
        }
    }
    Ok(conn.query_row(
        &format!(
            "SELECT COUNT(DISTINCT at.tile_id)
                  FROM activity_tiles at JOIN activities a ON a.id = at.activity_id
                  WHERE at.tile_kind = ?1 AND a.status = 'processed' {THROUGH}"
        ),
        params![tile_kind, start_utc, activity_id, include_activity],
        |row| row.get(0),
    )?)
}

pub fn compare_kml(settings: &Settings, path: &Path) -> Result<CompareKmlResult> {
    let detailed = compare_kml_detailed(settings, path)?;
    Ok(CompareKmlResult {
        squadrat: detailed.squadrat.counts,
        squadratinho: detailed.squadratinho.counts,
    })
}

pub fn compare_kml_detailed(settings: &Settings, path: &Path) -> Result<DetailedCompareKmlResult> {
    let conn = Connection::open_with_flags(settings.db_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    compare_kml_detailed_with_conn(settings, &conn, path)
}

pub(crate) fn compare_kml_detailed_with_conn(
    settings: &Settings,
    conn: &Connection,
    path: &Path,
) -> Result<DetailedCompareKmlResult> {
    require_zoom(settings, "Squadrats KML comparison")?;
    let exported = parse_squadrats_kml(path)?;
    let tx = conn.unchecked_transaction()?;
    let diff = |kind: TileKind| -> Result<DetailedKmlDiff> {
        let kml = exported.tiles(kind);
        let local = effective_tiles(settings, &tx, kind.as_str())?;
        let mut kml_only_tiles: Vec<_> = kml.difference(&local).cloned().collect();
        let mut local_only_tiles: Vec<_> = local.difference(kml).cloned().collect();
        kml_only_tiles.sort_unstable();
        local_only_tiles.sort_unstable();
        Ok(DetailedKmlDiff {
            counts: KmlDiffCounts {
                kml: kml.len(),
                local: local.len(),
                shared: kml.len() - kml_only_tiles.len(),
                kml_only: kml_only_tiles.len(),
                local_only: local_only_tiles.len(),
            },
            kml_only_tiles,
            local_only_tiles,
        })
    };
    Ok(DetailedCompareKmlResult {
        squadrat: diff(TileKind::Squadrat)?,
        squadratinho: diff(TileKind::Squadratinho)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrate;
    use std::fs;

    const CUTOFF: &str = "2026-08-01T18:43:00Z";

    fn ring_coordinates(points: impl IntoIterator<Item = (i64, i64)>, zoom: u8) -> String {
        use std::fmt::Write as _;

        let width = f64::from(1 << zoom);
        let mut coordinates = String::new();
        for (x, y) in points {
            let lon = x as f64 / width * 360.0 - 180.0;
            let lat = (std::f64::consts::PI * (1.0 - 2.0 * y as f64 / width))
                .sinh()
                .atan()
                .to_degrees();
            write!(coordinates, "{lon:.15},{lat:.15},0 ").unwrap();
        }
        coordinates
    }

    fn ring(left: i64, top: i64, right: i64, bottom: i64, zoom: u8) -> String {
        ring_coordinates(
            [
                (left, top),
                (right, top),
                (right, bottom),
                (left, bottom),
                (left, top),
            ],
            zoom,
        )
    }

    fn snapshot() -> String {
        let boundary = |kind, coordinates| {
            format!(
            "<{kind}><LinearRing><coordinates>{coordinates}</coordinates></LinearRing></{kind}>"
        )
        };
        let outer = boundary("outerBoundaryIs", ring(8000, 6000, 8002, 6002, 14));
        let hole = boundary("innerBoundaryIs", ring(8001, 6001, 8002, 6002, 14));
        let small = boundary("outerBoundaryIs", ring(64000, 48000, 64001, 48001, 17));
        format!(
            r#"<kml xmlns="http://www.opengis.net/kml/2.2">
            <Placemark><name> SquaDRats </name><MultiGeometry>
                <Polygon>{outer}{hole}</Polygon><Polygon>{outer}{hole}</Polygon>
            </MultiGeometry></Placemark>
            <Placemark><name>squadratinhos</name><Polygon>{small}</Polygon></Placemark>
            <Placemark><name>ubersquadrat</name><Polygon/></Placemark>
        </kml>"#
        )
    }

    fn fixture() -> Result<(tempfile::TempDir, Settings, std::path::PathBuf)> {
        let dir = tempfile::tempdir()?;
        let settings = Settings::with_data_dir(dir.path());
        migrate(&settings)?;
        let path = dir.path().join("snapshot.kml");
        fs::write(&path, snapshot())?;
        Ok((dir, settings, path))
    }

    #[test]
    fn rasterizes_dissolved_polygons_and_holes() -> Result<()> {
        let (_dir, _, path) = fixture()?;
        let tiles = parse_squadrats_kml(&path)?;
        assert_eq!(
            tiles.squadrats,
            ["14:8000:6000", "14:8001:6000", "14:8000:6001"]
                .map(String::from)
                .into()
        );
        assert_eq!(
            tiles.squadratinhos,
            HashSet::from(["17:64000:48000".into()])
        );
        assert_eq!(tiles.sha256, hex::encode(Sha256::digest(snapshot())));
        assert!(tiles.for_kind("other").is_err());
        Ok(())
    }

    #[test]
    fn wraps_antimeridian_and_enforces_limits() -> Result<()> {
        let coordinates = ring(16383, 6000, 1, 6001, 14);
        assert_eq!(
            tiles_inside_ring(&coordinates, 14, 2, 100)?,
            ["14:16383:6000", "14:0:6000"].map(String::from).into()
        );
        let large = ring(100, 100, 120, 120, 17);
        assert!(tiles_inside_ring(&large, 17, 10, 1000)
            .unwrap_err()
            .to_string()
            .contains("tile limit"));
        assert!(tiles_inside_ring(&large, 17, 0, 39)
            .unwrap_err()
            .to_string()
            .contains("work limit"));
        Ok(())
    }

    #[test]
    fn parses_large_dissolved_ring_and_rejects_tall_zigzag() -> Result<()> {
        let (left, top, right, bottom) = (64000, 48000, 82001, 50501);
        // A two-row strip with 9,000 one-tile notches and a one-tile-wide stem.
        let notches = (0..9000).flat_map(|i| {
            let x = left + 2 * i + 1;
            [(x, top), (x, top + 1), (x + 1, top + 1), (x + 1, top)]
        });
        let points = [(left, top)].into_iter().chain(notches).chain([
            (right, top),
            (right, top + 2),
            (left + 1, top + 2),
            (left + 1, bottom),
            (left, bottom),
            (left, top),
        ]);
        let coordinates = ring_coordinates(points, 17);
        let (_dir, _, path) = fixture()?;
        fs::write(
            &path,
            snapshot().replace(&ring(64000, 48000, 64001, 48001, 17), &coordinates),
        )?;
        let tiles = parse_squadrats_kml(&path)?.squadratinhos;
        assert_eq!(tiles.len(), 29_501);
        assert!(tiles.contains("17:64000:50500"));
        assert!(tiles.contains("17:82000:48000"));
        assert!(!tiles.contains("17:64001:48000"));

        let zigzag = (0..250).flat_map(|i| [(64000 + 2 * i, 1000), (64001 + 2 * i, 111000)]);
        let coordinates = ring_coordinates(zigzag.chain([(64000, 1000)]), 17);
        assert!(
            tiles_inside_ring(&coordinates, 17, MAX_RASTER_TILES, MAX_SCAN_WORK)
                .unwrap_err()
                .to_string()
                .contains("work limit")
        );
        Ok(())
    }

    #[test]
    fn rejects_dtd_and_entities_in_utf8_and_utf16() -> Result<()> {
        let (_dir, _, path) = fixture()?;
        for xml in [
            "<?xml version=\"1.0\" encoding=\"UTF-16\"?><!DOCTYPE kml [<!ENTITY x 'small'>]><kml>&x;</kml>",
            "<!eNtItY x 'small'><kml/>",
            "<kml>&unknown;</kml>",
        ] {
            let mut le = vec![0xff, 0xfe];
            let mut be = vec![0xfe, 0xff];
            for unit in xml.encode_utf16() {
                le.extend(unit.to_le_bytes());
                be.extend(unit.to_be_bytes());
            }
            for payload in [xml.as_bytes(), &le, &be, &le[2..], &be[2..]] {
                fs::write(&path, payload)?;
                assert!(parse_squadrats_kml(&path).unwrap_err().to_string().starts_with("Unsafe KML XML:"));
            }
        }
        let xml = format!("<?xml version=\"1.0\" encoding=\"UTF-16\"?>{}", snapshot());
        let utf16: Vec<_> = xml.encode_utf16().flat_map(u16::to_le_bytes).collect();
        fs::write(&path, &utf16)?;
        assert_eq!(
            parse_squadrats_kml(&path)?.sha256,
            hex::encode(Sha256::digest(&utf16))
        );
        Ok(())
    }

    #[test]
    fn rejects_incomplete_and_invalid_xml() -> Result<()> {
        let (_dir, _, path) = fixture()?;
        for (xml, message) in [
            (snapshot().replace(">squadratinhos<", ">other<"), "both"),
            ("<kml/>".into(), "neither"),
            ("<kml>".into(), "Invalid KML XML:"),
        ] {
            fs::write(&path, xml)?;
            assert!(parse_squadrats_kml(&path)
                .unwrap_err()
                .to_string()
                .contains(message));
        }
        Ok(())
    }

    #[test]
    fn normalizes_cutoff_and_checks_zoom() -> Result<()> {
        assert_eq!(
            parse_as_of_utc(Some(" 2026-08-01T20:43:00+02:00 "))?,
            CUTOFF
        );
        assert_eq!(parse_as_of_utc(Some("2026-08-01T18:43Z"))?, CUTOFF);
        assert!(parse_as_of_utc(Some("2026-08-01T18:43:00"))
            .unwrap_err()
            .to_string()
            .contains("timezone"));
        let (_dir, mut settings, path) = fixture()?;
        settings.squadrat_zoom = 13;
        let conn = get_db(&settings)?;
        assert_eq!(get_baseline(&settings, &conn)?, None);
        assert!(import_baseline(&settings, &path, None)
            .unwrap_err()
            .to_string()
            .contains("zoom"));
        assert!(compare_kml(&settings, &path)
            .unwrap_err()
            .to_string()
            .contains("zoom"));
        settings.squadrat_zoom = 14;
        import_baseline(&settings, &path, Some(CUTOFF))?;
        settings.squadratinho_zoom = 16;
        assert!(get_baseline(&settings, &conn)
            .unwrap_err()
            .to_string()
            .contains("zoom"));
        Ok(())
    }

    fn activity(conn: &Connection, id: i64, instant: &str, status: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO activities (id, start_utc, start_local, status) VALUES (?1, ?2, ?2, ?3)",
            params![id, instant, status],
        )?;
        Ok(())
    }

    #[test]
    fn restores_all_tables_when_rebuild_fails() -> Result<()> {
        let (_dir, settings, path) = fixture()?;
        let conn = get_db(&settings)?;
        activity(&conn, 1, CUTOFF, "pending")?;
        conn.execute_batch(
            "INSERT INTO global_tiles VALUES ('squadrat', '14:1:2', 1, 'now');
             CREATE TRIGGER fail_rebuild BEFORE UPDATE OF new_squadrat_count ON activities
             BEGIN SELECT RAISE(ABORT, 'rebuild failure'); END;",
        )?;
        let error = import_baseline(&settings, &path, Some(CUTOFF)).unwrap_err();
        assert!(error.to_string().contains("rebuild failure"));
        assert!(get_baseline(&settings, &conn)?.is_none());
        let counts = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM baseline_tiles),
                    (SELECT baseline_covered FROM activities WHERE id = 1),
                    (SELECT COUNT(*) FROM global_tiles)",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        assert_eq!(counts, (0, 0, 1));
        Ok(())
    }

    #[test]
    fn imports_once_and_applies_cutoff_to_effective_history() -> Result<()> {
        let (_dir, settings, path) = fixture()?;
        let conn = get_db(&settings)?;
        activity(&conn, 1, CUTOFF, "processed")?;
        activity(&conn, 2, "2026-08-01T20:43:00+02:00", "pending")?;
        activity(&conn, 3, "2026-08-02T00:00:00Z", "failed")?;
        conn.execute_batch(
            "INSERT INTO activity_tiles (activity_id, tile_kind, tile_id) VALUES
             (1, 'squadrat', '14:1:1'), (2, 'squadrat', '14:2:2'), (3, 'squadrat', '14:3:3');",
        )?;
        let imported = import_baseline(&settings, &path, Some(CUTOFF))?;
        assert_eq!(
            (imported.rebuilt, imported.squadrats, imported.squadratinhos),
            (1, 3, 1)
        );
        let mut stmt = conn.prepare("SELECT baseline_covered FROM activities ORDER BY id")?;
        let flags = stmt
            .query_map([], |row| row.get::<_, bool>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(flags, [false, true, false]);
        assert!(import_baseline(&settings, &path, None)
            .unwrap_err()
            .to_string()
            .contains("already installed"));
        assert!(activity_is_covered(
            &settings,
            &conn,
            "2026-08-01T20:43:00+02:00"
        )?);
        assert!(!activity_is_covered(&settings, &conn, "invalid")?);
        assert!(!activity_uses_baseline(&settings, &conn, CUTOFF, false)?);
        assert!(activity_uses_baseline(&settings, &conn, CUTOFF, true)?);
        assert!(activity_uses_baseline(
            &settings,
            &conn,
            "2026-08-02T00:00:00Z",
            false
        )?);
        assert_eq!(effective_tile_count(&settings, "squadrat")?, 3);
        conn.execute(
            "UPDATE activities SET status = 'processed' WHERE id = 2",
            [],
        )?;
        assert_eq!(effective_tile_count(&settings, "squadrat")?, 4);
        assert_eq!(
            effective_tile_count_through(&settings, &conn, "squadrat", 1, CUTOFF, false, true)?,
            1
        );
        assert_eq!(
            effective_tile_count_through(&settings, &conn, "squadrat", 2, CUTOFF, true, false)?,
            3
        );
        assert_eq!(
            effective_tile_count_through(&settings, &conn, "squadrat", 2, CUTOFF, true, true)?,
            4
        );
        assert_eq!(
            effective_tile_count_through(
                &settings,
                &conn,
                "squadrat",
                99,
                "2026-08-02T00:00:00Z",
                false,
                false
            )?,
            4
        );
        let route = (0..510)
            .map(|x| format!("14:{x}:1"))
            .chain(["14:8000:6000".into()])
            .collect();
        assert_eq!(
            baseline_tiles_on_route(&conn, "squadrat", &route)?,
            HashSet::from(["14:8000:6000".into()])
        );
        assert_eq!(
            compare_kml(&settings, &path)?,
            CompareKmlResult {
                squadrat: KmlDiffCounts {
                    kml: 3,
                    local: 4,
                    shared: 3,
                    kml_only: 0,
                    local_only: 1
                },
                squadratinho: KmlDiffCounts {
                    kml: 1,
                    local: 1,
                    shared: 1,
                    kml_only: 0,
                    local_only: 0
                },
            },
        );
        assert_eq!(
            get_baseline(&settings, &conn)?.unwrap().sha256,
            imported.sha256
        );
        Ok(())
    }
}
