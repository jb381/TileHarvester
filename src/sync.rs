//! Activity ingestion, route processing, and incremental synchronization.

use std::collections::HashSet;

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Params, Transaction, TransactionBehavior};
use serde_json::Value;

use crate::annotate::annotate_activity_with_conn;
use crate::config::Settings;
use crate::db::{
    get_activity_row, get_db, get_setting_with_conn, set_setting_with_conn, ActivityRow,
};
use crate::error::{Error, Result};
use crate::history::{rebuild_tile_history, AffectedTiles};
use crate::kml_baseline::{
    activity_is_covered, activity_uses_baseline, baseline_tiles_on_route,
    effective_tile_count_through, get_baseline,
};
use crate::models::{ActivityStatus, AnnotateStatus, TileComputeStatus, TileKind, TileSource};
use crate::strava_client::{classify_strava_error, StravaApi};
use crate::tile_engine::{make_engine, TileEngine};

pub const STREAM_KEYS: &str = "latlng,time,moving,distance";
pub const LAST_SUCCESSFUL_SYNC_KEY: &str = "last_successful_sync_at";
const PAGE_SIZE: u32 = 200;

fn parse_naive(value: &str) -> Result<NaiveDateTime> {
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H",
        "%Y-%m-%d %H",
    ] {
        if let Ok(date) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(date);
        }
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .ok_or_else(|| Error::Validation(format!("Invalid timestamp: {value}")))
}

pub fn parse_local_datetime(local_str: &str) -> Result<NaiveDateTime> {
    let value = local_str.strip_suffix('Z').unwrap_or(local_str);
    let value = value.rsplit_once('+').map_or(value, |(local, _)| local);
    let value = if let Some((before, _)) = value.rsplit_once(':') {
        before
            .rfind('-')
            .filter(|index| *index > 9)
            .map_or(value, |index| &value[..index])
    } else {
        value
    };
    parse_naive(value)
}

pub fn parse_utc_datetime(utc_str: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(utc_str)
        .or_else(|_| DateTime::parse_from_str(utc_str, "%Y-%m-%dT%H:%M:%S%.f%#z"))
        .or_else(|_| DateTime::parse_from_str(utc_str, "%Y-%m-%d %H:%M:%S%.f%#z"))
        .map(|date| date.to_utc())
        .or_else(|_| parse_naive(utc_str).map(|date| date.and_utc()))
}

pub fn latest_stored_activity_utc(settings: &Settings) -> Result<Option<DateTime<Utc>>> {
    latest_stored_activity_utc_with_conn(&get_db(settings)?)
}

pub(crate) fn latest_stored_activity_utc_with_conn(
    conn: &Connection,
) -> Result<Option<DateTime<Utc>>> {
    let value: Option<String> = conn
        .query_row(
            "SELECT start_utc FROM activities WHERE julianday(start_utc) IS NOT NULL
         ORDER BY julianday(start_utc) DESC, id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.and_then(|value| parse_utc_datetime(&value).ok()))
}

fn days_before(date: DateTime<Utc>, days: i64) -> Result<DateTime<Utc>> {
    Duration::try_days(days)
        .and_then(|duration| date.checked_sub_signed(duration))
        .ok_or_else(|| Error::Validation(format!("Date offset out of range: {days} days")))
}

pub fn sync_after_timestamp(settings: &Settings, sync_started_at: DateTime<Utc>) -> Result<i64> {
    sync_after_timestamp_with_conn(settings, &get_db(settings)?, sync_started_at)
}

pub(crate) fn sync_after_timestamp_with_conn(
    settings: &Settings,
    conn: &Connection,
    sync_started_at: DateTime<Utc>,
) -> Result<i64> {
    let cursor = get_setting_with_conn(conn, LAST_SUCCESSFUL_SYNC_KEY)?
        .and_then(|value| parse_utc_datetime(value.as_str()?).ok());
    let anchor = match cursor {
        Some(cursor) => cursor,
        None => latest_stored_activity_utc_with_conn(conn)?.unwrap_or(sync_started_at),
    };
    let after = days_before(anchor.min(sync_started_at), settings.sync_lookback_days)?;
    Ok(after.timestamp() + i64::from(after.timestamp() < 0 && after.timestamp_subsec_nanos() != 0))
}

pub type Point = (f64, f64);

#[derive(Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    pub points: usize,
    pub segments: usize,
    pub splits: usize,
    pub spikes_dropped: usize,
    pub stationary_dropped: usize,
    pub warning: Option<String>,
}

fn distance_meters(a: Point, b: Point) -> f64 {
    let dlat = (b.0 - a.0).to_radians();
    let dlon = (b.1 - a.1).to_radians();
    let h = (dlat / 2.0).sin().powi(2)
        + a.0.to_radians().cos() * b.0.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * h.sqrt().min(1.0).asin()
}

fn finite_number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

