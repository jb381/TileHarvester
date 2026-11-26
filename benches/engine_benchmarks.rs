#[path = "../tests/common/fixtures.rs"]
mod fixtures;
#[path = "../tests/common/mock_strava.rs"]
mod mock_strava;

use std::{fmt::Write, fs, hint::black_box, time::Duration};

use criterion::{
    criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, SamplingMode, Throughput,
};
use fixtures::{
    generate_clean_ride, generate_kml_from_rects, inject_coffee_stop_drift, inject_interior_spike,
    inject_train_transfer, inject_tunnel_dropout, with_gps_jitter,
};
use mock_strava::MockStravaApi;
use rusqlite::params;
use tempfile::tempdir;
use tileharvester::{
    config::Settings,
    db::{get_db, migrate},
    gpx::parse_gpx_bytes,
    history::{rebuild_tile_history, AffectedTiles},
    kml_baseline::parse_squadrats_kml,
    models::TileKind,
    squadrats_stats::compute_grid_metrics,
    sync::{clean_stream_segments, sync_once},
    tile_engine::{SquadratsEngine, TileCoord, TileEngine},
};

fn tile_engine(c: &mut Criterion) {
    let engine = SquadratsEngine::new(14, 17);
    let mut group = c.benchmark_group("tile_engine");
    group.sampling_mode(SamplingMode::Flat);
    for count in [1_000, 10_000, 50_000] {
        let ride = generate_clean_ride(42, 1, 52.0, 13.0, count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::new("tiles_for_points", count),
            &ride.raw_latlng,
            |b, points| b.iter(|| engine.tiles_for_points(black_box(points)).unwrap()),
        );
    }
    let ride = generate_clean_ride(42, 1, 52.0, 13.0, 10_000);
    let ride = inject_train_transfer(ride, 5_000, 0.1, 0.1, 900);
    group.throughput(Throughput::Elements(10_000));
    group.bench_function("tiles_for_segments/10000", |b| {
        b.iter(|| {
            engine
                .tiles_for_segments(black_box(&ride.true_segments))
                .unwrap()
        })
    });
    group.finish();
}

