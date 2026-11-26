//! Squadrats-compatible Mapbox tile computation.

use std::{
    collections::HashSet,
    f64::consts::PI,
    hash::{BuildHasherDefault, Hasher},
    sync::LazyLock,
};

use regex::Regex;

use crate::{
    config::Settings,
    error::{Error, Result},
};

pub const EARTH_RADIUS: f64 = 6_378_137.0;
pub const MAX_EXTENT: f64 = PI * EARTH_RADIUS;
pub const MAX_LATITUDE: f64 = 85.0511287798066;

pub fn validate_tile_id(tile_id: &str) -> Result<String> {
    static TILE_ID: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\d+:\d+(?::\d+)?\n?\z").unwrap());
    if TILE_ID.is_match(tile_id) {
        Ok(tile_id.to_owned())
    } else {
        Err(Error::Validation(format!(
            "Invalid tile_id format: {tile_id:?}"
        )))
    }
}

pub fn latlon_to_meters(lat: f64, lon: f64) -> (f64, f64) {
    (
        EARTH_RADIUS * lon.to_radians(),
        EARTH_RADIUS * (PI / 4.0 + lat.to_radians() / 2.0).tan().ln(),
    )
}

pub fn meters_to_tile(x: f64, y: f64, tile_size: f64) -> Result<String> {
    let tx = ((x + MAX_EXTENT) / tile_size).floor();
    let ty = ((y + MAX_EXTENT) / tile_size).floor();
    // Python's floor produces integer zero even when the result is signed zero.
    validate_tile_id(&format!("{:.0}:{:.0}", tx + 0.0, ty + 0.0))
}

pub trait TileEngine {
    fn id(&self) -> &str;
    fn version(&self) -> &str;
    fn tiles_for_points(&self, points: &[(f64, f64)])
        -> Result<(HashSet<String>, HashSet<String>)>;
    fn tiles_for_segments(
        &self,
        segments: &[Vec<(f64, f64)>],
    ) -> Result<(HashSet<String>, HashSet<String>)>;
}

#[derive(Debug, Clone, Copy)]
pub struct SquadratsEngine {
    pub squadrat_zoom: u8,
    pub squadratinho_zoom: u8,
}

impl SquadratsEngine {
    pub const ID: &'static str = "mapbox-z14-z17";
    pub const VERSION: &'static str = "2";

    pub fn new(squadrat_zoom: u8, squadratinho_zoom: u8) -> Self {
        Self {
            squadrat_zoom,
            squadratinho_zoom,
        }
    }

    /// Traverse a segment whose endpoints are in continuous XYZ grid coordinates.
    pub fn tiles_for_segment(
        &self,
        start: (f64, f64),
        end: (f64, f64),
        zoom: u8,
    ) -> Result<HashSet<String>> {
        let width = grid_width(zoom)?;
        for (x, y) in [start, end] {
            let margin = width * 1e-10;
            if !(0.0..=width).contains(&x) || !(-margin..=width + margin).contains(&y) {
                return Err(Error::Validation(format!(
                    "Invalid grid coordinate: {x}, {y}"
                )));
            }
        }
        let mut tiles = TileSet::default();
        Self::walk_segment(clamp_y(start, width), clamp_y(end, width), zoom, &mut tiles);
        Ok(tile_ids(tiles))
    }

    fn walk_segment(mut start: (f64, f64), mut end: (f64, f64), zoom: u8, tiles: &mut TileSet) {
        if start > end {
            std::mem::swap(&mut start, &mut end);
        }
        let n = 1_i64 << zoom;
        if end.0 - start.0 > n as f64 / 2.0 {
            end.0 -= n as f64;
        }

        let (mut x, mut y) = cell(start);
        let (end_x, end_y) = cell(end);
        let insert = |tiles: &mut TileSet, x: i64, y: i64| {
            tiles.insert(TileCoord {
                zoom,
                x: x.rem_euclid(n) as u64,
                y: y as u64,
            });
        };
        insert(tiles, x, y);
        if (x, y) == (end_x, end_y) {
            return;
        }
        insert(tiles, end_x, end_y);

        let dx = end.0 - start.0;
        let dy = end.1 - start.1;
        let step_x = if dx < 0.0 { -1 } else { 1 };
        let step_y = if dy < 0.0 { -1 } else { 1 };
        let offset_x = step_x.max(0);
        let offset_y = step_y.max(0);
        let crossing_x = |x| ((x + offset_x) as f64 - start.0) / dx;
        let crossing_y = |y| ((y + offset_y) as f64 - start.1) / dy;
        let mut tx = if x == end_x {
            f64::INFINITY
        } else {
            crossing_x(x)
        };
        let mut ty = if y == end_y {
            f64::INFINITY
        } else {
            crossing_y(y)
        };
        let mut enter = 0.0;
        while (x, y) != (end_x, end_y) {
            let leave = tx.min(ty);
            if enter < leave.min(1.0) {
                insert(tiles, x, y);
            }
            if leave >= 1.0 {
                break;
            }
            if tx == leave {
                x += step_x;
                tx = if x == end_x {
                    f64::INFINITY
                } else {
                    crossing_x(x)
                };
            }
            if ty == leave {
                y += step_y;
                ty = if y == end_y {
                    f64::INFINITY
                } else {
                    crossing_y(y)
                };
            }
            enter = leave;
        }
    }