pub fn clean_stream_segments(
    settings: &Settings,
    streams: &Value,
) -> Result<(Vec<Vec<Point>>, StreamStats)> {
    let streams = streams
        .as_object()
        .ok_or_else(|| Error::Validation("GPS streams must be an object".into()))?;
    let data_for = |key| -> Result<&[Value]> {
        match streams.get(key) {
            None => Ok(&[]),
            Some(stream) => stream
                .get("data")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .ok_or_else(|| {
                    Error::Validation(format!("GPS {key} stream must contain a data array"))
                }),
        }
    };
    let coordinates = data_for("latlng")?;
    let times = data_for("time")?
        .iter()
        .map(finite_number)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| Error::Validation("GPS time stream must contain finite numbers".into()))?;
    let mut stats = StreamStats {
        points: coordinates.len(),
        ..Default::default()
    };
    if stats.points > settings.stream_max_points {
        return Err(Error::Validation(format!(
            "GPS stream has {} points, exceeding TH_STREAM_MAX_POINTS={}; increase the limit and retry",
            stats.points, settings.stream_max_points
        )));
    }
    let mut points = Vec::with_capacity(coordinates.len());
    for point in coordinates {
        let pair = point
            .as_array()
            .filter(|pair| pair.len() == 2)
            .ok_or_else(|| {
                Error::Validation("GPS coordinates must be latitude/longitude pairs".into())
            })?;
        let (Some(lat), Some(lon)) = (finite_number(&pair[0]), finite_number(&pair[1])) else {
            return Err(Error::Validation(
                "GPS coordinates must be finite numbers".into(),
            ));
        };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            return Err(Error::Validation(
                "GPS stream contains an invalid coordinate".into(),
            ));
        }
        points.push((lat, lon));
    }

    let moving_error = || Error::Validation("GPS moving stream must contain booleans".into());
    let moving = data_for("moving")
        .map_err(|_| moving_error())?
        .iter()
        .map(Value::as_bool)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(moving_error)?;
    let distance_error =
        || Error::Validation("GPS distance stream must contain finite numbers".into());
    let distances = data_for("distance")
        .map_err(|_| distance_error())?
        .iter()
        .map(finite_number)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(distance_error)?;

    let aligned_times = times.len() == points.len();
    let aligned_distances = distances.len() == points.len();
    let stationary_near = |from: usize, to: usize| {
        if aligned_distances {
            (distances[to] - distances[from]).abs() < 15.0
        } else {
            distance_meters(points[from], points[to]) <= settings.stream_gap_min_meters
        }
    };
    let first_moving = (moving.len() == points.len())
        .then(|| moving.iter().position(|&sample| sample))
        .flatten();
    let start = first_moving.map_or(0, |first| {
        // Only trim the stationary suffix immediately before motion begins.
        (0..first)
            .rfind(|&index| !stationary_near(index, first))
            .map_or(first, |index| index + 1)
    });
    stats.stationary_dropped = start;

    let is_jump = |from: usize, to: usize| {
        let chord = distance_meters(points[from], points[to]);
        let dt = if aligned_times {
            times[to] - times[from]
        } else {
            0.0
        };
        if chord > settings.stream_max_segment_meters {
            return !(aligned_distances
                && chord <= 1500.0
                && dt > 0.0
                && dt <= 300.0
                && chord / dt <= settings.stream_max_speed_mps
                && (0.80 * chord..=1.35 * chord).contains(&(distances[to] - distances[from])));
        }
        chord > settings.stream_gap_min_meters
            && (dt > settings.stream_max_time_gap_seconds
                || (dt > 0.0 && chord / dt > settings.stream_max_speed_mps))
    };
    let is_lateral_bounce = |from: usize, first: usize, last: usize, to: usize| {
        let out_time = times[first] - times[from];
        let back_time = times[to] - times[last];
        if !(out_time > 0.0 && out_time <= 3.0 && back_time > 0.0 && back_time <= 3.0) {
            return false;
        }
        let out_distance = distance_meters(points[from], points[first]);
        let back_distance = distance_meters(points[last], points[to]);
        let direct_distance = distance_meters(points[from], points[to]);
        let direct_time = times[to] - times[from];
        let max_speed = settings.stream_max_speed_mps;
        out_distance / out_time > max_speed
            && back_distance / back_time > max_speed
            && direct_distance < 0.5 * (out_distance + back_distance)
            && direct_time > 0.0
            && times[last] >= times[first]
            && direct_distance / direct_time <= max_speed
            && !is_jump(from, to)
    };
    let mut segments = Vec::new();
    let mut current = Vec::new();
    let mut previous = start;
    let mut index = start;
    while index < points.len() {
        if index > start {
            let is_stationary = |sample: usize| {
                first_moving.is_some() && !moving[sample] && stationary_near(previous, sample)
            };
            if is_stationary(index) {
                stats.stationary_dropped += 1;
                index += 1;
                continue;
            }
            if let Some(resume) = aligned_times
                .then(|| {
                    (index + 1..=(index + 2).min(points.len() - 1))
                        .find(|&resume| is_lateral_bounce(previous, index, resume - 1, resume))
                })
                .flatten()
            {
                stats.spikes_dropped += resume - index;
                index = resume;
                continue;
            }
            if is_jump(previous, index) {
                let mut burst = [0; 3];
                let mut burst_len = 0;
                let mut stationary = 0;
                let mut resume_index = index;
                for resume in index..points.len() {
                    if is_stationary(resume) {
                        stationary += 1;
                        continue;
                    }
                    if !is_jump(previous, resume) {
                        if burst[..burst_len].iter().all(|&mid| is_jump(mid, resume)) {
                            stats.spikes_dropped += burst_len;
                            stats.stationary_dropped += stationary;
                            resume_index = resume;
                        }
                        break;
                    }
                    if burst_len == burst.len() {
                        break;
                    }
                    burst[burst_len] = resume;
                    burst_len += 1;
                }
                index = resume_index;
                if is_jump(previous, index) {
                    segments.push(std::mem::take(&mut current));
                    stats.splits += 1;
                }
            }
        }
        current.push(points[index]);
        previous = index;
        index += 1;
    }
    if !current.is_empty() {
        segments.push(current);
        if segments.iter().all(|segment| segment.len() == 1) {
            segments.clear();
            stats.warning = Some("all points filtered — every segment is a single point".into());
        } else {
            while segments.last().is_some_and(|segment| segment.len() == 1) {
                segments.pop();
                stats.spikes_dropped += 1;
                stats.splits = stats.splits.saturating_sub(1);
            }
            let leading = segments
                .iter()
                .take_while(|segment| segment.len() == 1)
                .count();
            segments.drain(..leading);
            stats.spikes_dropped += leading;
            stats.splits = stats.splits.saturating_sub(leading);
        }
    }
    stats.segments = segments.len();
    Ok((segments, stats))
}

fn activity_tiles(conn: &Connection, id: i64) -> Result<AffectedTiles> {
    let mut statement =
        conn.prepare("SELECT tile_kind, tile_id FROM activity_tiles WHERE activity_id = ?")?;
    let mut affected = AffectedTiles::default();
    for row in statement.query_map([id], |row| Ok((row.get::<_, TileKind>(0)?, row.get(1)?)))? {
        let (kind, tile) = row?;
        affected.add(kind, tile);
    }
    Ok(affected)
}

pub fn store_summary(
    settings: &Settings,
    conn: &Connection,
    activity: &Value,
) -> Result<(bool, bool)> {
    let ignored_sports = settings.ignored_sports();
    if conn.is_autocommit() {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let result = store_summary_in_transaction(settings, &tx, activity, &ignored_sports)?;
        tx.commit()?;
        Ok(result)
    } else {
        store_summary_in_transaction(settings, conn, activity, &ignored_sports)
    }
}

fn store_summary_in_transaction(
    settings: &Settings,
    conn: &Connection,
    activity: &Value,
    ignored_sports: &HashSet<String>,
) -> Result<(bool, bool)> {
    let id = activity["id"]
        .as_i64()
        .ok_or_else(|| Error::Validation("Activity must have an integer id".into()))?;
    let summary = activity["map"]["summary_polyline"]
        .as_str()
        .filter(|value| !value.is_empty());
    let sport = activity["sport_type"].as_str().unwrap_or_default();
    let ignored = !sport.is_empty() && ignored_sports.contains(sport);
    let Some(existing) = get_activity_row(conn, id)? else {
        let required = |key: &str| {
            activity[key]
                .as_str()
                .ok_or_else(|| Error::Validation(format!("Activity {id} must have a {key} string")))
        };
        let start_utc = required("start_date")?;
        conn.execute(
            "INSERT INTO activities (id, start_utc, start_local, timezone, sport_type, summary_polyline,
             has_gps, status, tile_engine, baseline_covered) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![id, start_utc, required("start_date_local")?, activity["timezone"].as_str().unwrap_or_default(),
                sport, summary, summary.is_some(), if ignored { ActivityStatus::SkippedIgnoredSport } else { ActivityStatus::Pending },
                make_engine(settings).id(), activity_is_covered(settings, conn, start_utc)?],
        )?;
        return Ok((true, ignored));
    };

    let route_changed = summary.is_some() && summary != existing.summary_polyline.as_deref();
    let status = if ignored {
        ActivityStatus::SkippedIgnoredSport
    } else {
        match existing.status {
            ActivityStatus::SkippedIgnoredSport
                if existing
                    .tile_source
                    .as_ref()
                    .map(TileSource::as_str)
                    .is_some_and(|source| !source.is_empty()) =>
            {
                ActivityStatus::Processed
            }
            ActivityStatus::SkippedIgnoredSport => ActivityStatus::Pending,
            ActivityStatus::SkippedNoGps if route_changed => ActivityStatus::Pending,
            ActivityStatus::Pending
            | ActivityStatus::Processed
            | ActivityStatus::Failed
            | ActivityStatus::SkippedNoGps => existing.status,
        }
    };
    let refresh =
        existing.stream_refresh_pending || (status == ActivityStatus::Processed && route_changed);
    conn.execute(
        "UPDATE activities SET sport_type = ?, status = ?, stream_refresh_pending = ?,
         summary_polyline = COALESCE(?4, summary_polyline),
         has_gps = CASE WHEN ?4 IS NOT NULL THEN 1 ELSE has_gps END WHERE id = ?5",
        params![sport, status, refresh, summary, id],
    )?;
    if status != existing.status {
        rebuild_tile_history(conn, Some(&activity_tiles(conn, id)?))?;
    }
    Ok((false, ignored))
}

