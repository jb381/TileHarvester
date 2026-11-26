//! Sparse spatial metrics for the Squadrats tile grids.

use std::{
    collections::{HashMap, HashSet},
    f64::consts::PI,
};

use crate::tile_engine::{FxBuildHasher, TileCoord};

pub fn parse_tile_coord(tile_id: &str) -> Option<TileCoord> {
    let (zoom, xy) = tile_id.split_once(':')?;
    let (x, y) = xy.split_once(':')?;
    if [zoom, x, y]
        .iter()
        .any(|field| field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    Some(TileCoord {
        zoom: zoom.parse().ok()?,
        x: x.parse().ok()?,
        y: y.parse().ok()?,
    })
}

pub fn tile_center_lat_lon(coord: TileCoord) -> (f64, f64) {
    let width = 2_f64.powi(i32::from(coord.zoom));
    let latitude = (PI * (1.0 - 2.0 * (coord.y as f64 + 0.5) / width))
        .sinh()
        .atan()
        .to_degrees();
    let longitude = 360.0 * (coord.x as f64 + 0.5) / width - 180.0;
    (latitude, longitude)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GridMetrics {
    pub total: usize,
    pub yard: usize,
    pub ubersquadrat: usize,
    pub ubersquadrat_origin: Option<(u64, u64)>,
}

pub fn compute_grid_metrics<'a>(tiles: impl IntoIterator<Item = &'a str>, zoom: u8) -> GridMetrics {
    let coords = tiles
        .into_iter()
        .filter_map(parse_tile_coord)
        .filter(|coord| coord.zoom == zoom)
        .map(|coord| (coord.x, coord.y))
        .collect();
    compute_grid_metrics_from_coords(&coords, zoom)
}

fn neighbors(x: u64, y: u64, width: u64) -> [(u64, u64); 4] {
    [
        ((x + width - 1) % width, y),
        ((x + 1) % width, y),
        (x, y - 1),
        (x, y + 1),
    ]
}

// The coordinate-set API uses the tile engine’s crate-private hasher.
#[allow(private_interfaces)]
pub fn compute_grid_metrics_from_coords(
    coords: &HashSet<(u64, u64), FxBuildHasher>,
    zoom: u8,
) -> GridMetrics {
    let Some(width) = 1_u64.checked_shl(u32::from(zoom)) else {
        return GridMetrics::default();
    };
    let mut cells = Vec::with_capacity(coords.len());
    let mut yard = HashSet::<_, FxBuildHasher>::default();
    let mut touches_west = false;
    let mut touches_east = false;
    for &(x, y) in coords {
        if x >= width || y >= width {
            continue;
        }
        cells.push((y, x));
        touches_west |= x == 0;
        touches_east |= x == width - 1;
        if y > 0 && y + 1 < width && neighbors(x, y, width).iter().all(|n| coords.contains(n)) {
            yard.insert((x, y));
        }
    }

    let mut metrics = GridMetrics {
        total: cells.len(),
        ..GridMetrics::default()
    };
    let mut stack = Vec::new();
    // Seed from the original cells so selecting successive components stays linear.
    for &(y, x) in &cells {
        if !yard.remove(&(x, y)) {
            continue;
        }
        let mut size = 0;
        stack.push((x, y));
        while let Some((x, y)) = stack.pop() {
            size += 1;
            stack.extend(
                neighbors(x, y, width)
                    .into_iter()
                    .filter(|n| yard.remove(n)),
            );
        }
        metrics.yard = metrics.yard.max(size);
    }

    if width > 1 && touches_west && touches_east {
        // Only a row's contiguous prefix can extend a square across the seam.
        for index in 0..metrics.total {
            let (y, x) = cells[index];
            if x == 0 {
                for prefix_x in 0..width {
                    if !coords.contains(&(prefix_x, y)) {
                        break;
                    }
                    cells.push((y, width + prefix_x));
                }
            }
        }
    }

    cells.sort_unstable();
    let mut sizes = HashMap::<_, u64, FxBuildHasher>::with_capacity_and_hasher(
        cells.len(),
        FxBuildHasher::default(),
    );
    for (y, x) in cells {
        let size = if x == 0 || y == 0 {
            1
        } else {
            let west = sizes.get(&(y, x - 1)).copied().unwrap_or_default();
            let north = sizes.get(&(y - 1, x)).copied().unwrap_or_default();
            let northwest = sizes.get(&(y - 1, x - 1)).copied().unwrap_or_default();
            (1 + west.min(north).min(northwest)).min(width)
        };
        sizes.insert((y, x), size);
        let origin = ((x - (size - 1)) % width, y - (size - 1));
        let size = size as usize;
        if size > metrics.ubersquadrat
            || (size == metrics.ubersquadrat && Some(origin) < metrics.ubersquadrat_origin)
        {
            metrics.ubersquadrat = size;
            metrics.ubersquadrat_origin = Some(origin);
        }
    }
    metrics
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rectangle(x: u64, y: u64, w: u64, h: u64) -> impl Iterator<Item = (u64, u64)> {
        (y..y + h).flat_map(move |y| (x..x + w).map(move |x| (x, y)))
    }

    #[test]
    fn solid_and_adjacent_squares() {
        for (w, h, yard, side) in [(3, 3, 1, 3), (4, 4, 4, 4), (5, 3, 3, 3), (4, 3, 2, 3)] {
            let cells = rectangle(10, 20, w, h).collect();
            assert_eq!(
                compute_grid_metrics_from_coords(&cells, 14),
                GridMetrics {
                    total: (w * h) as usize,
                    yard,
                    ubersquadrat: side,
                    ubersquadrat_origin: Some((10, 20)),
                }
            );
        }
    }

    #[test]
    fn hole_leaves_four_isolated_interior_tiles() {
        let cells = rectangle(10, 20, 5, 5)
            .filter(|cell| *cell != (12, 22))
            .collect();
        let metrics = compute_grid_metrics_from_coords(&cells, 14);
        assert_eq!(
            (metrics.total, metrics.yard, metrics.ubersquadrat),
            (24, 1, 2)
        );
    }

    #[test]
    fn diagonal_contact_and_l_shape() {
        let cells = rectangle(10, 20, 3, 3)
            .chain(rectangle(13, 23, 3, 3))
            .collect();
        let metrics = compute_grid_metrics_from_coords(&cells, 14);
        assert_eq!(
            (metrics.total, metrics.yard, metrics.ubersquadrat),
            (18, 1, 3)
        );
        let cells = rectangle(10, 20, 3, 1)
            .chain(rectangle(10, 21, 1, 2))
            .collect();
        let metrics = compute_grid_metrics_from_coords(&cells, 14);
        assert_eq!(
            (metrics.total, metrics.yard, metrics.ubersquadrat),
            (5, 0, 1)
        );
    }

    #[test]
    fn wraps_longitude_but_not_latitude() {
        for zoom in [14, 17] {
            let width = 1 << zoom;
            let cells = rectangle(width - 2, 20, 4, 4)
                .map(|(x, y)| (x % width, y))
                .collect();
            assert_eq!(
                compute_grid_metrics_from_coords(&cells, zoom),
                GridMetrics {
                    total: 16,
                    yard: 4,
                    ubersquadrat: 4,
                    ubersquadrat_origin: Some((width - 2, 20)),
                }
            );
            let cells = rectangle(20, width - 2, 4, 4)
                .map(|(x, y)| (x, y % width))
                .collect();
            let metrics = compute_grid_metrics_from_coords(&cells, zoom);
            assert_eq!((metrics.yard, metrics.ubersquadrat), (0, 2));
        }
        let cells = rectangle(0, 0, 2, 2).collect();
        assert_eq!(compute_grid_metrics_from_coords(&cells, 1).ubersquadrat, 2);
    }

    #[test]
    fn ten_thousand_squadratinhos() {
        let cells = rectangle(60_000, 61_000, 100, 100).collect();
        let metrics = compute_grid_metrics_from_coords(&cells, 17);
        assert_eq!(
            (metrics.total, metrics.yard, metrics.ubersquadrat),
            (10_000, 9_604, 100)
        );
    }

    #[test]
    fn ids_are_filtered_and_tile_centers_round_trip() {
        let metrics = compute_grid_metrics(
            [
                "14:10:20",
                "14:010:20",
                "14:16384:0",
                "17:10:20",
                "14:+10:20",
                "14:10:20:1",
            ],
            14,
        );
        assert_eq!(metrics.total, 1);
        let coord = parse_tile_coord("14:10:20").unwrap();
        let (lat, lon) = tile_center_lat_lon(coord);
        let (x, y) = crate::tile_engine::continuous_tile(lat, lon, 14).unwrap();
        assert!((x - 10.5).abs() < 1e-8 && (y - 20.5).abs() < 1e-8);
    }
}
