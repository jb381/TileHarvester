mod common;

use common::{
    fixtures::{tile_bounds_lon_lat, tile_center_lat_lon},
    isolated_db,
};
use serde_json::json;
use std::{collections::HashSet, fs, path::PathBuf};
use tileharvester::{
    config::Settings,
    db::{get_activity_row, get_db, migrate, MIGRATIONS},
    descriptions::{remove_description_line, update_description_line},
    kml_baseline::{effective_tile_count, import_baseline, parse_squadrats_kml, tiles_inside_ring},
    recompute::{recompute_all, recompute_novelty_from_stored_tiles},
    strava_client::compute_rate_limit_delay,
    sync::{
        compute_historical_novelty, compute_period_totals, latest_stored_activity_utc,
        store_activity_tiles,
    },
    tile_engine::SquadratsEngine,
};

fn ring(x: u64, y: u64, size: u64, zoom: u8) -> String {
    [
        (x, y),
        (x + size, y),
        (x + size, y + size),
        (x, y + size),
        (x, y),
    ]
    .map(|(x, y)| {
        let ((lon, lat), _) = tile_bounds_lon_lat(x, y, zoom);
        format!("{lon},{lat},0")
    })
    .join(" ")
}

fn fixture(settings: &Settings) -> PathBuf {
    let path = settings.data_dir.join("tiles.kml");
    let outer = ring(8000, 6000, 2, 14);
    let hole = ring(8001, 6001, 1, 14);
    let small = ring(64000, 48000, 1, 17);
    fs::write(
        &path,
        format!(
            r#"<kml xmlns="http://www.opengis.net/kml/2.2"><Document>
    <Placemark><name>squadrats</name><Polygon>
    <outerBoundaryIs><LinearRing><coordinates>{outer}</coordinates></LinearRing></outerBoundaryIs>
    <innerBoundaryIs><LinearRing><coordinates>{hole}</coordinates></LinearRing></innerBoundaryIs>
    </Polygon></Placemark>
    <Placemark><name>squadratinhos</name><Polygon>
    <outerBoundaryIs><LinearRing><coordinates>{small}</coordinates></LinearRing></outerBoundaryIs>
    </Polygon></Placemark></Document></kml>"#
        ),
    )
    .unwrap();
    path
}

fn seed(settings: &Settings, id: i64, utc: &str) {
    get_db(settings)
        .unwrap()
        .execute(
            "INSERT INTO activities (id,start_utc,start_local,sport_type) VALUES (?1,?2,?3,'Ride')",
            rusqlite::params![id, utc, utc.trim_end_matches('Z')],
        )
        .unwrap();
}

fn row(settings: &Settings, id: i64) -> tileharvester::db::ActivityRow {
    get_activity_row(&get_db(settings).unwrap(), id)
        .unwrap()
        .unwrap()
}

fn store(settings: &Settings, id: i64, x: i32) {
    let result = store_activity_tiles(
        settings,
        &row(settings, id),
        &[tile_center_lat_lon(x as u64, 6000, 14)],
        "summary_polyline",
        None,
    )
    .unwrap();
    assert_eq!(result.status.as_str(), "processed");
}

fn scalar(settings: &Settings, sql: &str) -> i64 {
    get_db(settings)
        .unwrap()
        .query_row(sql, [], |row| row.get(0))
        .unwrap()
}

#[test]
fn late_history_repairs_novelty_and_replacing_route_removes_obsolete_tiles() {
    let (_dir, s) = isolated_db();
    seed(&s, 1, "2026-08-01T10:00:00Z");
    seed(&s, 2, "2026-08-02T10:00:00Z");
    store(&s, 2, 8000);
    store(&s, 1, 8000);
    assert_eq!(
        (row(&s, 1).new_squadrat_count, row(&s, 2).new_squadrat_count),
        (1, 0)
    );
    assert_eq!(
        compute_period_totals(&s, "2026-08-02T10:00:00").unwrap(),
        (1, 1)
    );
    store(&s, 1, 8010);
    store(&s, 2, 8010);
    assert_eq!(effective_tile_count(&s, "squadrat").unwrap(), 1);
    assert_eq!(
        scalar(
            &s,
            "SELECT count(*) FROM global_tiles WHERE tile_id = '14:8000:6000'"
        ),
        0
    );
}