#[derive(Debug, Default)]
pub struct FetchSummariesResult {
    pub fetched: usize,
    pub stored: usize,
    pub updated: usize,
    pub skipped: usize,
    pub ignored: usize,
}

fn store_summaries(
    settings: &Settings,
    conn: &mut Connection,
    activities: &[Value],
    ignored_sports: &HashSet<String>,
) -> Result<FetchSummariesResult> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut result = FetchSummariesResult {
        fetched: activities.len(),
        ..Default::default()
    };
    for activity in activities {
        let (inserted, ignored) =
            store_summary_in_transaction(settings, &tx, activity, ignored_sports)?;
        result.stored += usize::from(inserted);
        result.updated += usize::from(!inserted);
        result.ignored += usize::from(ignored);
    }
    tx.commit()?;
    Ok(result)
}

pub fn fetch_and_store_summaries(
    settings: &Settings,
    api: &dyn StravaApi,
    page: u32,
    per_page: u32,
    max_items: Option<usize>,
) -> Result<FetchSummariesResult> {
    let activities = fetch_summary_page(api, page, per_page, max_items)?;
    store_summaries(
        settings,
        &mut get_db(settings)?,
        &activities,
        &settings.ignored_sports(),
    )
}

pub(crate) fn fetch_and_store_summaries_with_conn(
    settings: &Settings,
    api: &dyn StravaApi,
    page: u32,
    per_page: u32,
    max_items: Option<usize>,
    conn: &mut Connection,
    ignored_sports: &HashSet<String>,
) -> Result<FetchSummariesResult> {
    let activities = fetch_summary_page(api, page, per_page, max_items)?;
    store_summaries(settings, conn, &activities, ignored_sports)
}

fn fetch_summary_page(
    api: &dyn StravaApi,
    page: u32,
    per_page: u32,
    max_items: Option<usize>,
) -> Result<Vec<Value>> {
    let mut activities = api.get_activities(page, per_page, None, None)?;
    if let Some(max_items) = max_items {
        activities.truncate(max_items);
    }
    Ok(activities)
}

#[derive(Debug)]
pub struct HistoricalNovelty {
    pub processed_before: i64,
    pub total_squadrats_before: i64,
    pub total_squadratinhos_before: i64,
    pub seen_squadrats: usize,
    pub seen_squadratinhos: usize,
    pub new_squadrats: usize,
    pub new_squadratinhos: usize,
    pub total_squadrats_after: i64,
    pub total_squadratinhos_after: i64,
}

pub fn compute_historical_novelty(
    settings: &Settings,
    activity_id: i64,
    start_local: &str,
    squadrats: &HashSet<String>,
    squadratinhos: &HashSet<String>,
    start_utc: Option<&str>,
) -> Result<HistoricalNovelty> {
    compute_historical_novelty_with_conn(
        settings,
        &get_db(settings)?,
        activity_id,
        start_local,
        squadrats,
        squadratinhos,
        start_utc,
    )
}

pub(crate) fn compute_historical_novelty_with_conn(
    settings: &Settings,
    conn: &Connection,
    activity_id: i64,
    start_local: &str,
    squadrats: &HashSet<String>,
    squadratinhos: &HashSet<String>,
    start_utc: Option<&str>,
) -> Result<HistoricalNovelty> {
    let tx = conn.unchecked_transaction()?;
    let stored = get_activity_row(&tx, activity_id)?;
    let start_utc = stored
        .as_ref()
        .map(|row| row.start_utc.as_str())
        .or(start_utc);
    let covered = match (&stored, start_utc) {
        (Some(row), _) => row.baseline_covered,
        (None, Some(utc)) => activity_is_covered(settings, &tx, utc)?,
        _ => false,
    };
    let baseline = match start_utc {
        Some(utc) if activity_uses_baseline(settings, &tx, utc, covered)? => {
            get_baseline(settings, &tx)?
        }
        _ => None,
    };
    let (earlier, boundary) = match start_utc {
        Some(utc) => ("(julianday(a.start_utc), a.id) < (julianday(?1), ?2)", utc),
        None => ("(a.start_local, a.id) < (?1, ?2)", start_local),
    };
    let processed_before = tx.query_row(
        &format!("SELECT COUNT(*) FROM activities a WHERE a.status = 'processed' AND {earlier}"),
        params![boundary, activity_id],
        |row| row.get(0),
    )?;
    let prior = |kind: TileKind, tiles: &HashSet<String>| -> Result<(i64, usize)> {
        let query = format!(
            "FROM activity_tiles at JOIN activities a ON a.id = at.activity_id
             WHERE a.status = 'processed' AND {earlier} AND at.tile_kind = ?3"
        );
        let total = if let Some(utc) = start_utc {
            effective_tile_count_through(
                settings,
                &tx,
                kind.as_str(),
                activity_id,
                utc,
                covered,
                false,
            )?
        } else {
            tx.query_row(
                &format!("SELECT COUNT(DISTINCT at.tile_id) {query}"),
                params![boundary, activity_id, kind],
                |row| row.get(0),
            )?
        };
        let mut seen = if baseline.is_some() {
            baseline_tiles_on_route(&tx, kind.as_str(), tiles)?
        } else {
            HashSet::new()
        };
        let candidate_tiles: Vec<_> = tiles.iter().map(String::as_str).collect();
        let ownership_query = format!(
            "SELECT DISTINCT at.tile_id {query}
             AND (?4 IS NULL OR a.baseline_covered = 1 OR julianday(a.start_utc) > julianday(?4))
             AND at.tile_id"
        );
        let cutoff = baseline
            .as_ref()
            .map(|snapshot| snapshot.as_of_utc.as_str());
        for chunk in candidate_tiles.chunks(crate::db::SQL_CHUNK_SIZE) {
            seen.extend(crate::db::tile_ids_in_chunk(
                &tx,
                &ownership_query,
                &[&boundary, &activity_id, &kind, &cutoff],
                chunk,
            )?);
        }
        Ok((total, seen.len()))
    };
    let (total_squadrats_before, seen_squadrats) = prior(TileKind::Squadrat, squadrats)?;
    let (total_squadratinhos_before, seen_squadratinhos) =
        prior(TileKind::Squadratinho, squadratinhos)?;
    let new_squadrats = squadrats.len() - seen_squadrats;
    let new_squadratinhos = squadratinhos.len() - seen_squadratinhos;
    Ok(HistoricalNovelty {
        processed_before,
        total_squadrats_before,
        total_squadratinhos_before,
        seen_squadrats,
        seen_squadratinhos,
        new_squadrats,
        new_squadratinhos,
        total_squadrats_after: total_squadrats_before + new_squadrats as i64,
        total_squadratinhos_after: total_squadratinhos_before + new_squadratinhos as i64,
    })
}

#[derive(Debug)]
pub struct TileComputationResult {
    pub status: TileComputeStatus,
    pub activity_id: i64,
    pub new_squadrats: i64,
    pub new_squadratinhos: i64,
    pub squadrats: usize,
    pub squadratinhos: usize,
    pub source: Option<TileSource>,
    pub error: Option<String>,
    pub stream_stats: Option<StreamStats>,
}

