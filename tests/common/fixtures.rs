//! Deterministic geographic fixtures shared by tests and benchmarks.
#![allow(dead_code)]

use std::collections::HashSet;
use std::f64::consts::PI;
use std::fmt::Write;

use serde_json::{json, Value};
use tileharvester::tile_engine::{SquadratsEngine, TileCoord};

const EARTH_RADIUS_M: f64 = 6_378_137.0;
type Point = (f64, f64);
type Rect = (u64, u64, u64, u64);

#[derive(Debug, Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        value ^ (value >> 31)
    }

    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1_u64 << 53) as f64
    }

    pub fn range_f64(&mut self, min: f64, max: f64) -> f64 {
        min + (max - min) * self.next_f64()
    }

    /// Approximate standard normal with mean zero and variance one.
    pub fn gaussian_approx(&mut self) -> f64 {
        (0..12).map(|_| self.next_f64()).sum::<f64>() - 6.0
    }
}

fn width(zoom: u8) -> f64 {
    assert!(zoom <= 52, "tile grid exceeds f64 resolution");
    2_f64.powi(i32::from(zoom))
}

fn lon_lat(x: f64, y: f64, zoom: u8) -> Point {
    let n = width(zoom);
    (
        x / n * 360.0 - 180.0,
        (PI * (1.0 - 2.0 * y / n)).sinh().atan().to_degrees(),
    )
}

/// Returns the northwest and southeast corners as (longitude, latitude).
pub fn tile_bounds_lon_lat(tx: u64, ty: u64, zoom: u8) -> (Point, Point) {
    (
        lon_lat(tx as f64, ty as f64, zoom),
        lon_lat(tx as f64 + 1.0, ty as f64 + 1.0, zoom),
    )
}

pub fn tile_center_lat_lon(tx: u64, ty: u64, zoom: u8) -> Point {
    let (lon, lat) = lon_lat(tx as f64 + 0.5, ty as f64 + 0.5, zoom);
    (lat, lon)
}

/// Clip the segment's parameter interval against the two cell slabs.
/// Positive-length overlaps count. On a grid line only the owning side counts.
fn overlaps_cell(start: Point, end: Point, x: i64, y: i64) -> bool {
    let mut enter: f64 = 0.0;
    let mut leave: f64 = 1.0;
    for (start, end, low) in [(start.0, end.0, x as f64), (start.1, end.1, y as f64)] {
        let high = low + 1.0;
        let delta = end - start;
        if delta == 0.0 {
            if !(low..high).contains(&start) {
                return false;
            }
        } else {
            let a = (low - start) / delta;
            let b = (high - start) / delta;
            enter = enter.max(a.min(b));
            leave = leave.min(a.max(b));
        }
    }
    enter < leave
}

/// Scan the bounding box in an unwrapped grid and wrap accepted cells at the seam.
pub fn exact_oracle_tiles_for_segment(
    mut start_grid: Point,
    mut end_grid: Point,
    zoom: u8,
) -> HashSet<TileCoord> {
    let n = width(zoom);
    for point in [&mut start_grid, &mut end_grid] {
        assert!(point.0.is_finite() && point.1.is_finite());
        point.1 = point.1.clamp(0.0, f64::from_bits(n.to_bits() - 1));
    }
    // Keep the arithmetic identical when the endpoints are supplied in reverse.
    if start_grid > end_grid {
        std::mem::swap(&mut start_grid, &mut end_grid);
    }
    let dx = end_grid.0 - start_grid.0;
    if dx.abs() > n / 2.0 {
        end_grid.0 -= dx.signum() * n;
    }

    let tile = |x: i64, y: i64| TileCoord {
        zoom,
        x: x.rem_euclid(n as i64) as u64,
        y: y as u64,
    };
    let cell = |value: f64| value.floor() as i64;
    let (x0, y0) = (cell(start_grid.0), cell(start_grid.1));
    let (x1, y1) = (cell(end_grid.0), cell(end_grid.1));
    let mut tiles = HashSet::from([tile(x0, y0), tile(x1, y1)]);
    for y in y0.min(y1)..=y0.max(y1) {
        for x in x0.min(x1)..=x0.max(x1) {
            if overlaps_cell(start_grid, end_grid, x, y) {
                tiles.insert(tile(x, y));
            }
        }
    }
    tiles
}