#[test]
fn equal_utc_instants_use_id_with_or_without_baseline() {
    for baseline in [false, true] {
        let (_dir, s) = isolated_db();
        if baseline {
            import_baseline(&s, &fixture(&s), Some("2026-08-01T00:00:00Z")).unwrap();
        }
        seed(&s, 1, "2026-08-02T10:00:00Z");
        seed(&s, 2, "2026-08-02T12:00:00+02:00");
        store(&s, 2, 8010);
        store(&s, 1, 8010);
        for _ in 0..2 {
            assert_eq!(
                (row(&s, 1).new_squadrat_count, row(&s, 2).new_squadrat_count),
                (1, 0)
            );
            let novelty = compute_historical_novelty(
                &s,
                2,
                "2026-08-02T12:00:00",
                &HashSet::from(["14:8010:6000".into()]),
                &HashSet::new(),
                None,
            )
            .unwrap();
            assert_eq!((novelty.processed_before, novelty.new_squadrats), (1, 0));
            recompute_novelty_from_stored_tiles(&s).unwrap();
        }
    }
}

#[test]
fn incremental_history_matches_full_rebuild() {
    for baseline in [false, true] {
        let (_dir, s) = isolated_db();
        if baseline {
            import_baseline(&s, &fixture(&s), Some("2026-08-01T00:00:00Z")).unwrap();
        }
        for id in 1..=6 {
            seed(&s, id, &format!("2026-08-0{id}T10:00:00Z"));
        }
        for id in [6, 2, 4, 1, 5, 3] {
            store(&s, i64::from(id), 8000 + id % 3);
        }
        store(&s, 1, 8010);
        let counts = || {
            (1..=6)
                .map(|id| row(&s, id).new_squadrat_count)
                .collect::<Vec<_>>()
        };
        let before = counts();
        assert_eq!(
            before,
            if baseline {
                vec![1, 1, 0, 0, 0, 0]
            } else {
                vec![1, 1, 1, 1, 0, 0]
            }
        );
        let total = effective_tile_count(&s, "squadrat").unwrap();
        recompute_novelty_from_stored_tiles(&s).unwrap();
        assert_eq!(counts(), before);
        assert_eq!(effective_tile_count(&s, "squadrat").unwrap(), total);
    }
}

#[test]
fn negative_direction_grid_endpoints_terminate() {
    let (done, result) = std::sync::mpsc::channel();
    let traversal = std::thread::spawn(move || {
        let engine = SquadratsEngine::new(14, 17);
        for (start, end) in [
            ((1.5, 1.5), (1.0, 2.0)),
            ((1.5, 1.5), (2.0, 1.0)),
            ((1.0, 2.0), (1.5, 1.5)),
        ] {
            assert!(engine
                .tiles_for_segment(start, end, 14)
                .unwrap()
                .contains("14:1:1"));
        }
        done.send(()).unwrap();
    });
    result
        .recv_timeout(std::time::Duration::from_secs(3))
        .expect("grid traversal failed to complete");
    traversal.join().unwrap();
}

#[test]
fn rate_limit_delays_use_each_header_family_and_window() {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    for prefix in ["x-ratelimit", "x-readratelimit"] {
        for (usage, expected) in [
            ("195,300", 801.0),
            ("10,1995", 86301.0),
            ("195,1995", 86301.0),
            ("10,300", 0.0),
            ("invalid", 0.0),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                HeaderName::from_bytes(format!("{prefix}-limit").as_bytes()).unwrap(),
                HeaderValue::from_static("200,2000"),
            );
            headers.insert(
                HeaderName::from_bytes(format!("{prefix}-usage").as_bytes()).unwrap(),
                HeaderValue::from_static(usage),
            );
            assert_eq!(compute_rate_limit_delay(&headers, 5, 100.0), expected);
        }
    }
}

#[test]
fn description_preserves_tool_mentions_in_prose() {
    let (_dir, s) = isolated_db();
    let line = "🗺️ TileHarvester: stats";
    for description in [
        "I tried TileHarvester: it helped me explore.",
        "— TileHarvester: it helped me explore.",
        "- TileHarvester: it helped me explore.",
        "* TileHarvester: it helped me explore.",
        "... TileHarvester: it helped me explore.",
        "> TileHarvester: it helped me explore.",
    ] {
        let updated = update_description_line(&s, Some(description), line);
        assert_eq!(updated, format!("{description}\n\n{line}"));
        assert_eq!(remove_description_line(&s, Some(&updated)), description);
    }
}

