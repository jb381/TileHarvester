#[path = "common/fixtures.rs"]
mod fixtures;

use std::{collections::HashSet, f64::consts::PI, fmt::Write, fs};

use chrono::{DateTime, SecondsFormat};
use fixtures::{exact_oracle_tiles_for_segments, generate_clean_ride, tile_center_lat_lon};
use serde_json::json;
use tempfile::tempdir;
use tileharvester::{
    config::Settings,
    gpx::parse_gpx_file,
    squadrats_stats::{compute_grid_metrics, GridMetrics},
    sync::{clean_stream_segments, Point, StreamStats},
    tile_engine::{SquadratsEngine, TileEngine, EARTH_RADIUS},
};

const ENGINE: SquadratsEngine = SquadratsEngine {
    squadrat_zoom: 14,
    squadratinho_zoom: 17,
};

fn offset((lat, lon): Point, north: f64, east: f64) -> Point {
    (
        lat + (north / EARTH_RADIUS).to_degrees(),
        lon + (east / (EARTH_RADIUS * lat.to_radians().cos())).to_degrees(),
    )
}

fn clean(points: &[Point], interval: usize) -> (Vec<Vec<Point>>, StreamStats) {
    let times: Vec<_> = (0..points.len()).map(|i| i * interval).collect();
    clean_stream_segments(
        &Settings::default(),
        &json!({"latlng": {"data": points}, "time": {"data": times}}),
    )
    .unwrap()
}

fn oracle(segments: &[Vec<Point>], zoom: u8) -> HashSet<String> {
    exact_oracle_tiles_for_segments(segments, zoom)
        .into_iter()
        .map(|tile| tile.to_string())
        .collect()
}

fn assert_accuracy(actual: &HashSet<String>, expected: &HashSet<String>, zoom: u8) {
    assert!(!actual.is_empty() && !expected.is_empty());
    let hits = actual.intersection(expected).count() as f64;
    println!(
        "z{zoom} precision {:.3}, recall {:.3} ({} ground-truth tiles)",
        hits / actual.len() as f64,
        hits / expected.len() as f64,
        expected.len()
    );
    assert_eq!(actual, expected, "z{zoom} coverage");
}

fn assert_both_zooms(cleaned: &[Vec<Point>], truth: &[Vec<Point>]) {
    let (large, small) = ENGINE.tiles_for_segments(cleaned).unwrap();
    assert_accuracy(&large, &oracle(truth, 14), 14);
    assert_accuracy(&small, &oracle(truth, 17), 17);
}

#[test]
fn garmin_edge_one_hz_preserves_a_180_degree_hairpin() {
    let mut point = (52.0, 13.0);
    let mut route = vec![point];
    // Head east into a semicircular switchback, then head west. The
    // approach and exit bend smoothly while keeping an 8 m/s pace.
    for i in 0..1_200 {
        let heading = match i {
            0..600 => 0.45 * (i as f64 * PI / 100.0).sin(),
            600..612 => (i as f64 - 599.5) * PI / 12.0,
            _ => PI + 0.45 * ((i - 612) as f64 * PI / 100.0).sin(),
        };
        point = offset(point, 8.0 * heading.sin(), 8.0 * heading.cos());
        route.push(point);
    }
    let (cleaned, stats) = clean(&route, 1);
    assert_eq!(stats.spikes_dropped, 0);
    assert_eq!(cleaned.as_slice(), std::slice::from_ref(&route));
    assert_both_zooms(&cleaned, &[route]);
}