fn stream_cleaner(c: &mut Criterion) {
    const PAUSE: usize = 300;
    const DROPOUT: usize = 49;
    let dir = tempdir().unwrap();
    let settings = Settings::with_data_dir(dir.path());
    let mut group = c.benchmark_group("stream_cleaner");
    group.sampling_mode(SamplingMode::Flat);
    for count in [10_000, 50_000] {
        let ride = generate_clean_ride(42, 1, 52.0, 13.0, count - PAUSE + DROPOUT);
        let ride = inject_train_transfer(ride, count / 2, 0.1, 0.1, 900);
        let ride = inject_tunnel_dropout(ride, 80, DROPOUT);
        let mut ride = with_gps_jitter(ride, 43, 2.0);
        for index in (500..ride.raw_latlng.len() - 1).step_by(1_000) {
            ride = inject_interior_spike(ride, index, 0.02, -0.01);
        }
        let ride = inject_coffee_stop_drift(ride, count / 3, PAUSE, 10.0);
        let streams = ride.to_streams_json(true);
        group.throughput(Throughput::Elements(ride.raw_latlng.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("clean_stream_segments", count),
            &streams,
            |b, streams| b.iter(|| clean_stream_segments(&settings, black_box(streams)).unwrap()),
        );
    }
    group.finish();
}

fn kml_parser(c: &mut Criterion) {
    let dir = tempdir().unwrap();
    let mut group = c.benchmark_group("kml_parser");
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(20);
    for (count, width, hole_width, small_height) in [
        (500, 10, 0, 10),
        (5_000, 110, 20, 100),
        (20_000, 485, 20, 100),
    ] {
        let rect = (8_000, 5_000, 8_000 + width, 5_040);
        let holes = if hole_width == 0 {
            vec![]
        } else {
            vec![(8_005, 5_010, 8_005 + hole_width, 5_030)]
        };
        let (kml, squadrats, squadratinhos) = generate_kml_from_rects(
            &[rect],
            &holes,
            &[(64_000, 40_000, 64_010, 40_000 + small_height)],
        );
        let total_tiles = (squadrats.len() + squadratinhos.len()) as u64;
        assert_eq!(total_tiles, count);
        let path = dir.path().join(format!("{count}.kml"));
        fs::write(&path, kml).unwrap();
        let parsed = parse_squadrats_kml(&path).unwrap();
        assert_eq!(
            parsed.squadrats.len() + parsed.squadratinhos.len(),
            count as usize
        );
        group.throughput(Throughput::Elements(total_tiles));
        group.bench_with_input(
            BenchmarkId::new("parse_squadrats_kml", count),
            &path,
            |b, path| b.iter(|| parse_squadrats_kml(black_box(path)).unwrap()),
        );
    }
    group.finish();
}

fn history_rebuild(c: &mut Criterion) {
    const ACTIVITIES: i64 = 500;
    let dir = tempdir().unwrap();
    let settings = Settings::with_data_dir(dir.path());
    migrate(&settings).unwrap();
    let mut conn = get_db(&settings).unwrap();
    let mut affected = AffectedTiles::default();
    let tx = conn.transaction().unwrap();
    {
        let mut activity = tx
            .prepare(
                "INSERT INTO activities (id, start_utc, start_local, sport_type, status)
                      VALUES (?, ?, ?, 'Ride', 'processed')",
            )
            .unwrap();
        let mut tile = tx
            .prepare(
                "INSERT INTO activity_tiles (activity_id, tile_kind, tile_id, is_new)
                      VALUES (?, ?, ?, 0)",
            )
            .unwrap();
        for id in 1..=ACTIVITIES {
            let ride = generate_clean_ride(id as u64, id, 52.0, 13.0, 200);
            activity
                .execute(params![id, ride.start_utc, ride.start_local])
                .unwrap();
            // Shift the corridors across nearby tile rows to model repeated routes.
            let corridor = (id as u64 % 50) * 2;
            for (kind, zoom, x, y, count) in [
                (TileKind::Squadrat, 14, 8_780, 5_400 + corridor / 8, 10),
                (TileKind::Squadratinho, 17, 70_240, 43_200 + corridor, 90),
            ] {
                for offset in 0..count {
                    let tile_id = TileCoord {
                        zoom,
                        x: x + offset,
                        y,
                    }
                    .to_string();
                    tile.execute(params![id, kind, tile_id]).unwrap();
                    if id == ACTIVITIES {
                        affected.add(kind, tile_id);
                    }
                }
            }
        }
    }
    rebuild_tile_history(&tx, None).unwrap();
    tx.commit().unwrap();

    let mut group = c.benchmark_group("history_rebuild");
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(20);
    for (name, tiles, activities, measurement_secs) in [
        ("incremental_single_activity", Some(&affected), 1, 1.0),
        ("full_rebuild_500_activities", None, ACTIVITIES as u64, 5.5),
    ] {
        group.measurement_time(Duration::from_secs_f64(measurement_secs));
        group.throughput(Throughput::Elements(activities));
        group.bench_function(name, |b| {
            b.iter(|| {
                let tx = conn.transaction().unwrap();
                rebuild_tile_history(&tx, black_box(tiles)).unwrap();
                tx.rollback().unwrap();
            })
        });
    }
    group.finish();
}

fn sync_pipeline(c: &mut Criterion) {
    const RIDES: usize = 25;
    let rides: Vec<_> = (1..=RIDES as i64)
        .rev()
        .map(|id| generate_clean_ride(id as u64, id, 52.0, 13.0 + id as f64 * 0.001, 200))
        .collect();
    let mut group = c.benchmark_group("sync_pipeline");
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(20);
    group.throughput(Throughput::Elements(RIDES as u64));
    group.bench_function("sync_once_25_rides", |b| {
        b.iter_batched(
            || {
                let dir = tempdir().unwrap();
                let mut settings = Settings::with_data_dir(dir.path());
                // Include the fixed historical fixtures regardless of the run date.
                settings.sync_lookback_days = 36_500;
                settings.sync_annotation_window_days = 0;
                migrate(&settings).unwrap();
                let api = MockStravaApi::new()
                    .with_activities(rides.iter().map(|ride| ride.to_summary_json()).collect());
                let api = rides.iter().fold(api, |api, ride| {
                    api.with_streams(ride.id, ride.to_streams_json(true))
                });
                (dir, settings, api)
            },
            |(dir, settings, api)| {
                let result = sync_once(&settings, &api).unwrap();
                assert_eq!(result.processed, RIDES);
                assert_eq!(result.failed, 0);
                (dir, settings, api)
            },
            BatchSize::PerIteration,
        )
    });
    group.finish();
}

fn squadrats_stats(c: &mut Criterion) {
    let mut group = c.benchmark_group("squadrats_stats");
    for (name, zoom, x, y, width, height) in [
        ("squadrats", 14, 8_780, 5_400, 100, 50),
        ("squadratinhos", 17, 70_240, 43_200, 250, 100),
    ] {
        let tiles: Vec<_> = (y..y + height)
            .flat_map(|y| (x..x + width).map(move |x| TileCoord { zoom, x, y }.to_string()))
            .collect();
        group.throughput(Throughput::Elements(tiles.len() as u64));
        group.bench_with_input(BenchmarkId::new(name, tiles.len()), &tiles, |b, tiles| {
            b.iter(|| compute_grid_metrics(black_box(tiles).iter().map(String::as_str), zoom))
        });
    }
    group.finish();
}

fn gpx_parser(c: &mut Criterion) {
    const POINTS: usize = 10_000;
    let ride = generate_clean_ride(42, 1, 52.0, 13.0, POINTS);
    let start = chrono::DateTime::parse_from_rfc3339(&ride.start_utc).unwrap();
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<gpx xmlns="http://www.topografix.com/GPX/1/1" version="1.1" creator="TileHarvester">
<trk><name>Brandenburg morning ride</name><trkseg>"#,
    );
    for (second, &(lat, lon)) in ride.raw_latlng.iter().enumerate() {
        let time = (start + chrono::TimeDelta::seconds(second as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let elevation = 45.0 + 12.0 * (second as f64 / 180.0).sin();
        writeln!(
            xml,
            r#"<trkpt lat="{lat:.8}" lon="{lon:.8}"><ele>{elevation:.1}</ele><time>{time}</time></trkpt>"#,
        )
        .unwrap();
    }
    xml.push_str("</trkseg></trk></gpx>");
    let bytes = xml.as_bytes();
    let mut group = c.benchmark_group("gpx_parser");
    group.throughput(Throughput::Elements(POINTS as u64));
    group.bench_function("parse_gpx_bytes/10000", |b| {
        b.iter(|| parse_gpx_bytes(black_box(bytes)).unwrap())
    });
    group.finish();
}

criterion_group! {
    name = benchmarks;
    config = Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_millis(400))
        .measurement_time(Duration::from_secs(1));
    targets = tile_engine, stream_cleaner, kml_parser, history_rebuild, sync_pipeline, squadrats_stats, gpx_parser
}
criterion_main!(benchmarks);