#[test]
fn kml_validates_layers_zoom_raster_and_xml_safety() {
    let (_dir, mut s) = isolated_db();
    let path = fixture(&s);
    let valid = fs::read_to_string(&path).unwrap();
    let parsed = parse_squadrats_kml(&path).unwrap();
    assert_eq!(parsed.for_kind("squadrat").unwrap().len(), 3);
    assert!(!parsed
        .for_kind("squadrat")
        .unwrap()
        .contains("14:8001:6001"));
    fs::write(&path, valid.replace("<name>squadratinhos", "<name>other")).unwrap();
    assert!(parse_squadrats_kml(&path)
        .unwrap_err()
        .to_string()
        .contains("both"));
    let attack = format!(
        "<!DOCTYPE kml [<!ENTITY x \"squadrats\">]>{}",
        valid.replace(">squadrats<", ">&x;<")
    );
    let utf16 = [0xff, 0xfe]
        .into_iter()
        .chain(attack.encode_utf16().flat_map(u16::to_le_bytes))
        .collect::<Vec<_>>();
    for bytes in [attack.into_bytes(), utf16] {
        fs::write(&path, bytes).unwrap();
        assert!(parse_squadrats_kml(&path)
            .unwrap_err()
            .to_string()
            .contains("Unsafe KML XML"));
    }
    assert!(tiles_inside_ring(&ring(100, 100, 20, 17), 17, 10, 50_000)
        .unwrap_err()
        .to_string()
        .contains("tile limit"));
    let path = fixture(&s);
    s.squadrat_zoom = 13;
    assert!(import_baseline(&s, &path, None)
        .unwrap_err()
        .to_string()
        .contains("zoom"));
}

#[test]
fn history_failure_rolls_back_baseline_and_eligibility_changes() {
    let (_dir, s) = isolated_db();
    seed(&s, 1, "2026-08-01T10:00:00Z");
    store(&s, 1, 8000);
    let conn = get_db(&s).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER stop_history BEFORE DELETE ON global_tiles
        BEGIN SELECT RAISE(ABORT, 'injected history failure'); END;",
    )
    .unwrap();
    let error = import_baseline(&s, &fixture(&s), Some("2026-08-02T00:00:00Z")).unwrap_err();
    assert!(error.to_string().contains("injected history failure"));
    assert_eq!(scalar(&s, "SELECT count(*) FROM baseline_imports"), 0);
    assert_eq!(scalar(&s, "SELECT count(*) FROM baseline_tiles"), 0);
    assert!(!row(&s, 1).baseline_covered);
    conn.execute("UPDATE activities SET sport_type='VirtualRide'", [])
        .unwrap();
    assert!(recompute_all(&s)
        .unwrap_err()
        .to_string()
        .contains("injected history failure"));
    assert_eq!(row(&s, 1).status.as_str(), "processed");
    assert_eq!(scalar(&s, "SELECT sum(is_new) FROM activity_tiles"), 2);
    assert_eq!(effective_tile_count(&s, "squadrat").unwrap(), 1);
}

#[test]
fn unstored_novelty_uses_baseline_or_local_order_as_applicable() {
    let (_dir, s) = isolated_db();
    import_baseline(&s, &fixture(&s), Some("2026-08-01T00:00:00Z")).unwrap();
    let tiles = HashSet::from(["14:8000:6000".into()]);
    let result = compute_historical_novelty(
        &s,
        99,
        "2026-08-02T10:00:00",
        &tiles,
        &HashSet::new(),
        Some("2026-08-02T10:00:00Z"),
    )
    .unwrap();
    assert_eq!(
        (result.total_squadrats_before, result.new_squadrats),
        (3, 0)
    );
    assert_eq!(scalar(&s, "SELECT count(*) FROM activities"), 0);

    let (_dir, s) = isolated_db();
    seed(&s, 1, "2026-08-01T09:00:00Z");
    seed(&s, 2, "2026-08-01T10:00:00Z");
    seed(&s, 3, "invalid");
    store(&s, 1, 8000);
    get_db(&s).unwrap().execute("UPDATE activities SET start_local='2026-08-01T12:00:00', start_utc='2026-08-01T12:00:00+03:00' WHERE id=1",[]).unwrap();
    assert_eq!(
        latest_stored_activity_utc(&s)
            .unwrap()
            .unwrap()
            .to_rfc3339(),
        "2026-08-01T10:00:00+00:00"
    );
    let result =
        compute_historical_novelty(&s, 99, "2026-08-01T11:00:00", &tiles, &HashSet::new(), None)
            .unwrap();
    assert_eq!(
        (
            result.processed_before,
            result.seen_squadrats,
            result.new_squadrats
        ),
        (0, 0, 1)
    );
    let result =
        compute_historical_novelty(&s, 2, "2026-08-01T11:00:00", &tiles, &HashSet::new(), None)
            .unwrap();
    assert_eq!(
        (
            result.processed_before,
            result.seen_squadrats,
            result.new_squadrats
        ),
        (1, 1, 0)
    );
}

