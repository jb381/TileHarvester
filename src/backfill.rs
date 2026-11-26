//! Historical activity ingestion using summary routes where available.

use serde_json::Value;

use crate::config::Settings;
use crate::db::{get_db, set_setting_with_conn};
use crate::error::Result;
use crate::models::{TileComputeStatus, TileSource};
use crate::strava_client::StravaApi;
use crate::sync::{
    activity_ids, compute_activity_tiles_from_summary_with_conn,
    fetch_and_store_summaries_with_conn, INITIAL_BACKFILL_DONE_KEY, LAST_IGNORED_SPORTS_KEY,
};

#[derive(Debug, Default)]
pub struct BackfillResult {
    pub stored: usize,
    pub processed: usize,
    pub summary_processed: usize,
    pub stream_fallbacks: usize,
    pub failed: usize,
    pub skipped: usize,
    pub ignored: usize,
}

pub fn backfill(
    settings: &Settings,
    api: &dyn StravaApi,
    limit: Option<usize>,
) -> Result<BackfillResult> {
    backfill_with_conn(settings, &mut get_db(settings)?, api, limit)
}

pub(crate) fn backfill_with_conn(
    settings: &Settings,
    conn: &mut rusqlite::Connection,
    api: &dyn StravaApi,
    limit: Option<usize>,
) -> Result<BackfillResult> {
    const PAGE_SIZE: u32 = 200;
    let ignored_sports = settings.ignored_sports();
    let mut result = BackfillResult::default();
    let mut fetched = 0;
    for page in 1.. {
        let remaining = limit.map(|limit| limit - fetched);
        if remaining == Some(0) {
            break;
        }
        let batch = fetch_and_store_summaries_with_conn(
            settings,
            api,
            page,
            PAGE_SIZE,
            remaining,
            conn,
            &ignored_sports,
        )?;
        fetched += batch.fetched;
        result.stored += batch.stored;
        result.skipped += batch.skipped;
        result.ignored += batch.ignored;
        if batch.fetched == 0 {
            break;
        }
    }

    let ids = activity_ids(
        conn,
        "SELECT id FROM activities WHERE status = 'pending' ORDER BY start_local",
        [],
    )?;
    for id in ids {
        let tiles = compute_activity_tiles_from_summary_with_conn(settings, conn, api, id)?;
        match tiles.status {
            TileComputeStatus::Processed => {
                result.processed += 1;
                if tiles.source == Some(TileSource::SummaryPolyline) {
                    result.summary_processed += 1;
                } else {
                    result.stream_fallbacks += 1;
                }
            }
            TileComputeStatus::SkippedNoGps => result.skipped += 1,
            TileComputeStatus::Failed => result.failed += 1,
        }
    }
    set_setting_with_conn(
        conn,
        LAST_IGNORED_SPORTS_KEY,
        &Value::String(settings.canonical_ignored_sports()),
    )?;
    if limit.is_none() {
        set_setting_with_conn(conn, INITIAL_BACKFILL_DONE_KEY, &Value::Bool(true))?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::tests::{database, summary, MockApi};

    #[test]
    fn reads_short_pages_until_empty_or_limit() {
        for (limit, stored, last_page) in [(None, 3, 3), (Some(2), 2, 2)] {
            let (_dir, settings) = database();
            let api = MockApi {
                pages: vec![vec![summary(1, 1)], vec![summary(2, 2), summary(3, 3)]],
                ..Default::default()
            };
            let result = backfill(&settings, &api, limit).unwrap();
            assert_eq!(result.stored, stored);
            assert_eq!(result.summary_processed, stored);
            assert_eq!(
                *api.activity_requests.borrow(),
                (1..=last_page)
                    .map(|page| (page, 200, None))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn keeps_page_size_when_storing_partial_final_page() {
        let (_dir, settings) = database();
        let mut ignored = summary(1, 1);
        ignored["sport_type"] = serde_json::json!("VirtualRide");
        let api = MockApi {
            pages: vec![
                vec![ignored; 200],
                (2..=201).map(|id| summary(id, 2)).collect(),
            ],
            ..Default::default()
        };
        let result = backfill(&settings, &api, Some(201)).unwrap();
        assert_eq!(
            *api.activity_requests.borrow(),
            vec![(1, 200, None), (2, 200, None)]
        );
        assert_eq!(result.stored, 2);
        assert_eq!(result.ignored, 200);
        assert_eq!(result.summary_processed, 1);
        assert!(api.stream_requests.borrow().is_empty());
    }
}
