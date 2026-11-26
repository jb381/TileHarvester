mod common;

use chrono::Utc;
use common::{
    isolated_db,
    mock_strava::{ActivityRequest, MockStravaApi},
};
use serde_json::{json, Value};
use tileharvester::{
    backfill::backfill,
    cli::{parse_from, run_cli_with},
    config::Settings,
    db::{get_activity_row, get_db, get_setting, ActivityRow},
    recompute::{recompute_all, recompute_novelty_from_stored_tiles},
    refine::refine_streams,
    sync::{
        clean_stream_segments, compute_activity_tiles, retry_failed, store_summary, sync_once,
        LAST_SUCCESSFUL_SYNC_KEY, STREAM_KEYS,
    },
};

fn summary(id: i64, sport: &str, route: Option<&str>) -> Value {
    let now = Utc::now() - chrono::Duration::seconds(60 - id.min(59));
    json!({"id":id,"start_date":now.to_rfc3339(),"start_date_local":now.naive_utc().format("%Y-%m-%dT%H:%M:%S").to_string(),
        "sport_type":sport,"map":{"summary_polyline":route}})
}

fn route(lat: f64, lon: f64) -> String {
    polyline::encode_coordinates([(lon, lat).into(), (lon, lat + 0.0001).into()], 5).unwrap()
}

fn stream(lat: f64, lon: f64) -> Value {
    json!({"latlng":{"data":[[lat,lon],[lat+0.0001,lon]]},"time":{"data":[0,1]}})
}

fn put(settings: &Settings, activity: &Value) {
    store_summary(settings, &get_db(settings).unwrap(), activity).unwrap();
}

fn row(settings: &Settings, id: i64) -> ActivityRow {
    get_activity_row(&get_db(settings).unwrap(), id)
        .unwrap()
        .unwrap()
}