#[test]
fn migration_restores_refresh_column_without_disturbing_history() {
    let (_dir, s) = isolated_db();
    seed(&s, 1, "2026-08-01T10:00:00Z");
    store(&s, 1, 8000);
    let conn = get_db(&s).unwrap();
    conn.execute(
        "ALTER TABLE activities DROP COLUMN stream_refresh_pending",
        [],
    )
    .unwrap();
    conn.execute(
        "DELETE FROM schema_version WHERE version=?",
        [MIGRATIONS.len()],
    )
    .unwrap();
    for _ in 0..2 {
        migrate(&s).unwrap();
        assert_eq!(
            scalar(&s, "SELECT count(*) FROM schema_version"),
            MIGRATIONS.len() as i64
        );
        let activity = row(&s, 1);
        assert_eq!(activity.status.as_str(), "processed");
        assert!(!activity.stream_refresh_pending);
        assert_eq!(activity.new_squadrat_count, 1);
        assert_eq!(effective_tile_count(&s, "squadrat").unwrap(), 1);
    }
}

#[test]
fn settings_round_trip_and_database_reset() {
    use tileharvester::db::{get_setting, reset, set_setting};
    let (_dir, s) = isolated_db();
    assert_eq!(get_setting(&s, "test").unwrap(), None);
    for value in [
        json!("text"),
        json!(5),
        json!([1, 2]),
        json!({"enabled":true}),
    ] {
        set_setting(&s, "test", &value).unwrap();
        assert_eq!(get_setting(&s, "test").unwrap(), Some(value));
    }
    get_db(&s)
        .unwrap()
        .execute(
            "INSERT INTO settings(key,value) VALUES ('legacy','{invalid')",
            [],
        )
        .unwrap();
    assert_eq!(get_setting(&s, "legacy").unwrap(), Some(json!("{invalid")));
    seed(&s, 1, "2026-08-01T10:00:00Z");
    store(&s, 1, 8000);
    reset(&s).unwrap();
    migrate(&s).unwrap();
    assert_eq!(get_setting(&s, "test").unwrap(), None);
    assert_eq!(scalar(&s, "SELECT count(*) FROM activities"), 0);
    assert_eq!(effective_tile_count(&s, "squadrat").unwrap(), 0);
}

#[cfg(unix)]
#[test]
fn token_save_is_private_and_failed_save_preserves_original_bytes() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    use tileharvester::strava_client::{load_tokens, save_tokens};
    let (_dir, s) = isolated_db();
    let token = json!({"refresh_token":"old"});
    save_tokens(&s, &token).unwrap();
    let path = s.token_path();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // A directory at the destination makes the final rename fail even as root.
    let backup = path.with_extension("backup");
    fs::rename(&path, &backup).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(save_tokens(&s, &json!({"refresh_token":"new"})).is_err());
    assert!(path.is_dir());
    assert!(!fs::read_dir(&s.data_dir).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".strava-tokens-")));
    fs::remove_dir(&path).unwrap();
    symlink(&backup, &path).unwrap();
    assert_eq!(load_tokens(&s).unwrap(), Some(token));
}

#[test]
fn calendar_totals_start_at_local_midnight_across_month_boundaries() {
    let (_dir, s) = isolated_db();
    for (id, utc) in [
        (1, "2026-07-26T23:59:59Z"),
        (2, "2026-07-27T00:00:00Z"),
        (3, "2026-08-01T00:00:00Z"),
        (4, "2026-08-02T23:59:59Z"),
    ] {
        seed(&s, id, utc);
        store(&s, id, 8000 + id as i32);
    }
    assert_eq!(
        compute_period_totals(&s, "2026-08-02T10:00:00+02:00").unwrap(),
        (1, 2)
    );
    assert_eq!(
        compute_period_totals(&s, "2026-08-03T10:00:00-05:00").unwrap(),
        (2, 0)
    );
}

#[test]
fn poll_boundary_prefers_cursor_over_latest_stored_activity() {
    use tileharvester::{
        db::set_setting,
        sync::{parse_utc_datetime, sync_after_timestamp, LAST_SUCCESSFUL_SYNC_KEY},
    };
    let (_dir, mut s) = isolated_db();
    s.sync_lookback_days = 1;
    seed(&s, 1, "2026-08-05T10:00:00Z");
    let now = parse_utc_datetime("2026-08-07T10:00:00Z").unwrap();
    let timestamp = |utc| parse_utc_datetime(utc).unwrap().timestamp();
    assert_eq!(
        sync_after_timestamp(&s, now).unwrap(),
        timestamp("2026-08-04T10:00:00Z")
    );
    set_setting(
        &s,
        LAST_SUCCESSFUL_SYNC_KEY,
        &json!("2026-08-06T12:00:00+02:00"),
    )
    .unwrap();
    assert_eq!(
        sync_after_timestamp(&s, now).unwrap(),
        timestamp("2026-08-05T10:00:00Z")
    );
}