pub(crate) fn require_activity(conn: &Connection, activity_id: i64) -> Result<ActivityRow> {
    get_activity_row(conn, activity_id)?
        .ok_or_else(|| Error::Validation(format!("Activity {activity_id} not found")))
}

fn unsuccessful_route(
    conn: &mut Connection,
    activity_id: i64,
    source: &TileSource,
    error: Option<String>,
) -> Result<TileComputationResult> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let processed = get_activity_row(&tx, activity_id)?
        .is_some_and(|row| row.status == ActivityStatus::Processed);
    let error = error.or_else(|| {
        processed.then(|| {
            let route = if *source == TileSource::StreamsClean {
                "GPS stream"
            } else {
                "GPS route"
            };
            format!("No usable {route}; preserved previously processed tiles")
        })
    });
    let (status, stored_status) = if error.is_some() {
        (TileComputeStatus::Failed, ActivityStatus::Failed)
    } else {
        (
            TileComputeStatus::SkippedNoGps,
            ActivityStatus::SkippedNoGps,
        )
    };
    tx.execute(
        "UPDATE activities SET status = ?, last_error = ? WHERE id = ?",
        params![
            if processed {
                ActivityStatus::Processed
            } else {
                stored_status
            },
            error,
            activity_id
        ],
    )?;
    tx.commit()?;
    Ok(TileComputationResult {
        status,
        activity_id,
        source: error.is_none().then(|| source.clone()),
        error,
        new_squadrats: 0,
        new_squadratinhos: 0,
        squadrats: 0,
        squadratinhos: 0,
        stream_stats: None,
    })
}

pub fn store_activity_tiles(
    settings: &Settings,
    row: &ActivityRow,
    points: &[Point],
    source: &str,
    segments: Option<&[Vec<Point>]>,
) -> Result<TileComputationResult> {
    store_activity_tiles_with_conn(
        settings,
        &mut get_db(settings)?,
        row,
        points,
        &TileSource::from(source),
        segments,
    )
}