/// Project geographic (latitude, longitude) segments independently of the engine.
pub fn exact_oracle_tiles_for_segments(segments: &[Vec<Point>], zoom: u8) -> HashSet<TileCoord> {
    let n = width(zoom);
    let project = |(lat, lon): Point| {
        let max_lat = PI.sinh().atan().to_degrees();
        let mercator = lat.clamp(-max_lat, max_lat).to_radians().tan().asinh();
        ((lon + 180.0) / 360.0 * n, (1.0 - mercator / PI) * n / 2.0)
    };
    let mut tiles = HashSet::new();
    for segment in segments {
        if let [point] = segment.as_slice() {
            tiles.extend(exact_oracle_tiles_for_segment(
                project(*point),
                project(*point),
                zoom,
            ));
        }
        for pair in segment.windows(2) {
            tiles.extend(exact_oracle_tiles_for_segment(
                project(pair[0]),
                project(pair[1]),
                zoom,
            ));
        }
    }
    tiles
}

#[derive(Debug, Clone)]
pub struct SyntheticRide {
    pub id: i64,
    pub start_utc: String,
    pub start_local: String,
    pub sport_type: String,
    pub true_segments: Vec<Vec<Point>>,
    pub raw_latlng: Vec<Point>,
    pub times: Vec<f64>,
    pub moving: Vec<bool>,
    pub distances: Vec<f64>,
    pub summary_polyline: String,
}

impl SyntheticRide {
    pub fn to_summary_json(&self) -> Value {
        json!({
            "id": self.id, "start_date": self.start_utc, "start_date_local": self.start_local,
            "sport_type": self.sport_type, "timezone": "(GMT+00:00) UTC",
            "map": {"summary_polyline": self.summary_polyline}
        })
    }

    pub fn to_streams_json(&self, include_moving_distance: bool) -> Value {
        let mut streams = json!({
            "latlng": {"data": self.raw_latlng},
            "time": {"data": self.times}
        });
        if include_moving_distance {
            streams["moving"] = json!({"data": self.moving});
            streams["distance"] = json!({"data": self.distances});
        }
        streams
    }

    pub fn ground_truth_tiles(
        &self,
        engine: &SquadratsEngine,
    ) -> (HashSet<String>, HashSet<String>) {
        let tiles = |zoom| {
            exact_oracle_tiles_for_segments(&self.true_segments, zoom)
                .into_iter()
                .map(|tile| tile.to_string())
                .collect()
        };
        (tiles(engine.squadrat_zoom), tiles(engine.squadratinho_zoom))
    }
}

fn wrap_lon(lon: f64) -> f64 {
    if (-180.0..180.0).contains(&lon) {
        lon
    } else {
        (lon + 180.0).rem_euclid(360.0) - 180.0
    }
}

fn offset_meters((lat, lon): Point, north: f64, east: f64) -> Point {
    (
        lat + (north / EARTH_RADIUS_M).to_degrees(),
        wrap_lon(lon + (east / (EARTH_RADIUS_M * lat.to_radians().cos())).to_degrees()),
    )
}

fn distance_meters(a: Point, b: Point) -> f64 {
    let d_lat = (b.0 - a.0).to_radians();
    let d_lon = wrap_lon(b.1 - a.1).to_radians();
    let h = (d_lat / 2.0).sin().powi(2)
        + a.0.to_radians().cos() * b.0.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.clamp(0.0, 1.0).sqrt().asin()
}

fn simplified_polyline(points: &[Point]) -> String {
    if points.is_empty() {
        return String::new();
    }
    let origin = points[0];
    let local: Vec<_> = points
        .iter()
        .map(|&(lat, lon)| {
            (
                wrap_lon(lon - origin.1).to_radians()
                    * EARTH_RADIUS_M
                    * origin.0.to_radians().cos(),
                (lat - origin.0).to_radians() * EARTH_RADIUS_M,
            )
        })
        .collect();
    let last = points.len() - 1;
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[last] = true;
    let mut work = vec![(0, last)];
    while let Some((start, end)) = work.pop() {
        let (x, y) = local[start];
        let (dx, dy) = (local[end].0 - x, local[end].1 - y);
        let length_sq = dx * dx + dy * dy;
        let mut farthest = None;
        let mut max_sq = 25.0;
        for (index, &(px, py)) in local.iter().enumerate().take(end).skip(start + 1) {
            let (px, py) = (px - x, py - y);
            let t = if length_sq == 0.0 {
                0.0
            } else {
                ((px * dx + py * dy) / length_sq).clamp(0.0, 1.0)
            };
            let error_sq = (px - t * dx).powi(2) + (py - t * dy).powi(2);
            if error_sq > max_sq {
                farthest = Some(index);
                max_sq = error_sq;
            }
        }
        if let Some(index) = farthest {
            keep[index] = true;
            work.extend([(start, index), (index, end)]);
        }
    }
    polyline::encode_coordinates(
        points
            .iter()
            .zip(keep)
            .filter(|(_, keep)| *keep)
            .map(|(&(lat, lon), _)| (lon, lat).into()),
        5,
    )
    .expect("encode synthetic route")
}