fn tiles(settings: &Settings, id: i64) -> Vec<(String, i64)> {
    let conn = get_db(settings).unwrap();
    let mut query = conn
        .prepare("SELECT tile_id,is_new FROM activity_tiles WHERE activity_id=? ORDER BY tile_id")
        .unwrap();
    query
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn invoke(settings: &Settings, api: &MockStravaApi, args: &[&str]) -> (i32, String) {
    let cli = parse_from(std::iter::once("tileharvester").chain(args.iter().copied())).unwrap();
    let mut output = Vec::new();
    let exit = run_cli_with(cli, settings.clone(), api, &mut &b""[..], &mut output);
    (exit, String::from_utf8(output).unwrap())
}

#[test]
fn paginated_backfill_respects_limit_and_uses_both_tile_sources() {
    let (_dir, s) = isolated_db();
    let mut page = (1..=200)
        .map(|id| summary(id, "VirtualRide", None))
        .collect::<Vec<_>>();
    page[0] = summary(1, "Ride", Some(&route(52.0, 5.0)));
    page[1] = summary(2, "Ride", None);
    page[2] = summary(3, "Ride", None);
    page.extend([
        summary(201, "VirtualRide", None),
        summary(202, "Ride", None),
    ]);
    let api = MockStravaApi::new()
        .with_activities(page)
        .with_streams(2, stream(52.0, 5.0));
    let result = backfill(&s, &api, Some(201)).unwrap();
    assert_eq!(
        api.activity_requests(),
        [1, 2].map(|page| ActivityRequest {
            page,
            per_page: 200,
            after: None,
            before: None
        })
    );
    assert_eq!(
        (result.processed, result.summary_processed, result.failed),
        (2, 1, 0)
    );
    assert_eq!(
        row(&s, 1)
            .tile_source
            .as_ref()
            .map(|source| source.as_str()),
        Some("summary_polyline")
    );
    assert_eq!(
        row(&s, 2)
            .tile_source
            .as_ref()
            .map(|source| source.as_str()),
        Some("streams_clean")
    );
    assert!(row(&s, 2).has_gps);
    assert_eq!(row(&s, 3).status.as_str(), "skipped_no_gps");
    assert_eq!(row(&s, 201).status.as_str(), "skipped_ignored_sport");
    assert!(get_activity_row(&get_db(&s).unwrap(), 202)
        .unwrap()
        .is_none());
    assert_eq!(
        api.stream_calls(),
        [2, 3].map(|id| (id, STREAM_KEYS.into()))
    );
}

#[test]
fn refinement_limits_force_and_failed_requests_preserve_valid_tiles() {
    let (_dir, mut s) = isolated_db();
    s.refine_default_limit = 1;
    let api = MockStravaApi::new().with_activities(vec![
        summary(1, "Ride", Some(&route(52.0, 5.0))),
        summary(2, "Ride", Some(&route(51.0, 5.0))),
    ]);
    assert_eq!(backfill(&s, &api, None).unwrap().processed, 2);
    let limited = refine_streams(&s, &api, None, false).unwrap();
    assert_eq!((limited.selected, limited.failed), (1, 1));
    let old = tiles(&s, 1);
    let api = MockStravaApi::new()
        .fail_streams_for(1)
        .with_streams(2, stream(50.0, 5.0));
    let result = refine_streams(&s, &api, Some(0), false).unwrap();
    assert_eq!((result.selected, result.refined, result.failed), (2, 1, 1));
    assert_eq!(row(&s, 1).status.as_str(), "processed");
    assert!(row(&s, 1).last_error.is_some());
    assert_eq!(tiles(&s, 1), old);
    assert_eq!(
        row(&s, 2)
            .tile_source
            .as_ref()
            .map(|source| source.as_str()),
        Some("streams_clean")
    );
    assert_eq!(refine_streams(&s, &api, Some(0), true).unwrap().selected, 2);
    assert_eq!(row(&s, 2).status.as_str(), "processed");
}

#[test]
fn sync_retries_annotations_without_reprocessing_and_preserves_cursor_on_fetch_failure() {
    let (_dir, s) = isolated_db();
    let api = MockStravaApi::new().with_activities(vec![summary(1, "Ride", None)]);
    let api = api.with_streams(1, stream(52.0, 5.0));
    let failing = api
        .with_description(1, "Ride notes")
        .fail_update_description();
    let first = sync_once(&s, &failing).unwrap();
    assert_eq!(
        (first.new_activities, first.processed, first.failed),
        (1, 1, 1)
    );
    assert_eq!(row(&s, 1).annotation_status.as_str(), "failed");
    let original = tiles(&s, 1);
    let api = MockStravaApi::new().with_description(1, "Ride notes");
    let second = sync_once(&s, &api).unwrap();
    assert_eq!(
        (second.processed, second.annotated, second.failed),
        (0, 1, 0)
    );
    assert_eq!(row(&s, 1).annotation_status.as_str(), "done");
    assert_eq!(tiles(&s, 1), original);
    assert!(api.current_descriptions()[&1].starts_with("Ride notes\n\n"));
    let third = sync_once(&s, &api).unwrap();
    assert_eq!(
        (third.new_activities, third.processed, third.annotated),
        (0, 0, 0)
    );
    assert_eq!(failing.stream_calls(), [(1, STREAM_KEYS.into())]);
    assert!(api.stream_calls().is_empty());
    assert_eq!(failing.annotation_writes().len(), 1);
    assert_eq!(api.annotation_writes().len(), 1);
    let cursor = get_setting(&s, LAST_SUCCESSFUL_SYNC_KEY).unwrap();
    assert!(cursor.is_some());
    let api = api.fail_fetch();
    assert!(sync_once(&s, &api).is_err());
    assert_eq!(get_setting(&s, LAST_SUCCESSFUL_SYNC_KEY).unwrap(), cursor);
}

#[test]
fn changed_route_retains_old_history_until_next_poll_succeeds() {
    let (_dir, s) = isolated_db();
    let mut activity = summary(1, "Ride", Some(&route(52.0, 5.0)));
    let api = MockStravaApi::new().with_activities(vec![activity.clone()]);
    assert_eq!(backfill(&s, &api, None).unwrap().processed, 1);
    let old = tiles(&s, 1);
    activity["map"]["summary_polyline"] = json!(route(50.0, 5.0));
    let api = MockStravaApi::new().with_activities(vec![activity]);
    let api = api.fail_streams_for(1);
    let (exit, output) = invoke(&s, &api, &["sync", "--once"]);
    assert_eq!(exit, 1, "{output}");
    assert!(
        output.contains("1 processing or annotation failures"),
        "{output}"
    );
    let stored = row(&s, 1);
    assert_eq!(stored.status.as_str(), "processed");
    assert!(stored.stream_refresh_pending);
    assert_eq!(tiles(&s, 1), old);
    let api = MockStravaApi::new().with_streams(1, stream(50.0, 5.0));
    assert_eq!(sync_once(&s, &api).unwrap().processed, 1);
    assert!(!row(&s, 1).stream_refresh_pending);
    assert_ne!(tiles(&s, 1), old);
    assert_eq!(sync_once(&s, &api).unwrap().processed, 0);
}

#[test]
fn retry_handles_failed_routes_pending_refresh_and_failed_annotations() {
    let (_dir, s) = isolated_db();
    let mut refresh = summary(2, "Ride", Some(&route(52.0, 5.0)));
    let api = MockStravaApi::new().with_activities(vec![refresh.clone()]);
    assert_eq!(backfill(&s, &api, None).unwrap().processed, 1);
    let api = MockStravaApi::new().with_activities(vec![summary(1, "Ride", None), refresh.clone()]);
    let api = api.fail_streams_for(1);
    let failing = api.fail_update_description();
    assert_eq!(sync_once(&s, &failing).unwrap().failed, 2);
    refresh["map"]["summary_polyline"] = json!(route(50.0, 5.0));
    put(&s, &refresh);
    assert!(row(&s, 2).stream_refresh_pending);
    assert_eq!(row(&s, 1).status.as_str(), "failed");
    assert_eq!(row(&s, 2).annotation_status.as_str(), "failed");
    let api = MockStravaApi::new()
        .with_streams(1, stream(52.0, 5.0))
        .with_streams(2, stream(50.0, 5.0));
    let result = retry_failed(&s, &api).unwrap();
    assert_eq!((result.retried, result.success), (3, 3));
    assert!(!row(&s, 2).stream_refresh_pending);
    assert_eq!(row(&s, 2).annotation_status.as_str(), "done");
    assert_eq!(row(&s, 1).status.as_str(), "processed");
    assert_eq!(retry_failed(&s, &api).unwrap().retried, 0);
}

#[test]
fn unsuccessful_processing_is_retried_on_next_poll_even_without_new_summaries() {
    let (_dir, s) = isolated_db();
    let api = MockStravaApi::new().with_activities(vec![summary(1, "Ride", None)]);
    let api = api.fail_streams_for(1);
    assert_eq!(sync_once(&s, &api).unwrap().failed, 1);
    let api = MockStravaApi::new().with_streams(1, stream(52.0, 5.0));
    let result = sync_once(&s, &api).unwrap();
    assert_eq!(
        (result.new_activities, result.processed, result.annotated),
        (0, 1, 1)
    );
}

#[test]
fn recompute_preserves_streams_rebuilds_summary_and_requeues_allowed_sports() {
    let (_dir, mut s) = isolated_db();
    let api = MockStravaApi::new().with_activities(vec![
        summary(1, "Ride", None),
        summary(2, "Ride", Some(&route(52.0, 5.0))),
        summary(3, "VirtualRide", None),
    ]);
    let api = api.with_streams(1, stream(52.0, 5.0));
    assert_eq!(backfill(&s, &api, None).unwrap().processed, 2);
    assert_eq!(sync_once(&s, &api).unwrap().annotated, 1);
    assert_eq!(row(&s, 1).annotation_status.as_str(), "done");
    assert_eq!(row(&s, 2).annotation_status.as_str(), "skipped");
    let original = tiles(&s, 1);
    let old_summary = tiles(&s, 2);
    let conn = get_db(&s).unwrap();
    conn.execute(
        "UPDATE activities SET summary_polyline=? WHERE id=2",
        [route(50.0, 5.0)],
    )
    .unwrap();
    conn.execute("UPDATE activities SET sport_type='Ride' WHERE id=3", [])
        .unwrap();
    let result = recompute_all(&s).unwrap();
    assert_eq!((result.preserved, result.rebuilt, result.failed), (1, 1, 0));
    assert_eq!(tiles(&s, 1), original);
    assert_ne!(tiles(&s, 2), old_summary);
    assert_eq!(row(&s, 3).status.as_str(), "pending");
    s.rewrite_existing_annotations = true;
    assert_eq!(recompute_novelty_from_stored_tiles(&s).unwrap().rebuilt, 2);
    assert_eq!(row(&s, 1).annotation_status.as_str(), "none");
    assert_eq!(row(&s, 2).annotation_status.as_str(), "none");
}

#[test]
fn malformed_and_oversized_streams_fail_per_activity_without_partial_tiles() {
    let (_dir, mut s) = isolated_db();
    // Nonrepresentable JSON numbers are rejected by serde_json before stream cleaning.
    assert!(serde_json::from_str::<Value>(&format!("1{}", "0".repeat(1000))).is_err());
    let payloads = [
        Value::Null,
        json!([]),
        json!({"latlng":null}),
        json!({"latlng":{"data":null}}),
        json!({"latlng":{"data":[{"lat":52,"lon":5}]}}),
        json!({"time":null}),
        json!({"latlng":{"data":[[52,5],[52,5]]},"time":{"data":[null,{}]}}),
        json!({"latlng":{"data":[[1e300,5],[52,5]]}}),
    ];
    for (index, payload) in payloads.into_iter().enumerate() {
        let id = index as i64 + 1;
        put(&s, &summary(id, "Ride", None));
        assert!(
            clean_stream_segments(&s, &payload).is_err(),
            "payload {index}"
        );
        // The shared API filters out non-object responses before returning requested streams.
        let payload = if payload.is_object() {
            payload
        } else {
            json!({"latlng":null})
        };
        let api = MockStravaApi::new().with_streams(id, payload);
        assert_eq!(
            compute_activity_tiles(&s, &api, id)
                .unwrap()
                .status
                .as_str(),
            "failed",
            "payload {index}"
        );
        assert_eq!(row(&s, id).status.as_str(), "failed");
        assert!(tiles(&s, id).is_empty());
    }
    s.stream_max_points = 2;
    put(&s, &summary(99, "Ride", None));
    let api = MockStravaApi::new().with_streams(
        99,
        json!({"latlng":{"data":[[52,5],[52.0001,5],[52.0002,5]]}}),
    );
    let result = compute_activity_tiles(&s, &api, 99).unwrap();
    assert_eq!(result.status.as_str(), "failed");
    assert!(result.error.unwrap().contains("TH_STREAM_MAX_POINTS"));
    assert!(tiles(&s, 99).is_empty());

    let api = api.with_streams(123, json!({"latlng":null}));
    let (exit, output) = invoke(&s, &api, &["validate", "123"]);
    assert_eq!(exit, 1, "{output}");
    assert!(output.contains("Invalid GPS stream"), "{output}");
}

#[test]
fn invalid_summary_does_not_abort_backfill() {
    let (_dir, s) = isolated_db();
    let api = MockStravaApi::new().with_activities(vec![
        summary(1, "Ride", Some("_mljP_qo]")),
        summary(2, "Ride", Some(&route(52.0, 5.0))),
    ]);
    let result = backfill(&s, &api, None).unwrap();
    assert_eq!((result.failed, result.processed), (1, 1));
    assert!(row(&s, 1).last_error.unwrap().contains("Invalid route"));
    assert!(tiles(&s, 1).is_empty());
    assert!(!tiles(&s, 2).is_empty());
}

#[test]
fn sport_changes_reconcile_history_and_reactivate_without_summary() {
    let (_dir, s) = isolated_db();
    let mut early = summary(1, "Ride", None);
    put(&s, &early);
    put(&s, &summary(2, "Ride", None));
    let api = MockStravaApi::new()
        .with_streams(1, stream(52.0, 5.0))
        .with_streams(2, stream(52.0, 5.0));
    compute_activity_tiles(&s, &api, 2).unwrap();
    compute_activity_tiles(&s, &api, 1).unwrap();
    let counts = || (row(&s, 1).new_squadrat_count, row(&s, 2).new_squadrat_count);
    assert_eq!(counts(), (1, 0));
    early["sport_type"] = json!("VirtualRide");
    put(&s, &early);
    assert_eq!(counts(), (0, 1));
    early["sport_type"] = json!("Ride");
    put(&s, &early);
    assert_eq!(counts(), (1, 0));
    assert_eq!(row(&s, 1).status.as_str(), "processed");
}

#[test]
fn stream_cleaning_splits_gaps_and_accepts_missing_times() {
    let (_dir, s) = isolated_db();
    let (segments, stats) =
        clean_stream_segments(&s, &json!({"latlng":{"data":[[52,5],[52.0001,5],[53,5]]}})).unwrap();
    assert_eq!(segments, [vec![(52.0, 5.0), (52.0001, 5.0)]]);
    assert_eq!((stats.spikes_dropped, stats.splits), (1, 0));
    let (segments, stats) = clean_stream_segments(
        &s,
        &json!({"latlng":{"data":[[52,5],[52.0001,5],[53,5],[53.0001,5]]}}),
    )
    .unwrap();
    assert_eq!(segments.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2]);
    assert_eq!((stats.spikes_dropped, stats.splits), (0, 1));
    let (segments, stats) =
        clean_stream_segments(&s, &json!({"latlng":{"data":[[52,5]]}})).unwrap();
    assert!(segments.is_empty());
    assert!(stats.warning.is_some());
    let (segments, _) = clean_stream_segments(&s, &json!({"latlng":{"data":[]}})).unwrap();
    assert!(segments.is_empty());
}