#[test]
fn wahoo_five_second_recording_needs_segment_traversal_near_corners() {
    let width = (1_u64 << 17) as f64;
    // Follow a southeast road near Berlin. At this latitude each tile is
    // about 188 m wide. The two boundary crossings at each corner are
    // 1.5 s apart and fall between consecutive five-second samples.
    let route: Vec<_> = (0..=1_000)
        .map(|sec| {
            let delta = f64::from(sec) * 0.04;
            let x = 70_268.12 + delta;
            let y = 43_285.18 + delta;
            (
                (PI * (1.0 - 2.0 * y / width)).sinh().atan().to_degrees(),
                x * 360.0 / width - 180.0,
            )
        })
        .collect();
    let samples: Vec<_> = route.iter().step_by(5).copied().collect();
    let (cleaned, stats) = clean(&samples, 5);
    assert_eq!(stats.spikes_dropped, 0);
    assert_eq!(cleaned.as_slice(), std::slice::from_ref(&samples));
    let truth = oracle(&[route], 17);
    let (_, dda_tiles) = ENGINE.tiles_for_segments(&cleaned).unwrap();
    let point_only: HashSet<_> = samples
        .iter()
        .flat_map(|point| ENGINE.tiles_for_points(&[*point]).unwrap().1)
        .collect();
    let recall =
        |tiles: &HashSet<String>| tiles.intersection(&truth).count() as f64 / truth.len() as f64;
    let (dda_recall, point_recall) = (recall(&dda_tiles), recall(&point_only));
    println!("Wahoo z17 DDA recall {dda_recall:.3}, point-only recall {point_recall:.3}");
    assert_accuracy(&dda_tiles, &truth, 17);
    assert!(point_only.is_subset(&truth));
    assert!(dda_recall - point_recall > 0.45);
}

#[test]
fn urban_canyon_rejects_all_sub_50_m_lateral_bounces() {
    // The prime meridian forms the eastern border of the ridden z17 column.
    let route: Vec<_> = (0..=600)
        .map(|sec| offset((51.48, 0.0), 8.0 * f64::from(sec), -20.0))
        .collect();
    let mut raw = route.clone();
    let bursts = [
        (80, &[35.0][..]),
        (190, &[42.0, 45.0]),
        (410, &[38.0, 40.0]),
    ];
    let mut bounced = HashSet::new();
    for (start, offsets) in bursts {
        for (index, &east) in (start..).zip(offsets) {
            raw[index] = offset(route[index], 0.0, east);
            assert!(raw[index].1 > 0.0);
            bounced.insert(index);
        }
    }
    let truth = oracle(std::slice::from_ref(&route), 17);
    let (_, raw_tiles) = ENGINE.tiles_for_points(&raw).unwrap();
    assert!(truth.is_subset(&raw_tiles));
    assert!(
        !raw_tiles.is_subset(&truth),
        "raw multipath must award phantom tiles"
    );
    let (cleaned, stats) = clean(&raw, 1);
    assert_eq!(stats.spikes_dropped, bounced.len());
    let retained: Vec<_> = route
        .into_iter()
        .enumerate()
        .filter_map(|(i, point)| (!bounced.contains(&i)).then_some(point))
        .collect();
    assert_eq!(cleaned, [retained]);
    let (_, actual) = ENGINE.tiles_for_segments(&cleaned).unwrap();
    assert_accuracy(&actual, &truth, 17);
}