pub fn generate_clean_ride(
    seed: u64,
    id: i64,
    center_lat: f64,
    center_lon: f64,
    num_points: usize,
) -> SyntheticRide {
    let mut rng = SplitMix64::new(seed);
    let phase = rng.range_f64(0.0, 2.0 * PI);
    let mut point = (center_lat, wrap_lon(center_lon));
    let raw_latlng: Vec<_> = (0..num_points)
        .map(|index| {
            let previous = point;
            let t = index as f64;
            // Start on an eastbound straight (useful for tunnel dropouts), then
            // ease into bends without changing speed abruptly.
            let bend = ((t - 160.0) / 60.0).clamp(0.0, 1.0);
            let blend = bend * bend * (3.0 - 2.0 * bend);
            let heading = blend * (0.75 * (t / 97.0 + phase).sin() + 0.25 * (t / 31.0).sin());
            let speed = 8.0 + 0.6 * (t / 49.0 + phase).sin();
            point = offset_meters(point, speed * heading.sin(), speed * heading.cos());
            previous
        })
        .collect();
    let distances = raw_latlng
        .iter()
        .enumerate()
        .scan(0.0, |sum, (index, &point)| {
            if index > 0 {
                *sum += distance_meters(raw_latlng[index - 1], point);
            }
            Some(*sum)
        })
        .collect();
    let timestamp = 1_767_261_600 + id.rem_euclid(365 * 24 * 3600 / 60) * 60;
    let start = chrono::DateTime::from_timestamp(timestamp, 0).unwrap();
    SyntheticRide {
        id,
        start_utc: start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        start_local: start.format("%Y-%m-%dT%H:%M:%S").to_string(),
        sport_type: "Ride".into(),
        summary_polyline: simplified_polyline(&raw_latlng),
        true_segments: vec![raw_latlng.clone()],
        raw_latlng,
        times: (0..num_points).map(|i| i as f64).collect(),
        moving: vec![true; num_points],
        distances,
    }
}

pub fn with_gps_jitter(mut ride: SyntheticRide, seed: u64, sigma_meters: f64) -> SyntheticRide {
    let mut rng = SplitMix64::new(seed);
    for point in &mut ride.raw_latlng {
        *point = offset_meters(
            *point,
            rng.gaussian_approx() * sigma_meters,
            rng.gaussian_approx() * sigma_meters,
        );
    }
    ride
}

pub fn inject_interior_spike(
    mut ride: SyntheticRide,
    at_index: usize,
    offset_lat: f64,
    offset_lon: f64,
) -> SyntheticRide {
    let point = &mut ride.raw_latlng[at_index];
    *point = (point.0 + offset_lat, wrap_lon(point.1 + offset_lon));
    ride
}

pub fn inject_consecutive_spikes(
    mut ride: SyntheticRide,
    at_index: usize,
    count: usize,
    offsets: impl AsRef<[Point]>,
) -> SyntheticRide {
    let offsets = offsets.as_ref();
    assert!(!offsets.is_empty() || count == 0);
    for (index, &(lat, lon)) in offsets.iter().cycle().take(count).enumerate() {
        ride = inject_interior_spike(ride, at_index + index, lat, lon);
    }
    ride
}

pub fn inject_leading_spike(
    ride: SyntheticRide,
    offset_lat: f64,
    offset_lon: f64,
) -> SyntheticRide {
    inject_interior_spike(ride, 0, offset_lat, offset_lon)
}

pub fn inject_trailing_spike(
    ride: SyntheticRide,
    offset_lat: f64,
    offset_lon: f64,
) -> SyntheticRide {
    let last = ride.raw_latlng.len() - 1;
    inject_interior_spike(ride, last, offset_lat, offset_lon)
}