#[test]
fn kml_baseline_suppresses_old_tiles_and_compare_audits_new_activity() {
    use tileharvester::kml_baseline::{compare_kml, import_baseline};
    let (_dir, s) = isolated_db();
    let snapshot = s.data_dir.join("snapshot.kml");
    let layer = |name: &str, edge: f64| {
        format!(
        "<Placemark><name>{name}</name><Polygon><outerBoundaryIs><LinearRing><coordinates>0,0 {edge},0 {edge},{edge} 0,{edge} 0,0</coordinates></LinearRing></outerBoundaryIs></Polygon></Placemark>"
    )
    };
    let coarse = layer("squadrats", 0.022);
    let fine = layer("squadratinhos", 0.00275);
    std::fs::write(
        &snapshot,
        format!("<kml><Document>{coarse}{fine}</Document></kml>"),
    )
    .unwrap();
    import_baseline(&s, &snapshot, Some("2020-01-01T00:00:00Z")).unwrap();

    let mut covered = summary(1, "Ride", Some(&route(0.001, 0.001)));
    covered["start_date"] = json!("2019-12-01T10:00:00Z");
    covered["start_date_local"] = json!("2019-12-01T10:00:00");
    put(&s, &covered);
    let api = MockStravaApi::new()
        .with_activities(vec![summary(2, "Ride", None), summary(3, "Ride", None)]);
    let api = api
        .with_streams(1, stream(0.001, 0.001))
        .with_streams(2, stream(0.001, 0.001))
        .with_streams(3, stream(0.001, 0.045));
    assert_eq!(sync_once(&s, &api).unwrap().processed, 3);
    assert!(row(&s, 1).baseline_covered);
    assert_eq!(row(&s, 2).new_squadrat_count, 0);
    assert_eq!(row(&s, 3).new_squadrat_count, 1);
    let audit = compare_kml(&s, &snapshot).unwrap();
    assert_eq!(
        (
            audit.squadrat.shared,
            audit.squadrat.kml_only,
            audit.squadrat.local_only
        ),
        (1, 0, 1)
    );
    assert_eq!(
        (
            audit.squadratinho.shared,
            audit.squadratinho.kml_only,
            audit.squadratinho.local_only
        ),
        (1, 0, 1)
    );
    assert_eq!(compare_kml(&s, &snapshot).unwrap(), audit);

    let api = api.with_activity(99, summary(99, "Ride", None));
    let api = api.with_streams(99, stream(0.001, 0.001));
    let (exit, output) = invoke(&s, &api, &["validate", "99"]);
    assert_eq!(exit, 0, "{output}");
    assert!(output.contains("Historical comparison"), "{output}");
    assert!(
        output.contains("Already-seen Squadrats on route:   1"),
        "{output}"
    );
    assert!(
        output.contains("New Squadrats from this activity:  0"),
        "{output}"
    );
    assert!(get_activity_row(&get_db(&s).unwrap(), 99)
        .unwrap()
        .is_none());
}

