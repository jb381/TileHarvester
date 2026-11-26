//! Rebuild stored activity routes and tile novelty.

use rusqlite::{params_from_iter, TransactionBehavior};

use crate::config::Settings;
use crate::db::{get_db, ActivityRow};
use crate::error::Result;
use crate::history::rebuild_tile_history;
use crate::models::{TileComputeStatus, TileSource};
use crate::sync::process_stored_summary;

#[derive(Debug)]
pub struct RecomputeNoveltyResult {
    pub rebuilt: i64,
}

#[derive(Debug, Default)]
pub struct RecomputeAllResult {
    pub rebuilt: usize,
    pub preserved: usize,
    pub skipped: usize,
    pub failed: usize,
}

pub fn recompute_novelty_from_stored_tiles(settings: &Settings) -> Result<RecomputeNoveltyResult> {
    recompute_novelty_from_stored_tiles_with_conn(settings, &mut get_db(settings)?)
}

pub(crate) fn recompute_novelty_from_stored_tiles_with_conn(
    settings: &Settings,
    conn: &mut rusqlite::Connection,
) -> Result<RecomputeNoveltyResult> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let rebuilt = tx.query_row(
        "SELECT COUNT(*) FROM activities WHERE status = 'processed'",
        [],
        |row| row.get(0),
    )?;
    rebuild_tile_history(&tx, None)?;
    if settings.rewrite_existing_annotations {
        tx.execute(
            "UPDATE activities SET annotation_status = 'none'
             WHERE status = 'processed' AND annotation_status = 'done'",
            [],
        )?;
    }
    tx.commit()?;
    Ok(RecomputeNoveltyResult { rebuilt })
}

pub fn recompute_all(settings: &Settings) -> Result<RecomputeAllResult> {
    recompute_all_with_conn(settings, &mut get_db(settings)?)
}

pub(crate) fn recompute_all_with_conn(
    settings: &Settings,
    conn: &mut rusqlite::Connection,
) -> Result<RecomputeAllResult> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let ignored = settings.ignored_sports();
    let mut restore = String::from(
        "UPDATE activities SET status = CASE WHEN tile_source IS NOT NULL THEN 'processed' ELSE 'pending' END
         WHERE status = 'skipped_ignored_sport'",
    );
    if !ignored.is_empty() {
        let placeholders = crate::db::in_placeholders(ignored.len());
        tx.execute(
            &format!(
                "UPDATE activities SET status = 'skipped_ignored_sport'
             WHERE sport_type IN ({placeholders}) AND status IN ('pending', 'processed', 'failed')"
            ),
            params_from_iter(&ignored),
        )?;
        restore.push_str(&format!(
            " AND COALESCE(sport_type, '') NOT IN ({placeholders})"
        ));
    }
    tx.execute(&restore, params_from_iter(&ignored))?;
    rebuild_tile_history(&tx, None)?;
    tx.commit()?;

    let rows = {
        let mut statement = conn.prepare(
            "SELECT * FROM activities WHERE status = 'processed' AND has_gps = 1 ORDER BY start_local",
        )?;
        let rows = statement
            .query_map([], ActivityRow::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut result = RecomputeAllResult::default();
    for row in rows {
        if row.tile_source == Some(TileSource::StreamsClean) {
            result.preserved += 1;
            continue;
        }
        let Some(tiles) = process_stored_summary(settings, conn, &row)? else {
            result.skipped += 1;
            continue;
        };
        match tiles.status {
            TileComputeStatus::Processed => result.rebuilt += 1,
            TileComputeStatus::SkippedNoGps => result.skipped += 1,
            TileComputeStatus::Failed => result.failed += 1,
        }
    }
    recompute_novelty_from_stored_tiles_with_conn(settings, conn)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::tests::{database, summary, MockApi};
    use crate::sync::{compute_activity_tiles_from_summary, require_activity, store_summary};

    #[test]
    fn eligibility_and_history_roll_back_together() {
        let (_dir, settings) = database();
        let conn = get_db(&settings).unwrap();
        store_summary(&settings, &conn, &summary(1, 1)).unwrap();
        compute_activity_tiles_from_summary(&settings, &MockApi::default(), 1).unwrap();
        conn.execute(
            "UPDATE activities SET sport_type = 'VirtualRide' WHERE id = 1",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_rebuild BEFORE DELETE ON global_tiles
             BEGIN SELECT RAISE(ABORT, 'rebuild interrupted'); END;",
        )
        .unwrap();
        assert!(recompute_all(&settings).is_err());
        let row = require_activity(&conn, 1).unwrap();
        assert_eq!(row.status.as_str(), "processed");
        assert_eq!(row.new_squadrat_count, 1);
        conn.execute_batch("DROP TRIGGER fail_rebuild").unwrap();
        recompute_all(&settings).unwrap();
        let row = require_activity(&conn, 1).unwrap();
        assert_eq!(row.status.as_str(), "skipped_ignored_sport");
        assert_eq!(row.new_squadrat_count, 0);
    }
}