pub fn inject_coffee_stop_drift(
    mut ride: SyntheticRide,
    at_index: usize,
    duration_secs: usize,
    drift_meters: f64,
) -> SyntheticRide {
    let point = ride.raw_latlng[at_index];
    let time = ride.times[at_index];
    let distance = ride.distances[at_index];
    let insert = at_index + 1;
    for t in &mut ride.times[insert..] {
        *t += duration_secs as f64;
    }
    let mut rng = SplitMix64::new(ride.id as u64 ^ at_index as u64);
    let (mut north, mut east) = (0.0, 0.0);
    ride.raw_latlng.splice(
        insert..insert,
        (0..duration_secs).map(|_| {
            north = north * 0.87 + rng.gaussian_approx() * 0.5;
            east = east * 0.87 + rng.gaussian_approx() * 0.5;
            offset_meters(point, north * drift_meters, east * drift_meters)
        }),
    );
    ride.times
        .splice(insert..insert, (1..=duration_secs).map(|i| time + i as f64));
    ride.moving
        .splice(insert..insert, std::iter::repeat_n(false, duration_secs));
    ride.distances
        .splice(insert..insert, std::iter::repeat_n(distance, duration_secs));
    ride
}

/// Drop interior samples from a continuous ride. At 8 m/s, 49 missing
/// samples put the two surviving tunnel endpoints about 400 m apart.
/// The first 160 seconds of a clean ride provide a straight stretch.
pub fn inject_tunnel_dropout(
    mut ride: SyntheticRide,
    start_idx: usize,
    drop_count: usize,
) -> SyntheticRide {
    assert!(start_idx > 0 && start_idx + drop_count < ride.raw_latlng.len());
    let range = start_idx..start_idx + drop_count;
    ride.raw_latlng.drain(range.clone());
    ride.times.drain(range.clone());
    ride.moving.drain(range.clone());
    ride.distances.drain(range);
    ride
}

/// The index identifies the first point after the transfer in an aligned, single-segment ride.
pub fn inject_train_transfer(
    mut ride: SyntheticRide,
    at_index: usize,
    jump_lat: f64,
    jump_lon: f64,
    pause_secs: usize,
) -> SyntheticRide {
    assert_eq!(ride.true_segments.len(), 1);
    assert_eq!(ride.true_segments[0].len(), ride.raw_latlng.len());
    assert!(at_index > 0 && at_index < ride.raw_latlng.len());
    let translate = |point: &mut Point| {
        *point = (point.0 + jump_lat, wrap_lon(point.1 + jump_lon));
    };
    let mut second = ride.true_segments[0].split_off(at_index);
    second.iter_mut().for_each(translate);
    ride.true_segments.push(second);
    ride.raw_latlng[at_index..].iter_mut().for_each(translate);
    for time in &mut ride.times[at_index..] {
        *time += pause_secs as f64;
    }
    let mut distance = ride.distances[at_index - 1];
    let second = &ride.true_segments[1];
    for (index, sample_distance) in ride.distances[at_index..].iter_mut().enumerate() {
        if index > 0 {
            distance += distance_meters(second[index - 1], second[index]);
        }
        *sample_distance = distance;
    }
    ride.summary_polyline = simplified_polyline(&ride.true_segments.concat());
    ride
}

pub fn generate_high_latitude_ride(seed: u64, id: i64, lat: f64) -> SyntheticRide {
    generate_clean_ride(seed, id, lat, 15.0, 1800)
}

pub fn generate_antimeridian_ride(seed: u64, id: i64) -> SyntheticRide {
    generate_clean_ride(seed, id, 52.0, 179.97, 1800)
}

fn intersection(a: Rect, b: Rect) -> Option<Rect> {
    let overlap = (a.0.max(b.0), a.1.max(b.1), a.2.min(b.2), a.3.min(b.3));
    (overlap.0 < overlap.2 && overlap.1 < overlap.3).then_some(overlap)
}

fn subtract(outer: Rect, hole: Rect) -> Vec<Rect> {
    let Some((x0, y0, x1, y1)) = intersection(outer, hole) else {
        return vec![outer];
    };
    let (left, top, right, bottom) = outer;
    [
        (left, top, right, y0),
        (left, y1, right, bottom),
        (left, y0, x0, y1),
        (x1, y0, right, y1),
    ]
    .into_iter()
    .filter(|&(a, b, c, d)| a < c && b < d)
    .collect()
}

fn write_ring(kml: &mut String, boundary: &str, rect: (f64, f64, f64, f64), zoom: u8, inner: bool) {
    let (left, top, right, bottom) = rect;
    let mut points = [
        (left, top),
        (left, bottom),
        (right, bottom),
        (right, top),
        (left, top),
    ];
    if inner {
        points.reverse();
    }
    write!(kml, "<{boundary}><LinearRing><coordinates>").unwrap();
    for (x, y) in points {
        let (lon, lat) = lon_lat(x, y, zoom);
        write!(kml, "{lon:.15},{lat:.15},0 ").unwrap();
    }
    write!(kml, "</coordinates></LinearRing></{boundary}>").unwrap();
}