#[test]
fn later_recompute_failure_leaves_eligibility_and_history_consistent() {
    use tileharvester::kml_baseline::effective_tile_count;
    let (_dir, s) = isolated_db();
    let api = MockStravaApi::new().with_activities(vec![
        summary(1, "Ride", Some(&route(52.0, 5.0))),
        summary(2, "Ride", Some(&route(51.0, 5.0))),
    ]);
    assert_eq!(backfill(&s, &api, None).unwrap().processed, 2);
    let conn = get_db(&s).unwrap();
    conn.execute_batch(
        "UPDATE activities SET sport_type='VirtualRide' WHERE id=1;
        CREATE TRIGGER stop_route BEFORE DELETE ON activity_tiles WHEN OLD.activity_id=2
        BEGIN SELECT RAISE(ABORT, 'injected route failure'); END;",
    )
    .unwrap();
    let original = tiles(&s, 2);
    let error = recompute_all(&s).unwrap_err();
    assert!(error.to_string().contains("injected route failure"));
    assert_eq!(row(&s, 1).status.as_str(), "skipped_ignored_sport");
    assert!(tiles(&s, 1).iter().all(|(_, is_new)| *is_new == 0));
    assert_eq!(tiles(&s, 2), original);
    assert_eq!(effective_tile_count(&s, "squadrat").unwrap(), 1);
}