    fn compute_tiles<'a>(
        segments: impl IntoIterator<Item = &'a [(f64, f64)]>,
        zoom: u8,
    ) -> Result<TileSet> {
        let width = grid_width(zoom)?;
        let mut tiles = TileSet::default();
        for segment in segments {
            let mut previous: Option<(f64, f64)> = None;
            let mut last_cell = None;
            for &(lat, lon) in segment {
                let current = clamp_y(project(lat, lon, width)?, width);
                let current_cell = cell(current);
                if let Some(previous) = previous {
                    // The preceding endpoint already owns this cell. A segment
                    // between points in it stays there unless it wraps.
                    if last_cell != Some(current_cell)
                        || (current.0 - previous.0).abs() > width / 2.0
                    {
                        Self::walk_segment(previous, current, zoom, &mut tiles);
                    }
                } else {
                    tiles.insert(TileCoord {
                        zoom,
                        x: current_cell.0.rem_euclid(width as i64) as u64,
                        y: current_cell.1 as u64,
                    });
                }
                previous = Some(current);
                last_cell = Some(current_cell);
            }
        }
        Ok(tiles)
    }

    fn compute_both<'a>(
        &self,
        segments: impl IntoIterator<Item = &'a [(f64, f64)]> + Clone,
    ) -> Result<(HashSet<String>, HashSet<String>)> {
        let large_zoom = self.squadrat_zoom;
        let small_zoom = self.squadratinho_zoom;
        let small = Self::compute_tiles(segments.clone(), small_zoom)?;
        let large = if large_zoom < small_zoom {
            let shift = small_zoom - large_zoom;
            small
                .iter()
                .map(|tile| TileCoord {
                    zoom: large_zoom,
                    x: tile.x >> shift,
                    y: tile.y >> shift,
                })
                .collect()
        } else {
            Self::compute_tiles(segments, large_zoom)?
        };
        Ok((tile_ids(large), tile_ids(small)))
    }
}

fn grid_width(zoom: u8) -> Result<f64> {
    if zoom > 52 {
        return Err(Error::Validation(format!("Unsupported tile zoom: {zoom}")));
    }
    Ok((1_u64 << zoom) as f64)
}

fn clamp_y((x, y): (f64, f64), width: f64) -> (f64, f64) {
    (x, y.clamp(0.0, f64::from_bits(width.to_bits() - 1)))
}

fn cell((x, y): (f64, f64)) -> (i64, i64) {
    (x.floor() as i64, y.floor() as i64)
}

impl TileEngine for SquadratsEngine {
    fn id(&self) -> &str {
        Self::ID
    }

    fn version(&self) -> &str {
        Self::VERSION
    }

    fn tiles_for_points(
        &self,
        points: &[(f64, f64)],
    ) -> Result<(HashSet<String>, HashSet<String>)> {
        self.compute_both([points])
    }

    fn tiles_for_segments(
        &self,
        segments: &[Vec<(f64, f64)>],
    ) -> Result<(HashSet<String>, HashSet<String>)> {
        self.compute_both(segments.iter().map(Vec::as_slice))
    }
}

pub fn make_engine(settings: &Settings) -> SquadratsEngine {
    SquadratsEngine::new(settings.squadrat_zoom, settings.squadratinho_zoom)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileCoord {
    pub zoom: u8,
    pub x: u64,
    pub y: u64,
}

impl std::fmt::Display for TileCoord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.zoom, self.x, self.y)
    }
}

#[derive(Default)]
pub(crate) struct FxHasher(u64);

impl Hasher for FxHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.write_u64(u64::from_le_bytes(chunk.try_into().unwrap()));
        }
        for &byte in chunks.remainder() {
            self.write_u8(byte);
        }
    }

    fn write_u8(&mut self, value: u8) {
        self.write_u64(u64::from(value));
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(5) ^ value).wrapping_mul(0x517cc1b727220a95);
    }
}

pub(crate) type FxBuildHasher = BuildHasherDefault<FxHasher>;
pub(crate) type TileSet = HashSet<TileCoord, FxBuildHasher>;

pub(crate) fn tile_ids(tiles: TileSet) -> HashSet<String> {
    tiles.into_iter().map(|tile| tile.to_string()).collect()
}

pub fn continuous_tile(lat: f64, lon: f64, zoom: u8) -> Result<(f64, f64)> {
    project(lat, lon, 2_f64.powi(i32::from(zoom)))
}

pub(crate) fn project(lat: f64, lon: f64, width: f64) -> Result<(f64, f64)> {
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(Error::Validation(format!(
            "Invalid GPS coordinate: {lat}, {lon}"
        )));
    }
    let lat = lat.clamp(-MAX_LATITUDE, MAX_LATITUDE).to_radians();
    let x = (lon + 180.0) / 360.0 * width;
    let y = (1.0 - lat.tan().asinh() / PI) / 2.0 * width;
    Ok((x, y))
}
#[cfg(test)]
mod tests {

