//! Replace summary routes with full Strava GPS streams.

use rusqlite::{params_from_iter, types::Value};

use crate::config::Settings;
use crate::db::get_db;
use crate::error::Result;
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
    conn: &mut rusqlite::Connection,
    api: &dyn StravaApi,
    limit: Option<usize>,
    force: bool,
) -> Result<RefineResult> {
    let limit = limit.unwrap_or(settings.refine_default_limit);
    let ignored = settings.ignored_sports();
    let mut sql =
        String::from("SELECT id FROM activities WHERE status = 'processed' AND has_gps = 1");
    if !ignored.is_empty() {
        let placeholders = crate::db::in_placeholders(ignored.len());
        sql.push_str(&format!(
            " AND COALESCE(sport_type, '') NOT IN ({placeholders})"
        ));
    }
    let mut params: Vec<Value> = ignored.into_iter().map(Value::Text).collect();
    if !force {
        sql.push_str(" AND COALESCE(tile_source, '') != 'streams_clean'");
    }
    sql.push_str(" ORDER BY start_local");
    if limit > 0 {
        sql.push_str(" LIMIT ?");
        params.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
    }
    let ids = activity_ids(conn, &sql, params_from_iter(params))?;
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
        }
    }
    if result.refined > 0 {
        result.rebuilt = recompute_novelty_from_stored_tiles_with_conn(settings, conn)?.rebuilt;
    }
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