#[test]
fn annotation_checks_activity_state_and_skips_unchanged_description() {
    use tileharvester::annotate::annotate_activity;
    let (_dir, s) = isolated_db();
    let api = MockStravaApi::new();
    assert!(annotate_activity(&s, &api, 1).is_err());
    put(&s, &summary(1, "Ride", None));
    assert_eq!(
        annotate_activity(&s, &api, 1).unwrap().status.as_str(),
        "not_processed"
    );
    assert!(api.annotation_writes().is_empty());

    let api = api.with_streams(1, stream(52.0, 5.0));
    assert_eq!(
        compute_activity_tiles(&s, &api, 1).unwrap().status.as_str(),
        "processed"
    );
    let annotation = annotate_activity(&s, &api, 1).unwrap();
    assert_eq!(annotation.status.as_str(), "annotated");
    assert!(api.current_descriptions()[&1].ends_with(&annotation.line.unwrap()));
    annotate_activity(&s, &api, 1).unwrap();
    assert_eq!(
        api.annotation_writes()
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(row(&s, 1).annotation_status.as_str(), "done");
}

#[test]
fn annotation_uses_only_enabled_tile_counts_and_clears_stale_fields() {
    use tileharvester::{annotate::annotate_activity, models::AnnotateStatus};

    for (include_fine, coarse, fine, prior, expected) in [
        (false, 0, 1, "none", AnnotateStatus::Skipped),
        (false, 0, 1, "failed", AnnotateStatus::Skipped),
        (false, 1, 0, "failed", AnnotateStatus::Annotated),
        (true, 0, 1, "none", AnnotateStatus::Annotated),
        (true, 0, 0, "none", AnnotateStatus::Skipped),
    ] {
        let (_dir, mut s) = isolated_db();
        assert!(!s.include_squadratinhos_in_description);
        s.include_squadratinhos_in_description = include_fine;
        let initial = "Original Strava description";
        let api = MockStravaApi::new()
            .with_streams(1, stream(52.0, 5.0))
            .with_description(1, initial);
        put(&s, &summary(1, "Ride", Some(&route(52.0, 5.0))));
        compute_activity_tiles(&s, &api, 1).unwrap();
        get_db(&s)
            .unwrap()
            .execute(
                "UPDATE activities SET new_squadrat_count = ?1, new_squadratinho_count = ?2,
                 annotation_status = ?3, description_line = 'stale', last_error = 'old failure'
                 WHERE id = 1",
                rusqlite::params![coarse, fine, prior],
            )
            .unwrap();

        let result = annotate_activity(&s, &api, 1).unwrap();
        assert_eq!(result.status, expected);
        assert_eq!(result.activity_id, 1);
        assert!(result.error.is_none());
        let stored = row(&s, 1);
        assert!(stored.last_error.is_none());
        assert!(
            chrono::DateTime::parse_from_rfc3339(stored.annotated_at.as_deref().unwrap()).is_ok()
        );
        if expected == AnnotateStatus::Skipped {
            assert!(result.line.is_none());
            assert!(stored.description_line.is_none());
            assert_eq!(stored.annotation_status.as_str(), "skipped");
            assert!(api.activity_details_requests().is_empty());
            assert!(api.annotation_writes().is_empty());
            assert_eq!(api.current_descriptions()[&1], initial);
        } else {
            let line = result.line.unwrap();
            assert!(line.contains(&format!("· +{coarse} new ·")));
            if include_fine {
                assert!(line.ends_with("Squadratinhos (+1 new)"));
            }
            assert_eq!(stored.description_line.as_deref(), Some(line.as_str()));
            assert_eq!(stored.annotation_status.as_str(), "done");
            assert_eq!(api.activity_details_requests(), [1]);
            assert_eq!(api.annotation_writes().len(), 1);
            assert!(api.current_descriptions()[&1].ends_with(&line));
        }
    }
}

#[test]
fn sync_leaves_duplicate_route_skipped_on_later_runs_and_reports_it() {
    let (_dir, s) = isolated_db();
    let first = summary(1, "Ride", None);
    let duplicate = summary(2, "Ride", None);
    let api = MockStravaApi::new()
        .with_activities(vec![first])
        .with_streams(1, stream(52.0, 5.0));
    assert_eq!(sync_once(&s, &api).unwrap().annotated, 1);
    put(&s, &duplicate);
    let api = MockStravaApi::new().with_streams(2, stream(52.0, 5.0));
    let result = sync_once(&s, &api).unwrap();
    assert_eq!(
        (result.processed, result.annotated, result.failed),
        (1, 0, 0)
    );
    let skipped = row(&s, 2);
    assert_eq!(skipped.annotation_status.as_str(), "skipped");
    let result = sync_once(&s, &api).unwrap();
    assert_eq!(
        (result.processed, result.annotated, result.failed),
        (0, 0, 0)
    );
    assert_eq!(row(&s, 2).annotated_at, skipped.annotated_at);
    assert!(api.activity_details_requests().is_empty());
    assert!(api.annotation_writes().is_empty());
    let (exit, output) = invoke(&s, &api, &["status"]);
    assert_eq!(exit, 0);
    assert!(output
        .lines()
        .any(|line| line == format!("{:<27}1", "Annotation skipped:")));
}

#[test]
fn retry_resolves_failed_annotation_without_new_tiles() {
    let (_dir, s) = isolated_db();
    let polyline = route(52.0, 5.0);
    let api = MockStravaApi::new().with_activities(vec![
        summary(2, "Ride", Some(&polyline)),
        summary(1, "Ride", Some(&polyline)),
    ]);
    assert_eq!(backfill(&s, &api, None).unwrap().processed, 2);
    assert_eq!(row(&s, 2).new_squadrat_count, 0);
    get_db(&s)
        .unwrap()
        .execute(
            "UPDATE activities SET annotation_status = 'failed', last_error = 'old failure' WHERE id = 2",
            [],
        )
        .unwrap();

    let api = MockStravaApi::new();
    let result = retry_failed(&s, &api).unwrap();
    assert_eq!((result.retried, result.success), (1, 1));
    let stored = row(&s, 2);
    assert_eq!(stored.annotation_status.as_str(), "skipped");
    assert!(stored.last_error.is_none());
    assert!(api.activity_requests().is_empty());
    assert!(api.stream_calls().is_empty());
    assert!(api.activity_details_requests().is_empty());
    assert!(api.annotation_writes().is_empty());
}

fn summary_at(id: i64, days_ago: i64, sport: &str, route: Option<&str>) -> Value {
    let ts =
        Utc::now() - chrono::Duration::days(days_ago) - chrono::Duration::seconds(60 - id.min(59));
    json!({
        "id": id,
        "start_date": ts.to_rfc3339(),
        "start_date_local": ts.naive_utc().format("%Y-%m-%dT%H:%M:%S").to_string(),
        "sport_type": sport,
        "map": {"summary_polyline": route}
    })
}

#[test]
fn first_sync_auto_backfills_history_and_annotates_only_recent_rides() {
    use tileharvester::sync::INITIAL_BACKFILL_DONE_KEY;

    let (_dir, s) = isolated_db();
    let api = MockStravaApi::new()
        .with_activities(vec![
            summary_at(3, 0, "Ride", Some(&route(52.0, 5.0))),
            summary_at(2, 30, "Ride", Some(&route(51.0, 5.0))),
            summary_at(1, 60, "Ride", Some(&route(50.0, 5.0))),
        ])
        .with_streams(1, stream(50.0, 5.0))
        .with_streams(2, stream(51.0, 5.0))
        .with_streams(3, stream(52.0, 5.0));

    let first = sync_once(&s, &api).unwrap();
    assert!(first.backfilled);
    assert_eq!(
        (
            first.new_activities,
            first.processed,
            first.refined,
            first.unrefined_remaining,
            first.annotated,
            first.failed
        ),
        (3, 3, 3, 0, 1, 0)
    );
    assert_eq!(
        get_setting(&s, INITIAL_BACKFILL_DONE_KEY).unwrap(),
        Some(json!(true))
    );
    // First sync used unfiltered backfill pagination (after: None).
    assert!(api
        .activity_requests()
        .iter()
        .all(|req| req.after.is_none()));
    // Today's ride was annotated with the full 3-Squadrat lifetime total (not just 1!).
    assert!(api.current_descriptions()[&3].contains("3 Squadrats · +1 new"));
    assert_eq!(row(&s, 1).annotation_status.as_str(), "none");
    assert_eq!(row(&s, 2).annotation_status.as_str(), "none");
    assert_eq!(row(&s, 3).annotation_status.as_str(), "done");
    for id in 1..=3 {
        assert_eq!(
            row(&s, id).tile_source.as_ref().map(|src| src.as_str()),
            Some("streams_clean")
        );
    }

    // Second sync does NOT backfill again and switches to incremental `after` cursor.
    let api2 = MockStravaApi::new();
    let second = sync_once(&s, &api2).unwrap();
    assert!(!second.backfilled);
    assert!(api2
        .activity_requests()
        .iter()
        .all(|req| req.after.is_some()));
}

#[test]
fn interrupted_auto_backfill_resumes_on_next_sync() {
    use tileharvester::sync::INITIAL_BACKFILL_DONE_KEY;

    let (_dir, s) = isolated_db();
    let mut activities: Vec<Value> = (1..=200)
        .map(|id| summary_at(id, 40, "VirtualRide", None))
        .collect();
    activities[0] = summary_at(1, 40, "Ride", Some(&route(50.0, 5.0)));
    activities.push(summary_at(201, 20, "Ride", Some(&route(51.0, 5.0))));

    // Page 1 succeeds and stores 200 rows; page 2 fails mid-backfill.
    let failing = MockStravaApi::new()
        .with_activities(activities.clone())
        .fail_fetch_page(2);
    assert!(sync_once(&s, &failing).is_err());
    assert_eq!(
        get_setting(&s, INITIAL_BACKFILL_DONE_KEY).unwrap(),
        Some(json!(false))
    );
    assert!(get_activity_row(&get_db(&s).unwrap(), 1).unwrap().is_some());
    assert!(get_activity_row(&get_db(&s).unwrap(), 201)
        .unwrap()
        .is_none());

    // Next sync detects incomplete backfill and completes all pages cleanly.
    let healthy = MockStravaApi::new()
        .with_activities(activities)
        .with_streams(1, stream(50.0, 5.0))
        .with_streams(201, stream(51.0, 5.0));
    let recovered = sync_once(&s, &healthy).unwrap();
    assert!(recovered.backfilled);
    assert_eq!(
        get_setting(&s, INITIAL_BACKFILL_DONE_KEY).unwrap(),
        Some(json!(true))
    );
    assert_eq!(row(&s, 201).status.as_str(), "processed");
    assert_eq!(row(&s, 1).status.as_str(), "processed");
}

#[test]
fn auto_refine_trickles_newest_first_and_respects_rate_limit_budget_and_429() {
    use tileharvester::strava_client::{RateLimitInfo, RateLimitSnapshot};

    let (_dir, mut s) = isolated_db();
    s.auto_refine = true;
    s.auto_refine_per_cycle = 2;
    s.auto_refine_rate_limit_reserve_pct = 30;

    let activities = vec![
        summary_at(5, 10, "Ride", Some(&route(52.0, 5.0))),
        summary_at(4, 20, "Ride", Some(&route(51.0, 5.0))),
        summary_at(3, 30, "Ride", Some(&route(50.0, 5.0))),
        summary_at(2, 40, "Ride", Some(&route(49.0, 5.0))),
        summary_at(1, 50, "Ride", Some(&route(48.0, 5.0))),
    ];
    let bf_api = MockStravaApi::new().with_activities(activities);
    assert_eq!(backfill(&s, &bf_api, None).unwrap().processed, 5);

    // Cycle 1: refines newest 2 (4 & 5, processed chronologically 4 -> 5), leaves 3 remaining.
    let api1 = MockStravaApi::new()
        .with_streams(1, stream(48.0, 5.0))
        .with_streams(2, stream(49.0, 5.0))
        .with_streams(3, stream(50.0, 5.0))
        .with_streams(4, stream(51.0, 5.0))
        .with_streams(5, stream(52.0, 5.0));
    let r1 = sync_once(&s, &api1).unwrap();
    assert_eq!((r1.refined, r1.unrefined_remaining, r1.failed), (2, 3, 0));
    assert_eq!(
        api1.stream_calls(),
        [(4, STREAM_KEYS.into()), (5, STREAM_KEYS.into())]
    );

    // Cycle 2: rate limit below 30% reserve (75/100 used) => skips auto-refine cleanly with 0 stream calls.
    let tight_quota = RateLimitSnapshot {
        overall: RateLimitInfo {
            fifteen_min_limit: Some(200),
            fifteen_min_used: Some(10),
            daily_limit: Some(2000),
            daily_used: Some(100),
        },
        read: RateLimitInfo {
            fifteen_min_limit: Some(100),
            fifteen_min_used: Some(75),
            daily_limit: Some(1000),
            daily_used: Some(200),
        },
    };
    let api2 = MockStravaApi::new().with_last_rate_limit(tight_quota);
    let r2 = sync_once(&s, &api2).unwrap();
    assert_eq!((r2.refined, r2.unrefined_remaining, r2.failed), (0, 3, 0));
    assert!(api2.stream_calls().is_empty());

    // Cycle 3: 429 on first ride in batch (ride 2) aborts batch immediately without calling ride 3.
    let api3 = MockStravaApi::new()
        .fail_streams_with_status(2, 429)
        .with_streams(3, stream(50.0, 5.0));
    let r3 = sync_once(&s, &api3).unwrap();
    assert_eq!((r3.refined, r3.unrefined_remaining, r3.failed), (0, 3, 0));
    assert_eq!(api3.stream_calls(), [(2, STREAM_KEYS.into())]);

    // Cycle 4: healthy quota finishes 2 & 3, then Cycle 5 finishes ride 1.
    let api4 = MockStravaApi::new()
        .with_streams(1, stream(48.0, 5.0))
        .with_streams(2, stream(49.0, 5.0))
        .with_streams(3, stream(50.0, 5.0));
    let r4 = sync_once(&s, &api4).unwrap();
    assert_eq!((r4.refined, r4.unrefined_remaining), (2, 1));
    let r5 = sync_once(&s, &api4).unwrap();
    assert_eq!((r5.refined, r5.unrefined_remaining), (1, 0));

    // Disabling auto-refine skips refinement even when unrefined rides exist.
    s.auto_refine = false;
    put(&s, &summary_at(6, 60, "Ride", Some(&route(47.0, 5.0))));
    let api6 = MockStravaApi::new().with_streams(6, stream(47.0, 5.0));
    let r6 = sync_once(&s, &api6).unwrap();
    assert_eq!(r6.processed, 1); // pending activity processed directly via streams
    assert_eq!(r6.refined, 0);
}

#[test]
fn sync_auto_recomputes_when_ignored_sport_types_change() {
    use tileharvester::kml_baseline::effective_tile_count;

    let (_dir, mut s) = isolated_db();
    let api = MockStravaApi::new()
        .with_activities(vec![
            summary_at(1, 20, "Ride", Some(&route(52.0, 5.0))),
            summary_at(2, 15, "Swim", Some(&route(51.0, 5.0))),
        ])
        .with_streams(1, stream(52.0, 5.0))
        .with_streams(2, stream(51.0, 5.0));
    let first = sync_once(&s, &api).unwrap();
    assert!(!first.auto_recomputed);
    assert_eq!(effective_tile_count(&s, "squadrat").unwrap(), 2);

    // Add Swim to ignored sports -> next sync auto-recomputes without manual command.
    s.ignored_sport_types = "VirtualRide,VirtualRun,Swim".to_string();
    let idle = MockStravaApi::new();
    let second = sync_once(&s, &idle).unwrap();
    assert!(second.auto_recomputed);
    assert_eq!(row(&s, 2).status.as_str(), "skipped_ignored_sport");
    assert_eq!(effective_tile_count(&s, "squadrat").unwrap(), 1);

    // Next sync with unchanged ignored sports does not recompute again.
    let third = sync_once(&s, &idle).unwrap();
    assert!(!third.auto_recomputed);

    // Removing Swim restores it automatically.
    s.ignored_sport_types = "VirtualRide,VirtualRun".to_string();
    let fourth = sync_once(&s, &idle).unwrap();
    assert!(fourth.auto_recomputed);
    assert_eq!(row(&s, 2).status.as_str(), "processed");
    assert_eq!(effective_tile_count(&s, "squadrat").unwrap(), 2);
}

#[test]
fn baseline_skips_auto_backfill_and_excludes_covered_rides_from_auto_refine() {
    use tileharvester::kml_baseline::import_baseline;

    let (_dir, s) = isolated_db();
    let snapshot = s.data_dir.join("snapshot.kml");
    let layer = |name: &str, edge: f64| {
        format!(
            "<Placemark><name>{name}</name><Polygon><outerBoundaryIs><LinearRing><coordinates>0,0 {edge},0 {edge},{edge} 0,{edge} 0,0</coordinates></LinearRing></outerBoundaryIs></Polygon></Placemark>"
        )
    };
    std::fs::write(
        &snapshot,
        format!(
            "<kml><Document>{}{}</Document></kml>",
            layer("squadrats", 0.022),
            layer("squadratinhos", 0.00275)
        ),
    )
    .unwrap();
    import_baseline(&s, &snapshot, Some("2026-01-01T00:00:00Z")).unwrap();

    let api = MockStravaApi::new();
    let first = sync_once(&s, &api).unwrap();
    assert!(!first.backfilled);
    assert!(api
        .activity_requests()
        .iter()
        .all(|req| req.after.is_some()));

    // Insert a pre-baseline summary-only activity; auto-refine & status should skip it.
    let mut old = summary(10, "Ride", Some(&route(0.001, 0.001)));
    old["start_date"] = json!("2025-06-01T10:00:00Z");
    old["start_date_local"] = json!("2025-06-01T10:00:00");
    put(&s, &old);
    tileharvester::sync::compute_activity_tiles_from_summary(&s, &api, 10).unwrap();
    assert_eq!(
        row(&s, 10).tile_source.as_ref().map(|src| src.as_str()),
        Some("summary_polyline")
    );
    let second = sync_once(&s, &api).unwrap();
    assert_eq!((second.refined, second.unrefined_remaining), (0, 0));
    assert!(api.stream_calls().is_empty());
}

#[test]
fn setup_wizard_persists_credentials_authenticates_and_bootstraps_history() {
    let (_dir, mut s) = isolated_db();
    s.strava_client_id.clear();
    s.strava_client_secret.clear();
    let api = MockStravaApi::new()
        .with_authenticated(false)
        .with_activities(vec![summary_at(1, 10, "Ride", Some(&route(52.0, 5.0)))]);

    let cli = parse_from([
        "tileharvester",
        "setup",
        "--no-open-browser",
        "--no-listen",
        "--no-service",
    ])
    .unwrap();
    let mut output = Vec::new();
    let input = b"12345\nmy-secret\nhttp://localhost:8000/callback?code=auth-code\n";
    let exit = run_cli_with(cli, s.clone(), &api, &mut &input[..], &mut output);
    let text = String::from_utf8(output).unwrap();
    assert_eq!(exit, 0, "{text}");
    assert!(text.contains("Saved Strava credentials"), "{text}");
    assert!(text.contains("Authenticated successfully"), "{text}");
    assert!(
        text.contains("Backfill complete: 1 stored, 1 processed"),
        "{text}"
    );
    assert!(
        text.contains("refine to full-GPS accuracy automatically"),
        "{text}"
    );

    // Credentials were persisted into <data_dir>/.env and reload cleanly.
    let reloaded = Settings::load_with_sources(
        None,
        &[(
            "TH_DATA_DIR".to_string(),
            s.data_dir.to_string_lossy().into_owned(),
        )]
        .into(),
    )
    .unwrap();
    assert_eq!(reloaded.strava_client_id, "12345");
    assert_eq!(reloaded.strava_client_secret, "my-secret");

    // Re-running setup when already configured & authenticated is idempotent.
    let authed_api = MockStravaApi::new().with_authenticated(true);
    let (exit, text) = invoke(&reloaded, &authed_api, &["setup", "--no-service"]);
    assert_eq!(exit, 0, "{text}");
    assert!(
        text.contains("Strava credentials: already configured."),
        "{text}"
    );
    assert!(
        text.contains("Strava authentication: already connected."),
        "{text}"
    );
    assert!(
        text.contains("Local tile history: already initialized."),
        "{text}"
    );
}