fn write_layer(
    kml: &mut String,
    name: &str,
    zoom: u8,
    rects: &[Rect],
    holes: &[Rect],
) -> HashSet<String> {
    write!(kml, "<Placemark><name>{name}</name><MultiGeometry>").unwrap();
    let mut truth = HashSet::new();
    let n = width(zoom) as u64;
    let rects = rects.iter().flat_map(|&(left, top, right, bottom)| {
        assert!(left < right && top < bottom && right <= n && bottom <= n);
        // Keep each longitude span within the KML parser's antimeridian threshold.
        (left..right)
            .step_by((n / 2) as usize)
            .map(move |x| (x, top, (x + n / 2).min(right), bottom))
    });
    let grid_ring = |kml: &mut String, name, (left, top, right, bottom): Rect, inner| {
        write_ring(
            kml,
            name,
            (left as f64, top as f64, right as f64, bottom as f64),
            zoom,
            inner,
        );
    };
    for outer in rects {
        let clipped: Vec<_> = holes
            .iter()
            .filter_map(|&hole| intersection(outer, hole))
            .collect();
        let mut pieces = vec![outer];
        for &hole in &clipped {
            pieces = pieces
                .into_iter()
                .flat_map(|piece| subtract(piece, hole))
                .collect();
        }
        for &(left, top, right, bottom) in &pieces {
            for x in left..right {
                for y in top..bottom {
                    truth.insert(format!("{zoom}:{x}:{y}"));
                }
            }
        }
        let valid_rings = clipped.iter().enumerate().all(|(i, &hole)| {
            outer.0 < hole.0
                && outer.1 < hole.1
                && hole.2 < outer.2
                && hole.3 < outer.3
                && clipped[..i].iter().all(|&other| {
                    hole.2 < other.0 || other.2 < hole.0 || hole.3 < other.1 || other.3 < hole.1
                })
        });
        if valid_rings {
            kml.push_str("<Polygon>");
            grid_ring(kml, "outerBoundaryIs", outer, false);
            for hole in clipped {
                grid_ring(kml, "innerBoundaryIs", hole, true);
            }
            kml.push_str("</Polygon>");
        } else {
            for piece in pieces {
                kml.push_str("<Polygon>");
                grid_ring(kml, "outerBoundaryIs", piece, false);
                kml.push_str("</Polygon>");
            }
        }
    }
    if truth.is_empty() {
        // Retain the layer with a polygon that contains no tile center.
        kml.push_str("<Polygon>");
        write_ring(
            kml,
            "outerBoundaryIs",
            (0.125, 0.125, 0.25, 0.25),
            zoom,
            false,
        );
        kml.push_str("</Polygon>");
    }
    kml.push_str("</MultiGeometry></Placemark>");
    truth
}

/// Rectangles use half-open (left, top, right, bottom) XYZ grid coordinates.
pub fn generate_kml_from_rects(
    z14_rects: &[Rect],
    z14_holes: &[Rect],
    z17_rects: &[Rect],
) -> (String, HashSet<String>, HashSet<String>) {
    let mut kml = String::from(r#"<kml xmlns="http://www.opengis.net/kml/2.2"><Document>"#);
    let z14 = write_layer(&mut kml, "squadrats", 14, z14_rects, z14_holes);
    let z17 = write_layer(&mut kml, "squadratinhos", 17, z17_rects, &[]);
    kml.push_str("</Document></kml>");
    (kml, z14, z17)
}

#[derive(Debug, Clone)]
pub struct TileAccuracyMetrics {
    pub true_count: usize,
    pub predicted_count: usize,
    pub true_positives: usize,
    pub false_positives: usize,
    pub false_negatives: usize,
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
}

pub fn evaluate_accuracy(
    truth: &HashSet<String>,
    predicted: &HashSet<String>,
) -> TileAccuracyMetrics {
    let true_count = truth.len();
    let predicted_count = predicted.len();
    let true_positives = truth.intersection(predicted).count();
    let ratio = |denominator| {
        if denominator == 0 {
            1.0
        } else {
            true_positives as f64 / denominator as f64
        }
    };
    let precision = ratio(predicted_count);
    let recall = ratio(true_count);
    TileAccuracyMetrics {
        true_count,
        predicted_count,
        true_positives,
        false_positives: predicted_count - true_positives,
        false_negatives: true_count - true_positives,
        precision,
        recall,
        f1: if precision + recall == 0.0 {
            0.0
        } else {
            2.0 * precision * recall / (precision + recall)
        },
    }
}