pub(crate) fn store_activity_tiles_with_conn(
    settings: &Settings,
    conn: &mut Connection,
    row: &ActivityRow,
    points: &[Point],
    source: &TileSource,
    segments: Option<&[Vec<Point>]>,
) -> Result<TileComputationResult> {
    if segments.map_or(points.is_empty(), |segments| {
        segments.iter().all(Vec::is_empty)
    }) {
        return unsuccessful_route(conn, row.id, source, None);
    }

    let engine = make_engine(settings);
    let computed = match segments {
        Some(segments) => engine.tiles_for_segments(segments),
        None => engine.tiles_for_points(points),
    };
    let (squadrats, squadratinhos) = match computed {
        Ok(tiles) => tiles,
        Err(error) => {
            return unsuccessful_route(
                conn,
                row.id,
                source,
                Some(format!("Invalid route coordinates: {error}")),
            )
        }
    };
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut affected = activity_tiles(&tx, row.id)?;
    tx.execute("DELETE FROM activity_tiles WHERE activity_id = ?", [row.id])?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO activity_tiles (activity_id, tile_kind, tile_id) VALUES (?, ?, ?)",
        )?;
        for (kind, tiles) in [
            (TileKind::Squadrat, &squadrats),
            (TileKind::Squadratinho, &squadratinhos),
        ] {
            for tile in tiles {
                insert.execute(params![row.id, kind, tile])?;
                affected.add(kind, tile.clone());
            }
        }
    }
    tx.execute(
        "UPDATE activities SET status = 'processed', squadrat_count = ?, squadratinho_count = ?,
         new_squadrat_count = 0, new_squadratinho_count = 0, processed_at = ?, tile_engine = ?,
         tile_source = ?5, has_gps = 1, last_error = NULL,
         stream_refresh_pending = CASE WHEN ?5 = 'streams_clean' THEN 0 ELSE stream_refresh_pending END
         WHERE id = ?6",
        params![squadrats.len(), squadratinhos.len(), Utc::now().to_rfc3339(), engine.id(), source, row.id],
    )?;
    rebuild_tile_history(&tx, Some(&affected))?;
    let (new_squadrats, new_squadratinhos) = tx.query_row(
        "SELECT new_squadrat_count, new_squadratinho_count FROM activities WHERE id = ?",
        [row.id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    tx.commit()?;
    Ok(TileComputationResult {
        status: TileComputeStatus::Processed,
        activity_id: row.id,
        new_squadrats,
        new_squadratinhos,
        squadrats: squadrats.len(),
        squadratinhos: squadratinhos.len(),
        source: Some(source.clone()),
        error: None,
        stream_stats: None,
    })
}

pub fn compute_activity_tiles(
    settings: &Settings,
    api: &dyn StravaApi,
    activity_id: i64,
) -> Result<TileComputationResult> {
    compute_activity_tiles_with_conn(settings, &mut get_db(settings)?, api, activity_id)
}

pub(crate) fn compute_activity_tiles_with_conn(
    settings: &Settings,
    conn: &mut Connection,
    api: &dyn StravaApi,
    activity_id: i64,
) -> Result<TileComputationResult> {
    let row = require_activity(conn, activity_id)?;
    let streams = match api.get_activity_streams(activity_id, STREAM_KEYS) {
        Ok(streams) => streams,
        Err(error) => {
            return unsuccessful_route(
                conn,
                activity_id,
                &TileSource::StreamsClean,
                Some(classify_strava_error(&error).to_string()),
            )
        }
    };
    let (segments, stats) = match clean_stream_segments(settings, &streams) {
        Ok(cleaned) => cleaned,
        Err(error) => {
            return unsuccessful_route(
                conn,
                activity_id,
                &TileSource::StreamsClean,
                Some(format!("Invalid GPS stream: {error}")),
            )
        }
    };
    let mut result = store_activity_tiles_with_conn(
        settings,
        conn,
        &row,
        &[],
        &TileSource::StreamsClean,
        Some(&segments),
    )?;
    result.stream_stats = Some(stats);
    Ok(result)
}

pub fn decode_summary_polyline(
    encoded: &str,
) -> std::result::Result<Vec<Point>, polyline::errors::PolylineError> {
    polyline::decode_polyline(encoded, 5)
        .map(|line| line.0.into_iter().map(|coord| (coord.y, coord.x)).collect())
}

pub(crate) fn process_stored_summary(
    settings: &Settings,
    conn: &mut Connection,
    row: &ActivityRow,
) -> Result<Option<TileComputationResult>> {
    use polyline::errors::PolylineError;

    let Some(summary) = row.summary_polyline.as_deref() else {
        return Ok(None);
    };
    match decode_summary_polyline(summary) {
        Ok(points) if !points.is_empty() => store_activity_tiles_with_conn(
            settings,
            conn,
            row,
            &points,
            &TileSource::SummaryPolyline,
            None,
        )
        .map(Some),
        Err(
            error @ (PolylineError::LatitudeCoordError { .. }
            | PolylineError::LongitudeCoordError { .. }),
        ) => unsuccessful_route(
            conn,
            row.id,
            &TileSource::SummaryPolyline,
            Some(format!("Invalid route coordinates: {error}")),
        )
        .map(Some),
        _ => Ok(None),
    }
}

pub fn compute_activity_tiles_from_summary(
    settings: &Settings,
    api: &dyn StravaApi,
    activity_id: i64,
) -> Result<TileComputationResult> {
    compute_activity_tiles_from_summary_with_conn(
        settings,
        &mut get_db(settings)?,
        api,
        activity_id,
    )
}

pub(crate) fn compute_activity_tiles_from_summary_with_conn(
    settings: &Settings,
    conn: &mut Connection,
    api: &dyn StravaApi,
    activity_id: i64,
) -> Result<TileComputationResult> {
    let row = require_activity(conn, activity_id)?;
    match process_stored_summary(settings, conn, &row)? {
        Some(result) => Ok(result),
        None => compute_activity_tiles_with_conn(settings, conn, api, activity_id),
    }
}

pub fn compute_period_totals(settings: &Settings, start_local: &str) -> Result<(i64, i64)> {
    compute_period_totals_for_kind(settings, start_local, TileKind::Squadrat)
}

pub fn compute_period_totals_for_kind(
    settings: &Settings,
    start_local: &str,
    kind: TileKind,
) -> Result<(i64, i64)> {
    compute_period_totals_for_kind_with_conn(&get_db(settings)?, start_local, kind)
}

pub(crate) fn compute_period_totals_with_conn(
    conn: &Connection,
    start_local: &str,
) -> Result<(i64, i64)> {
    compute_period_totals_for_kind_with_conn(conn, start_local, TileKind::Squadrat)
}

pub(crate) fn compute_period_totals_for_kind_with_conn(
    conn: &Connection,
    start_local: &str,
    kind: TileKind,
) -> Result<(i64, i64)> {
    let column = match kind {
        TileKind::Squadrat => "new_squadrat_count",
        TileKind::Squadratinho => "new_squadratinho_count",
    };
    let date = parse_local_datetime(start_local)?.date();
    let month = date.with_day(1).expect("every month has a first day");
    let week = date - Duration::days(date.weekday().num_days_from_monday().into());
    let total = |start: NaiveDate| -> Result<i64> {
        Ok(conn.query_row(
            &format!(
                "SELECT COALESCE(SUM({column}), 0) FROM activities
                 WHERE status = 'processed' AND start_local >= ? AND start_local <= ?"
            ),
            params![format!("{start}T00:00:00"), start_local],
            |row| row.get(0),
        )?)
    };
    Ok((total(month)?, total(week)?))
}

pub fn compute_total_unique_squadrats_through(
    settings: &Settings,
    activity_id: i64,
    _start_local: &str,
) -> Result<i64> {
    compute_total_unique_squadrats_through_with_conn(settings, &get_db(settings)?, activity_id)
}

pub(crate) fn compute_total_unique_squadrats_through_with_conn(
    settings: &Settings,
    conn: &Connection,
    activity_id: i64,
) -> Result<i64> {
    let Some(row) = get_activity_row(conn, activity_id)? else {
        return Ok(0);
    };
    effective_tile_count_through(
        settings,
        conn,
        TileKind::Squadrat.as_str(),
        activity_id,
        &row.start_utc,
        row.baseline_covered,
        true,
    )
}

pub(crate) fn activity_ids(conn: &Connection, sql: &str, params: impl Params) -> Result<Vec<i64>> {
    let mut statement = conn.prepare(sql)?;
    let ids = statement
        .query_map(params, |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

#[derive(Debug, Default)]
pub struct SyncOnceResult {
    pub new_activities: usize,
    pub failed: usize,
    pub processed: usize,
    pub annotated: usize,
    pub skipped: usize,
}

pub fn sync_once(settings: &Settings, api: &dyn StravaApi) -> Result<SyncOnceResult> {
    sync_once_with_conn(
        settings,
        &mut get_db(settings)?,
        api,
        &mut std::io::stdout(),
    )
}

pub(crate) fn sync_once_with_conn(
    settings: &Settings,
    conn: &mut Connection,
    api: &dyn StravaApi,
    output: &mut impl std::io::Write,
) -> Result<SyncOnceResult> {
    let matcher = crate::descriptions::DescriptionMatcher::new(settings);
    let sync_started_at = Utc::now();
    let after = sync_after_timestamp_with_conn(settings, conn, sync_started_at)?;
    let mut activities = Vec::new();
    for page in 1.. {
        let batch = api.get_activities(page, PAGE_SIZE, Some(after), None)?;
        if batch.is_empty() {
            break;
        }
        activities.extend(batch);
    }
    let mut result = SyncOnceResult {
        new_activities: store_summaries(settings, conn, &activities, &settings.ignored_sports())?
            .stored,
        ..Default::default()
    };
    let ids = activity_ids(
        conn,
        "SELECT id FROM activities WHERE status IN ('pending', 'failed')
         OR (status = 'processed' AND stream_refresh_pending = 1)
         ORDER BY julianday(start_utc), id",
        [],
    )?;
    for id in ids {
        match compute_activity_tiles_with_conn(settings, conn, api, id)?.status {
            TileComputeStatus::Processed => result.processed += 1,
            TileComputeStatus::SkippedNoGps => result.skipped += 1,
            TileComputeStatus::Failed => result.failed += 1,
        }
    }
    let cutoff = days_before(Utc::now(), settings.sync_annotation_window_days)?;
    let ids = activity_ids(
        conn,
        "SELECT id FROM activities WHERE status = 'processed' AND stream_refresh_pending = 0
         AND (annotation_status IS NULL OR annotation_status IN ('none', 'failed'))
         AND (julianday(start_utc) >= julianday(?) OR ?) ORDER BY start_utc, id",
        params![
            cutoff.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            settings.rewrite_existing_annotations
        ],
    )?;
    for id in ids {
        let annotated = annotate_activity_with_conn(settings, conn, &matcher, api, id)?;
        match annotated.status {
            AnnotateStatus::Annotated => {
                result.annotated += 1;
                if let Some(line) = annotated.line {
                    writeln!(output, "Annotated {id}: {line}")?;
                }
            }
            AnnotateStatus::AnnotationFailed => result.failed += 1,
            AnnotateStatus::Skipped | AnnotateStatus::NotProcessed => {}
        }
    }
    set_setting_with_conn(
        conn,
        LAST_SUCCESSFUL_SYNC_KEY,
        &Value::String(sync_started_at.to_rfc3339()),
    )?;
    Ok(result)
}

#[derive(Debug, Default)]
pub struct RetryResult {
    pub retried: usize,
    pub success: usize,
}

pub fn retry_failed(settings: &Settings, api: &dyn StravaApi) -> Result<RetryResult> {
    retry_failed_with_conn(settings, &mut get_db(settings)?, api)
}

pub(crate) fn retry_failed_with_conn(
    settings: &Settings,
    conn: &mut rusqlite::Connection,
    api: &dyn StravaApi,
) -> Result<RetryResult> {
    let matcher = crate::descriptions::DescriptionMatcher::new(settings);
    let mut result = RetryResult::default();
    let ids = activity_ids(conn,
        "SELECT id FROM activities WHERE status = 'failed' OR (status = 'processed' AND stream_refresh_pending = 1)
         ORDER BY julianday(start_utc), id", [],
    )?;
    for id in ids {
        result.retried += 1;
        result.success += usize::from(
            compute_activity_tiles_with_conn(settings, conn, api, id)?.status
                == TileComputeStatus::Processed,
        );
    }
    let ids = activity_ids(conn,
        "SELECT id FROM activities WHERE annotation_status = 'failed' ORDER BY julianday(start_utc), id", [],
    )?;
    for id in ids {
        result.retried += 1;
        result.success += usize::from(matches!(
            annotate_activity_with_conn(settings, conn, &matcher, api, id)?.status,
            AnnotateStatus::Annotated | AnnotateStatus::Skipped
        ));
    }
    Ok(result)
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::db::{get_setting, set_setting};
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;
    use crate::db::migrate;
    use crate::strava_client::RateLimitStatus;

    pub(crate) const ROUTE: &str = "_gk|H_qo]";

    pub(crate) fn database() -> (tempfile::TempDir, Settings) {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::with_data_dir(dir.path());
        migrate(&settings).unwrap();
        (dir, settings)
    }

    pub(crate) fn summary(id: i64, day: i64) -> Value {
        json!({
            "id": id, "sport_type": "Ride",
            "start_date": format!("2026-01-{day:02}T10:00:00Z"),
            "start_date_local": format!("2026-01-{day:02}T11:00:00"),
            "map": {"summary_polyline": ROUTE}
        })
    }

    #[derive(Default)]
    pub(crate) struct MockApi {
        pub pages: Vec<Vec<Value>>,
        pub fail_page: Option<u32>,
        pub activity_requests: RefCell<Vec<(u32, u32, Option<i64>)>>,
        pub streams: Value,
        pub stream_requests: RefCell<Vec<i64>>,
        pub description: RefCell<String>,
        pub updates: RefCell<usize>,
    }

    impl StravaApi for MockApi {
        fn is_authenticated(&self) -> bool {
            true
        }
        fn build_auth_url(&self) -> Result<String> {
            unreachable!()
        }
        fn exchange_code(&self, _: &str) -> Result<Value> {
            unreachable!()
        }
        fn get_athlete(&self) -> Result<Value> {
            unreachable!()
        }
        fn get_activities(
            &self,
            page: u32,
            per_page: u32,
            after: Option<i64>,
            before: Option<i64>,
        ) -> Result<Vec<Value>> {
            assert_eq!(before, None);
            self.activity_requests
                .borrow_mut()
                .push((page, per_page, after));
            if self.fail_page == Some(page) {
                return Err(Error::Other("fetch failed".into()));
            }
            Ok(self
                .pages
                .get(page as usize - 1)
                .cloned()
                .unwrap_or_default())
        }
        fn get_activity(&self, _: i64) -> Result<Value> {
            Ok(json!({"description": *self.description.borrow()}))
        }
        fn get_activity_streams(&self, id: i64, keys: &str) -> Result<Value> {
            assert_eq!(keys, STREAM_KEYS);
            self.stream_requests.borrow_mut().push(id);
            Ok(self.streams.clone())
        }
        fn update_activity_description(&self, _: i64, description: &str) -> Result<Value> {
            *self.updates.borrow_mut() += 1;
            *self.description.borrow_mut() = description.into();
            Ok(json!({}))
        }
        fn get_rate_limit_status(&self) -> RateLimitStatus {
            unreachable!()
        }
    }

    #[test]
    fn sync_reads_short_pages_until_an_empty_page() {
        let (_dir, settings) = database();
        let api = MockApi {
            pages: vec![vec![summary(1, 1)], vec![summary(2, 2)]],
            ..Default::default()
        };
        let result = sync_once(&settings, &api).unwrap();
        assert_eq!(result.new_activities, 2);
        let pages: Vec<_> = api
            .activity_requests
            .borrow()
            .iter()
            .map(|request| request.0)
            .collect();
        assert_eq!(pages, [1, 2, 3]);
    }

    #[test]
    fn drops_sub_gap_lateral_bounces_without_awarding_neighboring_squadratinhos() {
        let settings = Settings::default();
        // Longitude zero is a z17 tile boundary. At the equator 0.00036° is 40 m.
        let start = (1.0, -0.00018);
        let spike = (1.000072, 0.00018);
        let end = (1.000144, -0.00018);
        let engine = make_engine(&settings);
        let (_, spike_tiles) = engine.tiles_for_points(&[spike]).unwrap();
        for points in [vec![start, spike, end], vec![start, spike, spike, end]] {
            let times: Vec<_> = (0..points.len()).collect();
            let mut streams = json!({"latlng": {"data": points}});
            for time_data in [json!([]), json!([0])] {
                streams["time"] = json!({"data": time_data});
                let (segments, stats) = clean_stream_segments(&settings, &streams).unwrap();
                assert_eq!(segments, std::slice::from_ref(&points));
                assert_eq!(stats.spikes_dropped, 0);
            }
            streams["time"] = json!({"data": times});
            let (segments, stats) = clean_stream_segments(&settings, &streams).unwrap();
            assert_eq!(segments, [vec![start, end]]);
            assert_eq!(stats.spikes_dropped, points.len() - 2);
            assert_eq!(stats.splits, 0);
            let (_, squadratinhos) = engine.tiles_for_segments(&segments).unwrap();
            assert!(!spike_tiles.is_empty());
            assert!(squadratinhos.is_disjoint(&spike_tiles));
        }
    }

    #[test]
    fn preserves_cycling_hairpin() {
        let start = (1.0, 0.0);
        let turn = (1.000072, 0.0);
        let points = [start, turn, start];
        let streams = json!({"latlng": {"data": points}, "time": {"data": [0, 1, 2]}});
        let (segments, stats) = clean_stream_segments(&Settings::default(), &streams).unwrap();
        assert_eq!(segments, [points.to_vec()]);
        assert_eq!(stats.spikes_dropped, 0);
        assert_eq!(stats.splits, 0);
    }

    #[test]
    fn computes_period_totals_for_both_tile_kinds() {
        let (_dir, settings) = database();
        let conn = get_db(&settings).unwrap();
        for (id, date, status, squadrats, squadratinhos) in [
            (1, "2025-12-31T10:00:00", "processed", 50, 60),
            (2, "2026-01-01T00:00:00", "processed", 2, 7),
            (3, "2026-01-05T00:00:00", "processed", 3, 11),
            (4, "2026-01-07T10:00:00", "processed", 5, 13),
            (5, "2026-01-07T10:00:00", "pending", 50, 60),
            (6, "2026-01-07T10:00:01", "processed", 50, 60),
        ] {
            conn.execute(
                "INSERT INTO activities
                 (id, start_utc, start_local, status, new_squadrat_count, new_squadratinho_count)
                 VALUES (?1, ?2, ?2, ?3, ?4, ?5)",
                params![id, date, status, squadrats, squadratinhos],
            )
            .unwrap();
        }
        let end = "2026-01-07T10:00:00";
        assert_eq!(
            compute_period_totals_for_kind(&settings, end, TileKind::Squadrat).unwrap(),
            (10, 8)
        );
        assert_eq!(
            compute_period_totals_for_kind(&settings, end, TileKind::Squadratinho).unwrap(),
            (31, 24)
        );
        assert_eq!(compute_period_totals(&settings, end).unwrap(), (10, 8));
    }

    #[test]
    fn drops_isolated_spike_without_awarding_its_tiles() {
        let settings = Settings::default();
        let start = (52.0, 5.0);
        let spike = (53.0, 5.0);
        let end = (52.001, 5.0);
        let engine = make_engine(&settings);
        let (spike_squadrats, spike_squadratinhos) = engine.tiles_for_points(&[spike]).unwrap();
        for times in [None, Some(vec![0]), Some(vec![0, 3, 4])] {
            let mut streams = json!({"latlng": {"data": [start, spike, end]}});
            if let Some(times) = times {
                streams["time"] = json!({"data": times});
            }
            let (segments, stats) = clean_stream_segments(&settings, &streams).unwrap();
            assert_eq!(segments, [vec![start, end]]);
            assert_eq!(stats.spikes_dropped, 1);
            assert_eq!(stats.splits, 0);
            let (squadrats, squadratinhos) = engine.tiles_for_segments(&segments).unwrap();
            assert!(squadrats.is_disjoint(&spike_squadrats));
            assert!(squadratinhos.is_disjoint(&spike_squadratinhos));
        }
    }

    #[test]
    fn drops_repeated_isolated_spikes_and_preserves_excursions_and_gaps() {
        let settings = Settings::default();
        let a = (52.0, 5.0);
        let b = (53.0, 5.0);
        let c = (52.0001, 5.0);
        let d = (53.0001, 5.0);
        let e = (52.0002, 5.0);
        let far = (54.0, 5.0);
        let remote_route = vec![b, d, (53.0002, 5.0), (53.0003, 5.0)];
        let cases = [
            (vec![a, b, c, d, e], vec![vec![a, c, e]], 2),
            (
                [vec![a, c], remote_route.clone(), vec![c, e]].concat(),
                vec![vec![a, c], remote_route, vec![c, e]],
                0,
            ),
            (vec![a, c, b, far], vec![vec![a, c]], 2),
        ];
        for (points, expected, dropped) in cases {
            let (segments, stats) =
                clean_stream_segments(&settings, &json!({"latlng": {"data": points}})).unwrap();
            assert_eq!(segments, expected);
            assert_eq!(stats.spikes_dropped, dropped);
            assert_eq!(stats.splits, expected.len() - 1);
        }
    }

    #[test]
    fn removes_spike_bursts_and_endpoint_glitches() {
        let route = [(52.0, 5.0), (52.0001, 5.0), (52.0002, 5.0)];
        let spike = (53.0, 5.0);
        for (index, count) in [(1, 2), (1, 3), (0, 1), (route.len(), 1)] {
            let mut points = route.to_vec();
            points.splice(index..index, std::iter::repeat_n(spike, count));
            let streams = json!({"latlng": {"data": points}});
            let (segments, stats) = clean_stream_segments(&Settings::default(), &streams).unwrap();
            assert_eq!(segments, [route.to_vec()]);
            assert_eq!(stats.spikes_dropped, count);
            assert_eq!(stats.splits, 0);
        }
    }

    #[test]
    fn filters_stationary_drift_without_awarding_tiles_or_splitting() {
        let settings = Settings::default();
        let start = (52.0, 5.0);
        let end = (52.00007, 5.0);
        let drift = (52.001, 5.0);
        let streams = json!({
            "latlng": {"data": [drift, start, drift, drift, end]},
            "time": {"data": [0, 1, 100, 600, 601]},
            "moving": {"data": [false, true, false, false, true]},
            "distance": {"data": [0, 0, 0, 0, 8]}
        });
        let (segments, stats) = clean_stream_segments(&settings, &streams).unwrap();
        assert_eq!(segments, [vec![start, end]]);
        assert_eq!(stats.stationary_dropped, 3);
        assert_eq!(stats.splits, 0);
        let engine = make_engine(&settings);
        assert_eq!(
            engine.tiles_for_segments(&segments).unwrap(),
            engine.tiles_for_points(&[start, end]).unwrap()
        );
    }

    #[test]
    fn preserves_motion_when_moving_flags_are_false_or_misaligned() {
        let route = [(52.0, 5.0), (52.0002, 5.0), (52.0004, 5.0)];
        for moving in [vec![false; 3], vec![true, false, true], vec![false]] {
            let streams = json!({
                "latlng": {"data": route},
                "moving": {"data": moving},
                "distance": {"data": [0, 22, 44]}
            });
            let (segments, stats) = clean_stream_segments(&Settings::default(), &streams).unwrap();
            assert_eq!(segments, [route.to_vec()]);
            assert_eq!(stats.stationary_dropped, 0);
        }
    }

    #[test]
    fn bridges_corroborated_tunnel_but_splits_transfers() {
        let start = (52.0, 5.0);
        let before = (52.00007, 5.0);
        for (meters, seconds, traveled, splits) in [
            (400.0_f64, 50, 400, 0),
            (400.0, 301, 400, 1),
            (400.0, 50, 0, 1),
            (400.0, 5, 400, 1),
            (15_000.0, 1200, 0, 1),
        ] {
            let after = (before.0 + (meters / 6_371_000.0).to_degrees(), 5.0);
            let end = (after.0 + 0.00007, 5.0);
            let streams = json!({
                "latlng": {"data": [start, before, after, end]},
                "time": {"data": [0, 1, 1 + seconds, 2 + seconds]},
                "distance": {"data": [0, 8, 8 + traveled, 16 + traveled]}
            });
            let (segments, stats) = clean_stream_segments(&Settings::default(), &streams).unwrap();
            let expected = if splits == 0 {
                vec![vec![start, before, after, end]]
            } else {
                vec![vec![start, before], vec![after, end]]
            };
            assert_eq!(segments, expected);
            assert_eq!(stats.splits, splits);
        }
    }

    #[test]
    fn rejects_invalid_optional_telemetry_even_when_misaligned() {
        for (key, kind, invalid_sample) in [
            ("moving", "booleans", json!(1)),
            ("distance", "finite numbers", json!(true)),
        ] {
            let expected = format!("GPS {key} stream must contain {kind}");
            for invalid in [Value::Null, json!({}), json!({"data": [invalid_sample]})] {
                let mut streams = json!({"latlng": {"data": [[52, 5], [52, 5]]}});
                streams[key] = invalid;
                assert!(matches!(
                    clean_stream_segments(&Settings::default(), &streams),
                    Err(Error::Validation(message)) if message == expected
                ));
            }
        }
    }

    #[test]
    fn sync_writes_successful_annotations_to_the_supplied_writer() {
        let (_dir, settings) = database();
        let mut activity = summary(1, 1);
        activity["start_date"] = json!(Utc::now().to_rfc3339());
        let api = MockApi {
            pages: vec![vec![activity]],
            streams: json!({"latlng": {"data": [[52.0, 5.0], [52.0001, 5.0001]]}}),
            ..Default::default()
        };
        let mut output = Vec::new();
        let result = sync_once_with_conn(
            &settings,
            &mut get_db(&settings).unwrap(),
            &api,
            &mut output,
        )
        .unwrap();
        assert_eq!(result.annotated, 1);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("Annotated 1: {}\n", api.description.borrow())
        );
    }

    #[test]
    fn decodes_latitude_before_longitude() {
        assert_eq!(decode_summary_polyline(ROUTE).unwrap(), vec![(52.0, 5.0)]);
    }

    #[test]
    fn parses_wall_time_and_normalizes_utc() {
        let naive = parse_local_datetime("2026-01-02T03:04:05").unwrap();
        for suffix in ["Z", "+01:00", "-04:30"] {
            assert_eq!(
                parse_local_datetime(&format!("2026-01-02T03:04:05{suffix}")).unwrap(),
                naive
            );
        }
        assert_eq!(
            parse_utc_datetime("2026-01-02T05:04:05+02:00").unwrap(),
            naive.and_utc()
        );
    }

    #[test]
    fn rejects_malformed_stream_payloads() {
        for payload in [
            Value::Null,
            json!([]),
            json!({"latlng": null}),
            json!({"latlng": {"data": null}}),
            json!({"latlng": {"data": [{"lat": 52, "lon": 5}]}}),
            json!({"latlng": {"data": [[true, 5]]}}),
            json!({"latlng": {"data": [[52, "5"]]}}),
            json!({"latlng": {"data": [[91, 5]]}}),
            json!({"time": null}),
            json!({"time": {"data": [null, {}]}}),
            json!({"time": {"data": [true]}}),
            json!({"time": {"data": ["1"]}}),
        ] {
            assert!(
                matches!(
                    clean_stream_segments(&Settings::default(), &payload),
                    Err(Error::Validation(_))
                ),
                "{payload}"
            );
        }
    }

    #[test]
    fn rejects_oversized_stream_instead_of_truncating() {
        let settings = Settings {
            stream_max_points: 1,
            ..Settings::default()
        };
        let streams = json!({"latlng": {"data": [[52, 5], [52, 5]]}});
        assert!(matches!(
            clean_stream_segments(&settings, &streams),
            Err(Error::Validation(error)) if error.contains("TH_STREAM_MAX_POINTS=1")
        ));
    }

    #[test]
    fn reports_unusable_single_point_segments() {
        let streams = json!({"latlng": {"data": [[52, 5], [51, 6]]}});
        let (segments, stats) = clean_stream_segments(&Settings::default(), &streams).unwrap();
        assert!(segments.is_empty());
        assert_eq!(stats.points, 2);
        assert_eq!(stats.splits, 1);
        assert_eq!(
            stats.warning.as_deref(),
            Some("all points filtered — every segment is a single point")
        );
    }

    #[test]
    fn late_activity_repairs_novelty_and_uses_utc_for_stored_candidates() {
        let (_dir, settings) = database();
        let conn = get_db(&settings).unwrap();
        let api = MockApi::default();
        store_summary(&settings, &conn, &summary(2, 2)).unwrap();
        assert_eq!(
            compute_activity_tiles_from_summary(&settings, &api, 2)
                .unwrap()
                .new_squadrats,
            1
        );
        store_summary(&settings, &conn, &summary(1, 1)).unwrap();
        assert_eq!(
            compute_activity_tiles_from_summary(&settings, &api, 1)
                .unwrap()
                .new_squadrats,
            1
        );
        assert_eq!(require_activity(&conn, 2).unwrap().new_squadrat_count, 0);

        let (squadrats, squadratinhos) = make_engine(&settings)
            .tiles_for_points(&[(52.0, 5.0)])
            .unwrap();
        let novelty = compute_historical_novelty(
            &settings,
            2,
            "1990-01-01",
            &squadrats,
            &squadratinhos,
            None,
        )
        .unwrap();
        assert_eq!(novelty.processed_before, 1);
        assert_eq!(novelty.seen_squadrats, 1);
        assert_eq!(novelty.new_squadratinhos, 0);
        let novelty = compute_historical_novelty(
            &settings,
            3,
            "1990-01-01",
            &squadrats,
            &squadratinhos,
            None,
        )
        .unwrap();
        assert_eq!(novelty.processed_before, 0);
        assert_eq!(novelty.new_squadrats, 1);
    }

    #[test]
    fn out_of_bounds_summary_fails_without_fetching_streams() {
        let (_dir, settings) = database();
        let api = MockApi::default();
        let mut activity = summary(1, 1);
        // Encoded (91.0, 5.0).
        activity["map"]["summary_polyline"] = json!("_mljP_qo]");
        store_summary(&settings, &get_db(&settings).unwrap(), &activity).unwrap();
        let result = compute_activity_tiles_from_summary(&settings, &api, 1).unwrap();
        assert_eq!(result.status.as_str(), "failed");
        assert!(result.error.unwrap().contains("Invalid route coordinates"));
        assert!(api.stream_requests.borrow().is_empty());
    }

    #[test]
    fn unstored_candidate_uses_baseline_when_utc_is_available() {
        let (_dir, settings) = database();
        let conn = get_db(&settings).unwrap();
        conn.execute(
            "INSERT INTO baseline_imports VALUES (1, 'test', 'hash', '2026-01-01',
             '2026-01-01T00:00:00Z', 1, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO baseline_tiles VALUES (1, 'squadrat', '14:1:1')",
            [],
        )
        .unwrap();
        let tiles = HashSet::from(["14:1:1".to_owned()]);
        let local = "2026-01-02T10:00:00";
        let with_utc = compute_historical_novelty(
            &settings,
            1,
            local,
            &tiles,
            &HashSet::new(),
            Some("2026-01-02T09:00:00Z"),
        )
        .unwrap();
        assert_eq!(with_utc.total_squadrats_before, 1);
        assert_eq!(with_utc.new_squadrats, 0);
        let without_utc =
            compute_historical_novelty(&settings, 1, local, &tiles, &HashSet::new(), None).unwrap();
        assert_eq!(without_utc.total_squadrats_before, 0);
        assert_eq!(without_utc.new_squadrats, 1);
    }

    #[test]
    fn changed_route_keeps_tiles_until_stream_refresh_succeeds() {
        let (_dir, settings) = database();
        let conn = get_db(&settings).unwrap();
        let mut api = MockApi::default();
        let mut activity = summary(1, 1);
        store_summary(&settings, &conn, &activity).unwrap();
        compute_activity_tiles_from_summary(&settings, &api, 1).unwrap();
        activity["map"]["summary_polyline"] = json!("new route");
        store_summary(&settings, &conn, &activity).unwrap();
        assert!(require_activity(&conn, 1).unwrap().stream_refresh_pending);

        for streams in [Value::Null, json!({})] {
            api.streams = streams;
            assert_eq!(
                compute_activity_tiles(&settings, &api, 1)
                    .unwrap()
                    .status
                    .as_str(),
                "failed"
            );
            let row = require_activity(&conn, 1).unwrap();
            assert_eq!(row.status.as_str(), "processed");
            assert_eq!(row.new_squadrat_count, 1);
            assert!(row.stream_refresh_pending);
        }

        api.streams = json!({"latlng": {"data": [[51.0, 6.0], [51.0, 6.0]]}});
        assert_eq!(retry_failed(&settings, &api).unwrap().success, 1);
        let row = require_activity(&conn, 1).unwrap();
        assert!(!row.stream_refresh_pending);
        assert_eq!(
            row.tile_source.as_ref().map(|source| source.as_str()),
            Some("streams_clean")
        );
        assert!(row.last_error.is_none());
    }

    #[test]
    fn failed_second_page_does_not_advance_cursor_or_store_partial_fetch() {
        let (_dir, settings) = database();
        let cursor = json!("2026-01-01T00:00:00Z");
        set_setting(&settings, LAST_SUCCESSFUL_SYNC_KEY, &cursor).unwrap();
        let api = MockApi {
            pages: vec![vec![summary(1, 1); 200]],
            fail_page: Some(2),
            ..Default::default()
        };
        assert!(sync_once(&settings, &api).is_err());
        let after = sync_after_timestamp(&settings, Utc::now()).unwrap();
        assert_eq!(
            *api.activity_requests.borrow(),
            vec![(1, 200, Some(after)), (2, 200, Some(after))]
        );
        assert_eq!(
            get_setting(&settings, LAST_SUCCESSFUL_SYNC_KEY).unwrap(),
            Some(cursor)
        );
        assert!(latest_stored_activity_utc(&settings).unwrap().is_none());
    }
}