    #[test]
    fn tile_coord_displays_xyz_grid_id() {
        let coord = TileCoord {
            zoom: 14,
            x: 12,
            y: 34,
        };
        assert_eq!(coord.to_string(), "14:12:34");
        assert_eq!(HashSet::from([coord, coord]).len(), 1);
    }

    use super::*;

    #[test]
    fn validate_tile_formats_and_convert_meters() -> Result<()> {
        for id in ["1:2", "14:123:456", "١:٢", "1:2\n"] {
            assert_eq!(validate_tile_id(id)?, id);
        }
        for id in ["", "-1:2", " 1:2", "1:2:3:4", "1:2\n\n"] {
            assert!(matches!(validate_tile_id(id), Err(Error::Validation(_))));
        }
        let (x, y) = latlon_to_meters(0.0, 1.0);
        assert!((x - 111_319.49).abs() < 0.01);
        assert!(y.abs() < 1e-6);
        assert_eq!(meters_to_tile(0.0, 0.0, MAX_EXTENT)?, "1:1");
        assert!(meters_to_tile(0.0, 0.0, 0.0).is_err());
        Ok(())
    }

    #[test]
    fn points_are_deduplicated_and_segments_remain_disjoint() -> Result<()> {
        let engine = make_engine(&Settings::default());
        assert_eq!(engine.id(), SquadratsEngine::ID);
        assert_eq!(engine.version(), SquadratsEngine::VERSION);
        let (large, small) = engine.tiles_for_points(&[(0.0, 0.0); 2])?;
        assert_eq!(large, HashSet::from(["14:8192:8192".into()]));
        assert_eq!(small, HashSet::from(["17:65536:65536".into()]));
        let (large, small) =
            engine.tiles_for_segments(&[vec![(0.0, 0.0)], vec![], vec![(0.0, 0.1)]])?;
        assert_eq!((large.len(), small.len()), (2, 2));
        let (large, small) = engine.tiles_for_points(&[(0.0, 0.0), (0.0, 0.1)])?;
        assert!(large.len() > 2 && small.len() > large.len() && small.len() <= large.len() * 12);
        for point in [
            (f64::NAN, 0.0),
            (0.0, f64::INFINITY),
            (91.0, 0.0),
            (0.0, -181.0),
        ] {
            assert!(engine.tiles_for_points(&[point]).is_err());
        }
        assert!(engine
            .tiles_for_points(&[(90.0, 180.0), (85.0, 179.9)])
            .is_ok());
        Ok(())
    }

    #[test]
    fn exact_grid_endpoints_terminate_in_both_directions() -> Result<()> {
        let engine = SquadratsEngine::new(14, 17);
        for (start, end, other_tile) in [
            ((1.5, 1.5), (1.0, 2.0), "14:1:2"),
            ((1.5, 1.5), (2.0, 1.0), "14:2:1"),
            ((1.0, 2.0), (1.5, 1.5), "14:1:2"),
        ] {
            assert_eq!(
                engine.tiles_for_segment(start, end, 14)?,
                HashSet::from(["14:1:1".into(), other_tile.into()])
            );
        }
        Ok(())
    }

    #[test]
    fn exact_diagonals_and_seam_match_oracle_in_both_directions() -> Result<()> {
        let engine = SquadratsEngine::new(14, 17);
        let n = 131_072.0;
        let diagonal = (0..24)
            .map(|i| format!("17:{}:{}", 800 + i, 823 - i))
            .chain(["17:824:800".into(), "17:800:824".into()])
            .collect::<HashSet<_>>();
        let seam = (800..=824)
            .map(|y| format!("17:0:{y}"))
            .collect::<HashSet<_>>();
        let inward = (0..24)
            .map(|i| format!("17:{}:{}", 131_071 - i, 800 + i))
            .chain(["17:0:800".into(), "17:131048:824".into()])
            .collect::<HashSet<_>>();
        for (start, end, expected) in [
            ((824.0, 800.0), (800.0, 824.0), &diagonal),
            ((0.0, 800.0), (n, 824.0), &seam),
            ((n, 800.0), (n, 824.0), &seam),
            ((n, 800.0), (n - 24.0, 824.0), &inward),
        ] {
            assert_eq!(&engine.tiles_for_segment(start, end, 17)?, expected);
            assert_eq!(&engine.tiles_for_segment(end, start, 17)?, expected);
        }
        Ok(())
    }

    #[test]
    fn antimeridian_crossing_uses_both_edges_in_either_direction() -> Result<()> {
        let engine = SquadratsEngine::new(14, 17);
        for (start, end) in [(179.9, -179.9), (-179.9, 179.9)] {
            let (large, small) = engine.tiles_for_points(&[(0.0, start), (0.0, end)])?;
            assert!(large.len() < 100 && small.len() < 500);
            assert!(large.contains("14:0:8192"));
            assert!(large.contains("14:16383:8192"));
        }
        Ok(())
    }
}