#[test]
fn gpx_round_trip_preserves_coffee_stop_and_cleans_spike() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("coffee-loop.gpx");
    let ride = generate_clean_ride(42, 4, 52.0, 13.0, 1_200);
    let split = 600;
    let pause = 900;
    let truth = [
        ride.raw_latlng[..split].to_vec(),
        ride.raw_latlng[split..].to_vec(),
    ];
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<gpx xmlns="http://www.topografix.com/GPX/1/1" version="1.1" creator="TileHarvester">
<trk><name>Coffee &amp; country lanes</name>"#,
    );
    let epoch = 1_767_261_600;
    for (segment, points) in truth.iter().enumerate() {
        xml.push_str("<trkseg>");
        for (index, &(lat, lon)) in points.iter().enumerate() {
            let (lat, lon) = if segment == 0 && index == 200 {
                offset((lat, lon), 1_500.0, 800.0)
            } else {
                (lat, lon)
            };
            let time =
                DateTime::from_timestamp(epoch + (index + segment * (split + pause)) as i64, 0)
                    .unwrap()
                    .to_rfc3339_opts(SecondsFormat::Secs, true);
            write!(
                xml,
                r#"<trkpt lat="{lat}" lon="{lon}"><time>{time}</time></trkpt>"#
            )
            .unwrap();
        }
        xml.push_str("</trkseg>");
    }
    xml.push_str("</trk></gpx>");
    fs::write(&path, xml).unwrap();
    let track = parse_gpx_file(&path).unwrap();
    assert_eq!(track.name.as_deref(), Some("Coffee & country lanes"));
    assert_eq!(track.segments.len(), 2);
    let settings = Settings::default();
    let mut cleaned = Vec::new();
    for (index, segment) in track.segments.iter().enumerate() {
        let start = index * (split + pause);
        assert_eq!(segment.points.len(), split);
        assert_eq!(
            segment.times,
            (start..start + split).map(|t| t as f64).collect::<Vec<_>>()
        );
        let (segments, stats) =
            clean_stream_segments(&settings, &segment.to_stream_json()).unwrap();
        assert_eq!(stats.spikes_dropped, usize::from(index == 0));
        assert_eq!(segments.len(), 1);
        cleaned.extend(segments);
    }
    assert_both_zooms(&cleaned, &truth);
}

#[test]
fn gpx_file_rejects_dtd_and_external_entities() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("unsafe.gpx");
    for xml in [
        r#"<!DOCTYPE gpx><gpx version="1.1"/>"#,
        r#"<!DOCTYPE gpx [<!ENTITY external SYSTEM "file:///nonexistent-gpx-fixture">]>
<gpx version="1.1"><trk><name>&external;</name><trkseg/></trk></gpx>"#,
    ] {
        fs::write(&path, xml).unwrap();
        assert!(parse_gpx_file(&path)
            .unwrap_err()
            .to_string()
            .starts_with("Unsafe GPX XML:"));
    }
}

// Compute each row corridor as a separate ride and aggregate its coverage.
fn row_coverage(zoom: u8, rows: impl IntoIterator<Item = (u64, u64, u64)>) -> HashSet<String> {
    let width = 1_u64 << zoom;
    rows.into_iter()
        .flat_map(|(x, y, count)| {
            let points: Vec<_> = (x..x + count)
                .map(|column| tile_center_lat_lon(column % width, y, zoom))
                .collect();
            let (large, small) = ENGINE.tiles_for_points(&points).unwrap();
            if zoom == 14 {
                large
            } else {
                small
            }
        })
        .collect()
}

#[test]
fn multi_ride_yards_and_ubersquadrats_match_analytical_grids() {
    let large = row_coverage(14, (5_400..5_405).map(|y| (8_780, y, 5)));
    assert_eq!(
        compute_grid_metrics(large.iter().map(String::as_str), 14),
        GridMetrics {
            total: 25,
            yard: 9,
            ubersquadrat: 5,
            ubersquadrat_origin: Some((8_780, 5_400))
        }
    );

    // A 6×6 hole leaves a two-tile-wide frame in the 10×10 grid.
    // Only the four inner corner cells qualify for Yardinho, each isolated.
    let frame_rows = (0..10).flat_map(|row| {
        let y = 43_200 + row;
        if (2..8).contains(&row) {
            vec![(70_240, y, 2), (70_248, y, 2)]
        } else {
            vec![(70_240, y, 10)]
        }
    });
    let seam = (1 << 17) - 2;
    let small = row_coverage(17, frame_rows.chain((43_200..43_204).map(|y| (seam, y, 4))));
    assert_eq!(
        compute_grid_metrics(small.iter().map(String::as_str), 17),
        GridMetrics {
            total: 100 - 36 + 16,
            yard: 4,
            ubersquadrat: 4,
            ubersquadrat_origin: Some((seam, 43_200))
        }
    );
}
