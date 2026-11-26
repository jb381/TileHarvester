mod common;

use std::collections::{BTreeMap, HashSet};

use common::fixtures::*;
use common::mock_strava::{MockStravaApi, MockStravaServer};
use serde_json::json;
use tileharvester::db::get_db;
use tileharvester::kml_baseline::{compare_kml, effective_tiles, parse_squadrats_kml};
use tileharvester::strava_client::{load_tokens, HttpStravaClient, StravaApi};
use tileharvester::sync::{
    clean_stream_segments, decode_summary_polyline, sync_once, Point, STREAM_KEYS,
};
use tileharvester::tile_engine::{SquadratsEngine, TileCoord, TileEngine};

fn difference<T: Ord + std::hash::Hash + Clone>(a: &HashSet<T>, b: &HashSet<T>) -> Vec<T> {
    let mut tiles: Vec<_> = a.difference(b).cloned().collect();
    tiles.sort_unstable();
    tiles
}

#[test]
fn oracle_randomized_and_adversarial_segments() {
    let n = f64::from(1 << 17);
    let engine = SquadratsEngine::new(14, 17);
    let mut rng = SplitMix64::new(0xf04a_2026);
    let groups = [
        ("general interior", 32.0, 32.0, None),
        ("near horizontal", 80.0, 1e-6, None),
        ("near vertical", 1e-6, 80.0, None),
        ("steep diagonal", 8.0, 80.0, None),
        ("short sub-tile", 0.2, 0.2, None),
        ("long multi-tile", 256.0, 32.0, None),
        ("equatorial", 32.0, 4.0, Some(0.0)),
        ("mid-latitude", 32.0, 32.0, Some(47.0)),
        ("high latitude north", 32.0, 32.0, Some(84.0)),
        ("high latitude south", 32.0, 32.0, Some(-84.0)),
    ];
    let grid_y = |lat: f64| n * (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0;
    let mut cases = Vec::new();
    for (group, dx, dy, latitude) in groups {
        for _ in 0..900 {
            let x = rng.range_f64(300.0, n - 300.0);
            let y = match latitude {
                Some(lat) => grid_y(lat) + rng.range_f64(-1.0, 1.0),
                None => rng.range_f64(300.0, n - 300.0),
            };
            let end = (x + rng.range_f64(-dx, dx), y + rng.range_f64(-dy, dy));
            cases.push((group, (x, y), end));
        }
    }
    for _ in 0..1000 {
        let y = rng.range_f64(300.0, n - 300.0);
        let west = n / 3600.0 + rng.range_f64(-1.0, 1.0);
        let east = n - n / 3600.0 + rng.range_f64(-1.0, 1.0);
        cases.push((
            "antimeridian",
            (east, y),
            (west, y + rng.range_f64(-8.0, 8.0)),
        ));
    }
    assert_eq!(cases.len(), 10_000);
    cases.extend([
        ("horizontal grid line", (800.25, 800.0), (824.75, 800.0)),
        ("vertical grid line", (800.0, 824.75), (800.0, 800.25)),
        ("interior corner crossings", (800.5, 800.5), (832.5, 832.5)),
        ("opposed corner crossings", (824.0, 800.0), (800.0, 824.0)),
        (
            "integer endpoints southwest",
            (824.0, 824.0),
            (800.0, 800.0),
        ),
        (
            "negative x integer endpoint",
            (824.75, 801.25),
            (800.0, 801.75),
        ),
        (
            "negative y integer endpoint",
            (801.25, 824.75),
            (801.75, 800.0),
        ),
        ("corner singleton", (800.0, 800.0), (800.0, 800.0)),
        ("x=0 grid line", (0.0, 800.0), (0.0, 824.0)),
        ("x=n grid line", (n, 800.0), (n, 824.0)),
        ("x=0 to x=n", (0.0, 800.0), (n, 824.0)),
        ("x=n then inward", (n, 800.0), (n - 24.0, 824.0)),
        ("x=0 then inward", (0.0, 800.0), (24.0, 824.0)),
        ("north polar clamp", (800.5, -1e-6), (824.5, 4.0)),
        ("south polar clamp", (800.5, n + 1e-6), (824.5, n - 4.0)),
        ("antimeridian corner", (n - 8.0, 792.0), (8.0, 808.0)),
        ("near corner", (800.5, 800.5), (824.5, 824.5 + 1e-10)),
    ]);

    let mut counts = [0; 3];
    let mut examples = BTreeMap::<(&str, &str), Vec<String>>::new();
    for (group, a, b) in cases.iter().copied() {
        let mut matches = true;
        let mut symmetric = true;
        let mut forward = Vec::new();
        let mut reverse = Vec::new();
        let mut record = |kind, detail: String| {
            let entries = examples.entry((group, kind)).or_default();
            if entries.len() < 4 {
                entries.push(format!("{a:?} -> {b:?} (z17 grid): {detail}"));
            }
        };
        for zoom in [14, 17] {
            let scale = |(x, y): Point| {
                (
                    x / f64::from(1 << (17 - zoom)),
                    y / f64::from(1 << (17 - zoom)),
                )
            };
            let (start, end) = (scale(a), scale(b));
            let actual = engine.tiles_for_segment(start, end, zoom).unwrap();
            let reversed = engine.tiles_for_segment(end, start, zoom).unwrap();
            let oracle = exact_oracle_tiles_for_segment(start, end, zoom)
                .into_iter()
                .map(|tile| tile.to_string())
                .collect();
            for (direction, tiles) in [("forward", &actual), ("reverse", &reversed)] {
                if *tiles != oracle {
                    matches = false;
                    record(
                        "oracle",
                        format!(
                            "z{zoom} {direction}: extra {:?}, missing {:?}",
                            difference(tiles, &oracle),
                            difference(&oracle, tiles)
                        ),
                    );
                }
            }
            if actual != reversed {
                symmetric = false;
                record(
                    "reversal",
                    format!(
                        "z{zoom}: forward-only {:?}, reverse-only {:?}",
                        difference(&actual, &reversed),
                        difference(&reversed, &actual)
                    ),
                );
            }
            forward.push(actual);
            reverse.push(reversed);
        }
        let mut consistent = true;
        for (direction, tiles) in [("forward", forward), ("reverse", reverse)] {
            let parents = tiles[1]
                .iter()
                .map(|tile| {
                    let mut parts = tile.split(':').skip(1);
                    let mut coord = || parts.next().unwrap().parse::<u64>().unwrap() >> 3;
                    TileCoord {
                        zoom: 14,
                        x: coord(),
                        y: coord(),
                    }
                    .to_string()
                })
                .collect();
            if parents != tiles[0] {
                consistent = false;
                record(
                    "zoom",
                    format!(
                        "{direction}: parent-only {:?}, z14-only {:?}",
                        difference(&parents, &tiles[0]),
                        difference(&tiles[0], &parents)
                    ),
                );
            }
        }
        counts[0] += usize::from(!matches);
        counts[1] += usize::from(!symmetric);
        counts[2] += usize::from(!consistent);
    }
    println!("Geometry: {} segments, {} segments with oracle mismatches, {} reversal mismatches, {} parent-child mismatches", cases.len(), counts[0], counts[1], counts[2]);
    for ((group, kind), entries) in examples {
        for entry in entries {
            println!("  {group} / {kind}: {entry}");
        }
    }
    assert_eq!(counts, [0, 0, 0], "geometry counterexamples listed above");
}

type TileSets = (HashSet<String>, HashSet<String>);

fn accuracy_row(name: &str, truth: &TileSets, actual: &TileSets, details: &mut Vec<String>) {
    let mut cells = Vec::new();
    for (zoom, expected, predicted) in [(14, &truth.0, &actual.0), (17, &truth.1, &actual.1)] {
        let m = evaluate_accuracy(expected, predicted);
        cells.push(format!(
            "{:.6} / {:.6} / {:.6} / {} / {}",
            m.precision, m.recall, m.f1, m.false_positives, m.false_negatives
        ));
        if (m.precision, m.recall, m.false_positives, m.false_negatives) != (1.0, 1.0, 0, 0) {
            details.push(format!(
                "{name} z{zoom}: extra {:?}, missing {:?}",
                difference(predicted, expected),
                difference(expected, predicted)
            ));
        }
    }
    println!("| {name} | {} | {} |", cells[0], cells[1]);
}

#[test]
fn gps_scenario_accuracy_report() {
    let (dir, settings) = common::isolated_db();
    let engine = SquadratsEngine::new(14, 17);
    let clean = generate_clean_ride(867, 101, 52.0, 5.0, 500);
    let north_deg = |meters: f64| (meters / 6_378_137.0).to_degrees();
    let small = north_deg(500.0);
    let far = north_deg(5000.0);
    let transfer = inject_train_transfer(clean.clone(), 250, north_deg(15_000.0), 0.0, 1200);
    let mut noisy = with_gps_jitter(clean.clone(), 54, 4.0);
    for ((lat, lon), &(true_lat, true_lon)) in noisy.raw_latlng.iter_mut().zip(&clean.raw_latlng) {
        let lat_limit = north_deg(4.0);
        let lon_limit = lat_limit / true_lat.to_radians().cos();
        *lat = lat.clamp(true_lat - lat_limit, true_lat + lat_limit);
        *lon = lon.clamp(true_lon - lon_limit, true_lon + lon_limit);
    }
    let rides = [
        ("Clean 1 Hz (500 points)", clean.clone()),
        ("Noisy 1 Hz (±4 m per axis)", noisy),
        (
            "Interior spike (500 m)",
            inject_interior_spike(clean.clone(), 245, small, 0.0),
        ),
        (
            "Interior spike (5 km)",
            inject_interior_spike(clean.clone(), 245, far, 0.0),
        ),
        (
            "2-point burst",
            inject_consecutive_spikes(clean.clone(), 245, 2, [(far, 0.0)]),
        ),
        (
            "3-point burst",
            inject_consecutive_spikes(clean.clone(), 245, 3, [(far, 0.0)]),
        ),
        (
            "Leading spike",
            inject_leading_spike(clean.clone(), far, 0.0),
        ),
        (
            "Trailing spike",
            inject_trailing_spike(clean.clone(), far, 0.0),
        ),
        (
            "Coffee stop (10 min)",
            inject_coffee_stop_drift(clean.clone(), 250, 600, 35.0),
        ),
        (
            "Tunnel dropout (~400 m)",
            inject_tunnel_dropout(clean.clone(), 70, 49),
        ),
        ("Transfer (15 km, 20 min)", transfer.clone()),
        (
            "High latitude +78°",
            generate_high_latitude_ride(867, 102, 78.0),
        ),
        (
            "High latitude −78°",
            generate_high_latitude_ride(867, 103, -78.0),
        ),
        (
            "Antimeridian crossing",
            generate_antimeridian_ride(867, 104),
        ),
    ];
    println!("| Scenario | z14 Precision / Recall / F1 / FP / FN | z17 Precision / Recall / F1 / FP / FN |");
    println!("|---|---|---|");
    let mut details = Vec::new();
    let mut cleaner_stats = Vec::new();
    let mut transfer_segments = None;
    for (name, ride) in rides {
        let (segments, stats) =
            clean_stream_segments(&settings, &ride.to_streams_json(true)).unwrap();
        let tiles = engine.tiles_for_segments(&segments).unwrap();
        accuracy_row(
            name,
            &ride.ground_truth_tiles(&engine),
            &tiles,
            &mut details,
        );
        cleaner_stats.push(format!(
            "{name}: {} points, {} retained, {} segments, {} splits, {} spikes dropped",
            stats.points,
            segments.iter().map(Vec::len).sum::<usize>(),
            stats.segments,
            stats.splits,
            stats.spikes_dropped
        ));
        if ride.true_segments.len() == 2 {
            transfer_segments = Some(segments);
        }
    }

    let truth = clean.ground_truth_tiles(&engine);
    let summary = decode_summary_polyline(&clean.summary_polyline).unwrap();
    let full = engine.tiles_for_points(&clean.raw_latlng).unwrap();
    let simplified = engine.tiles_for_points(&summary).unwrap();
    accuracy_row("Full 1 Hz stream", &truth, &full, &mut details);
    accuracy_row(
        "Precision-5 simplified summary",
        &truth,
        &simplified,
        &mut details,
    );

    let (kml, z14, z17) = generate_kml_from_rects(
        &[(8000, 5300, 8075, 5370)],
        &[(8010, 5310, 8020, 5325), (8040, 5340, 8050, 5350)],
        &[(64002, 42402, 64052, 42502)],
    );
    let local = (z14, z17);
    let path = dir.path().join("dissolved.kml");
    std::fs::write(&path, kml).unwrap();
    let parsed = parse_squadrats_kml(&path).unwrap();
    let mut conn = get_db(&settings).unwrap();
    let transaction = conn.transaction().unwrap();
    for (kind, tiles) in [("squadrat", &local.0), ("squadratinho", &local.1)] {
        for tile in tiles {
            transaction.execute(
                "INSERT INTO global_tiles (tile_kind, tile_id, first_activity_id, first_seen_local) VALUES (?1, ?2, 1, '2026-01-01T00:00:00')",
                rusqlite::params![kind, tile],
            ).unwrap();
        }
    }
    transaction.commit().unwrap();
    let parsed_tiles = (parsed.squadrats, parsed.squadratinhos);
    accuracy_row(
        "Dissolved KML (5,000 tiles per zoom)",
        &local,
        &parsed_tiles,
        &mut details,
    );
    for line in cleaner_stats.iter().chain(&details) {
        println!("  {line}");
    }

    assert!(details.is_empty(), "imperfect scenarios: {details:#?}");
    assert!(summary.len() < clean.raw_latlng.len());
    assert_eq!(full, truth);
    assert_eq!(
        transfer_segments
            .as_ref()
            .unwrap()
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
        [250, 250],
        "transfer must not bridge"
    );
    assert_eq!((local.0.len(), local.1.len()), (5000, 5000));
    let compared = compare_kml(&settings, &path).unwrap();
    assert_eq!(parsed_tiles, local);
    for counts in [compared.squadrat, compared.squadratinho] {
        assert_eq!(
            (counts.shared, counts.kml_only, counts.local_only),
            (5000, 0, 0)
        );
    }
}

#[test]
fn adversarial_gps_cleaner_edge_cases() {
    let (_dir, settings) = common::isolated_db();
    let north = |meters: f64| (52.0 + (meters / 6_371_000.0).to_degrees(), 5.0);
    let points = [north(0.0), north(20.0), north(40.0)];
    let clean = |payload| clean_stream_segments(&settings, &payload).unwrap();

    let stationary = [north(0.0); 3];
    let (segments, stats) = clean(json!({
        "latlng": {"data": stationary}, "time": {"data": [0, 2, 4]},
        "moving": {"data": [false, false, false]}, "distance": {"data": [0, 0, 0]}
    }));
    assert_eq!(segments, [stationary.to_vec()]);
    assert_eq!(stats.stationary_dropped, 0);

    let (segments, stats) = clean(json!({
        "latlng": {"data": points}, "time": {"data": [0, 2, 4]},
        "moving": {"data": [true, false, true]}, "distance": {"data": [0, 20, 40]}
    }));
    assert_eq!(segments, [points.to_vec()]);
    assert_eq!(stats.stationary_dropped, 0);

    let mut payload = json!({
        "latlng": {"data": [points[0], points[1], north(500.0), points[2]]},
        "time": {"data": [0, 2, 3, 4]}
    });
    let (expected, baseline) = clean(payload.clone());
    payload["moving"] = json!({"data": [false]});
    payload["distance"] = json!({"data": [0, 20, 40, 60, 80]});
    let (segments, stats) = clean(payload);
    assert_eq!(expected, [points.to_vec()]);
    assert_eq!(segments, expected);
    assert_eq!(baseline.spikes_dropped, 1);
    assert_eq!(stats.spikes_dropped, baseline.spikes_dropped);
    assert_eq!(stats.stationary_dropped, 0);

    // Four recorded samples from a side street, with reception gaps at both ends.
    let approach = [north(0.0), north(10.0)];
    let excursion = [north(330.0), north(340.0), north(335.0), north(325.0)];
    let exit = [north(15.0), north(25.0)];
    let (segments, stats) = clean(json!({
        "latlng": {"data": ([approach.as_slice(), &excursion, &exit].concat())},
        "time": {"data": [0, 1, 46, 47, 49, 50, 95, 96]}
    }));
    assert_eq!(
        segments,
        [approach.to_vec(), excursion.to_vec(), exit.to_vec()]
    );
    assert_eq!((stats.spikes_dropped, stats.splits), (0, 2));

    for (name, chord, seconds, recorded) in [
        ("high-speed tunnel teleport", 400.0, 2, 400.0),
        ("tunnel detour", 400.0, 50, 900.0),
        (
            "train transfer exceeding bridge cap",
            15_000.0,
            300,
            15_000.0,
        ),
    ] {
        let before = [north(0.0), north(8.0)];
        let after = [north(8.0 + chord), north(16.0 + chord)];
        let (segments, stats) = clean(json!({
            "latlng": {"data": ([before, after].concat())},
            "time": {"data": [0, 1, 1 + seconds, 2 + seconds]},
            "moving": {"data": [true, true, true, true]},
            "distance": {"data": [0.0, 8.0, 8.0 + recorded, 16.0 + recorded]}
        }));
        assert_eq!(segments, [before.to_vec(), after.to_vec()], "{name}");
        assert_eq!((stats.spikes_dropped, stats.splits), (0, 1), "{name}");
    }
}

#[test]
fn http_client_end_to_end_oauth_refresh_retry_sync() {
    let (_dir, mut settings) = common::isolated_db();
    settings.strava_client_id = "loopback-client".into();
    settings.strava_client_secret = "loopback-secret".into();
    let now = chrono::Utc::now();
    let ride_at = |id, minutes_ago| {
        let mut ride = generate_clean_ride(867, id, 52.0, 5.0 + (id - 201) as f64, 500);
        let start = now - chrono::Duration::minutes(minutes_ago);
        ride.start_utc = start.to_rfc3339();
        ride.start_local = start.format("%Y-%m-%dT%H:%M:%S").to_string();
        ride
    };
    let first = ride_at(201, 15);
    let second = ride_at(202, 5);
    let expired = json!({"access_token": "expired", "refresh_token": "initial", "expires_in": -1, "expires_at": now.timestamp() - 1});
    let fresh = json!({"access_token": "fresh", "refresh_token": "rotated", "expires_in": 3600, "expires_at": now.timestamp() + 3600});
    // The mock uses fixed offsets based on per_page even when a page is short.
    let mut summaries = vec![second.to_summary_json()];
    for id in 1000..1199 {
        let mut skipped = second.to_summary_json();
        skipped["id"] = json!(id);
        skipped["sport_type"] = json!("VirtualRide");
        summaries.push(skipped);
    }
    summaries.push(first.to_summary_json());
    let api = MockStravaApi::new()
        .with_activities(summaries)
        .with_short_page(1, 1)
        .with_short_page(2, 1)
        .with_streams(first.id, first.to_streams_json(true))
        .with_streams(second.id, second.to_streams_json(true))
        .with_description(first.id, "Morning ride")
        .with_description(second.id, "Lunch ride")
        .with_oauth_tokens(expired.clone());
    let streams_path = "/api/v3/activities/201/streams";
    let server = MockStravaServer::builder()
        .with_api(api)
        .with_authorization_code("test + code")
        .with_refresh("initial", fresh.clone())
        .with_bearer_token("fresh")
        .with_transient_failures("GET", streams_path, 1, 503)
        .start()
        .unwrap();
    let client =
        HttpStravaClient::with_endpoints(settings.clone(), server.token_url(), server.api_base())
            .unwrap()
            .with_clock_and_sleeper(move || now.timestamp() as f64, |_| {});
    assert_eq!(client.exchange_code("test + code").unwrap(), expired);
    assert_eq!(load_tokens(&settings).unwrap(), Some(expired));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(settings.token_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let result = sync_once(&settings, &client).unwrap();
    assert_eq!(
        (
            result.new_activities,
            result.processed,
            result.annotated,
            result.failed
        ),
        (2, 2, 2, 0)
    );
    assert_eq!(load_tokens(&settings).unwrap(), Some(fresh));
    let requests = server.requests();
    assert_eq!(requests[0].path, "/oauth/token");
    assert_eq!(requests[0].form()["grant_type"], "authorization_code");
    assert_eq!(requests[0].form()["code"], "test + code");
    assert_eq!(requests[1].path, "/oauth/token");
    assert_eq!(requests[1].form()["grant_type"], "refresh_token");
    assert_eq!(requests[1].form()["refresh_token"], "initial");
    assert!(requests[2..]
        .iter()
        .all(|request| request.headers["authorization"] == "Bearer fresh"));
    let pages: Vec<_> = server
        .api()
        .activity_requests()
        .into_iter()
        .map(|request| (request.page, request.per_page))
        .collect();
    assert_eq!(pages, [(1, 200), (2, 200), (3, 200)]);
    let stream_attempts: Vec<_> = requests
        .iter()
        .filter(|request| request.path == streams_path)
        .collect();
    assert_eq!(stream_attempts.len(), 2);
    assert!(stream_attempts
        .iter()
        .all(|request| request.method == "GET" && request.query["keys"] == STREAM_KEYS));
    assert_eq!(STREAM_KEYS, "latlng,time,moving,distance");
    assert_eq!(
        server.api().stream_calls(),
        [(201, STREAM_KEYS.into()), (202, STREAM_KEYS.into())]
    );

    let engine = SquadratsEngine::new(14, 17);
    let mut expected = TileSets::default();
    let writes = server.api().annotation_writes();
    assert_eq!(writes.len(), 2);
    for (ride, original) in [(&first, "Morning ride"), (&second, "Lunch ride")] {
        let (z14, z17) = ride.ground_truth_tiles(&engine);
        let (_, description) = writes.iter().find(|(id, _)| *id == ride.id).unwrap();
        assert!(description.starts_with(&format!("{original}\n\n")));
        assert!(tileharvester::descriptions::has_description_line(
            &settings,
            Some(description)
        ));
        let line = description.lines().nth(2).unwrap();
        assert_eq!(description.lines().count(), 3);
        assert!(line.contains(&format!(
            "{} Squadrats · +{} new",
            expected.0.len() + z14.len(),
            z14.len()
        )));
        assert!(line.contains("/mo · +"));
        assert!(line.ends_with("/wk"));
        expected.0.extend(z14);
        expected.1.extend(z17);
    }
    let conn = get_db(&settings).unwrap();
    assert_eq!(
        effective_tiles(&settings, &conn, "squadrat").unwrap(),
        expected.0
    );
    assert_eq!(
        effective_tiles(&settings, &conn, "squadratinho").unwrap(),
        expected.1
    );

    let again = sync_once(&settings, &client).unwrap();
    assert_eq!(
        (
            again.new_activities,
            again.processed,
            again.annotated,
            again.failed
        ),
        (0, 0, 0, 0)
    );
    let later_requests = server.requests();
    assert!(later_requests[requests.len()..]
        .iter()
        .all(|request| request.method != "PUT"));
    assert_eq!(server.api().annotation_writes(), writes);
}
