//! Replace summary routes with full Strava GPS streams.

use rusqlite::{params_from_iter, types::Value, Connection};

use crate::config::Settings;
use crate::db::get_db;
use crate::error::{Result, StravaError};
use crate::kml_baseline::get_baseline;
use crate::models::TileComputeStatus;
use crate::recompute::recompute_novelty_from_stored_tiles_with_conn;
use crate::strava_client::StravaApi;
use crate::sync::{activity_ids, compute_activity_tiles_with_conn};

#[derive(Debug, Default)]
pub struct RefineResult {
    pub refined: usize,
    pub failed: usize,
    pub selected: usize,
    pub rebuilt: i64,
    pub splits: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct AutoRefineResult {
    pub selected: usize,
    pub refined: usize,
    pub failed: usize,
    pub splits: usize,
    pub remaining: i64,
    pub stopped_for_rate_limit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefineOrder {
    OldestFirst,
    NewestFirst,
}

fn select_refine_ids(
    settings: &Settings,
    conn: &Connection,
    limit: usize,
    force: bool,
    exclude_refresh_pending: bool,
    order: RefineOrder,
) -> Result<Vec<i64>> {
    let ignored = settings.ignored_sports();
    let baseline = if force {
        None
    } else {
        get_baseline(settings, conn)?
    };
    let mut sql =
        String::from("SELECT id FROM activities WHERE status = 'processed' AND has_gps = 1");
    let mut params: Vec<Value> = Vec::new();
    if !ignored.is_empty() {
        let placeholders = crate::db::in_placeholders(ignored.len());
        sql.push_str(&format!(
            " AND COALESCE(sport_type, '') NOT IN ({placeholders})"
        ));
        params.extend(ignored.into_iter().map(Value::Text));
    }
    if !force {
        sql.push_str(" AND COALESCE(tile_source, '') != 'streams_clean'");
    }
    if exclude_refresh_pending {
        sql.push_str(" AND stream_refresh_pending = 0");
    }
    if let Some(baseline) = baseline {
        sql.push_str(" AND baseline_covered = 0 AND julianday(start_utc) > julianday(?)");
        params.push(Value::Text(baseline.as_of_utc));
    }
    match order {
        RefineOrder::OldestFirst => sql.push_str(" ORDER BY start_local"),
        RefineOrder::NewestFirst => sql.push_str(" ORDER BY julianday(start_utc) DESC, id DESC"),
    }
    if limit > 0 {
        sql.push_str(" LIMIT ?");
        params.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
    }
    activity_ids(conn, &sql, params_from_iter(params))
}

/// Count processed GPS activities that still benefit from full-stream refinement
/// (excluding ignored sports and activities already covered by an installed KML baseline).
pub(crate) fn unrefined_count_with_conn(settings: &Settings, conn: &Connection) -> Result<i64> {
    let ignored = settings.ignored_sports();
    let baseline = get_baseline(settings, conn)?;
    let mut sql = String::from(
        "SELECT COUNT(*) FROM activities
         WHERE status = 'processed' AND has_gps = 1
           AND COALESCE(tile_source, '') != 'streams_clean'",
    );
    let mut params: Vec<Value> = Vec::new();
    if !ignored.is_empty() {
        let placeholders = crate::db::in_placeholders(ignored.len());
        sql.push_str(&format!(
            " AND COALESCE(sport_type, '') NOT IN ({placeholders})"
        ));
        params.extend(ignored.into_iter().map(Value::Text));
    }
    if let Some(baseline) = baseline {
        sql.push_str(" AND baseline_covered = 0 AND julianday(start_utc) > julianday(?)");
        params.push(Value::Text(baseline.as_of_utc));
    }
    Ok(conn.query_row(&sql, params_from_iter(params), |row| row.get(0))?)
}

pub fn refine_streams(
    settings: &Settings,
    api: &dyn StravaApi,
    limit: Option<usize>,
    force: bool,
) -> Result<RefineResult> {
    refine_streams_with_conn(settings, &mut get_db(settings)?, api, limit, force)
}

pub(crate) fn refine_streams_with_conn(
    settings: &Settings,
    conn: &mut Connection,
    api: &dyn StravaApi,
    limit: Option<usize>,
    force: bool,
) -> Result<RefineResult> {
    let limit = limit.unwrap_or(settings.refine_default_limit);
    let ids = select_refine_ids(
        settings,
        conn,
        limit,
        force,
        false,
        RefineOrder::OldestFirst,
    )?;
    let mut result = RefineResult {
        selected: ids.len(),
        ..Default::default()
    };
    for id in ids {
        let tiles = compute_activity_tiles_with_conn(settings, conn, api, id)?;
        if tiles.status == TileComputeStatus::Processed {
            result.refined += 1;
            result.splits += tiles.stream_stats.map_or(0, |stats| stats.splits);
        } else {
            result.failed += 1;
            if matches!(
                tiles.strava_error,
                Some(StravaError::RateLimit(_) | StravaError::Auth(_))
            ) {
                break;
            }
        }
    }
    if result.refined > 0 {
        result.rebuilt = recompute_novelty_from_stored_tiles_with_conn(settings, conn)?.rebuilt;
    }
    Ok(result)
}

/// Rate-limit-aware background refinement intended to run automatically during `sync`.
/// Refines newest unrefined activities first while keeping Strava quota headroom reserved
/// for live activity syncing and annotations.
pub(crate) fn auto_refine_with_conn(
    settings: &Settings,
    conn: &mut Connection,
    api: &dyn StravaApi,
) -> Result<AutoRefineResult> {
    if !settings.auto_refine || settings.auto_refine_per_cycle == 0 {
        return Ok(AutoRefineResult {
            remaining: unrefined_count_with_conn(settings, conn)?,
            ..Default::default()
        });
    }

    let mut ids = select_refine_ids(
        settings,
        conn,
        settings.auto_refine_per_cycle,
        false,
        true,
        RefineOrder::NewestFirst,
    )?;
    // Prioritize the newest unrefined activities, but process the selected batch
    // chronologically so earlier rides claim newly discovered tiles first.
    ids.reverse();
    let mut result = AutoRefineResult {
        selected: ids.len(),
        ..Default::default()
    };
    for id in ids {
        if let Some(snapshot) = api.last_rate_limit() {
            if !snapshot.has_budget(
                settings.auto_refine_rate_limit_reserve_pct,
                settings.rate_limit_buffer,
            ) {
                result.stopped_for_rate_limit = true;
                break;
            }
        }
        let tiles = compute_activity_tiles_with_conn(settings, conn, api, id)?;
        if tiles.status == TileComputeStatus::Processed {
            result.refined += 1;
            result.splits += tiles.stream_stats.map_or(0, |stats| stats.splits);
        } else {
            result.failed += 1;
            if matches!(
                tiles.strava_error,
                Some(
                    StravaError::RateLimit(_)
                        | StravaError::Auth(_)
                        | StravaError::Network(_)
                        | StravaError::Server(_)
                )
            ) {
                if matches!(tiles.strava_error, Some(StravaError::RateLimit(_))) {
                    result.stopped_for_rate_limit = true;
                }
                break;
            }
        }
    }
    result.remaining = unrefined_count_with_conn(settings, conn)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::tests::{database, summary, MockApi};
    use crate::sync::{compute_activity_tiles_from_summary, require_activity, store_summary};

    #[test]
    fn zero_limit_refines_all_and_force_includes_refined_routes() {
        let (_dir, mut settings) = database();
        settings.refine_default_limit = 1;
        let api = MockApi {
            streams: serde_json::json!({"latlng": {"data": [[52, 5], [52, 5]]}}),
            ..Default::default()
        };
        let conn = get_db(&settings).unwrap();
        for id in 1..=2 {
            store_summary(&settings, &conn, &summary(id, id)).unwrap();
            compute_activity_tiles_from_summary(&settings, &api, id).unwrap();
        }
        assert_eq!(
            refine_streams(&settings, &api, None, false)
                .unwrap()
                .selected,
            1
        );
        assert_eq!(
            refine_streams(&settings, &api, Some(0), false)
                .unwrap()
                .refined,
            1
        );
        let result = refine_streams(&settings, &api, Some(0), true).unwrap();
        assert_eq!(result.refined, 2);
        assert_eq!(result.rebuilt, 2);
        assert_eq!(require_activity(&conn, 2).unwrap().new_squadrat_count, 0);
    }
}
