//! Reconcile tile ownership and novelty within the caller's transaction.

use std::collections::{HashMap, HashSet};

use rusqlite::{params, params_from_iter, Connection, OptionalExtension};

use crate::error::Result;
use crate::models::{ActivityStatus, TileKind};
use crate::tile_engine::FxBuildHasher;

const KINDS: [TileKind; 2] = [TileKind::Squadrat, TileKind::Squadratinho];

#[derive(Debug, Default, Clone)]
pub struct AffectedTiles {
    pub squadrat: HashSet<String>,
    pub squadratinho: HashSet<String>,
}

impl AffectedTiles {
    pub fn add(&mut self, tile_kind: TileKind, tile_id: String) {
        match tile_kind {
            TileKind::Squadrat => self.squadrat.insert(tile_id),
            TileKind::Squadratinho => self.squadratinho.insert(tile_id),
        };
    }

    pub fn for_kind(&self, kind: TileKind) -> &HashSet<String> {
        match kind {
            TileKind::Squadrat => &self.squadrat,
            TileKind::Squadratinho => &self.squadratinho,
        }
    }
}

/// Reconcile every tile, or only the supplied tiles. After removing activity
/// tile rows, callers must refresh counts for any activity no longer linked to
/// the affected tiles.
pub fn rebuild_tile_history(conn: &Connection, affected: Option<&AffectedTiles>) -> Result<()> {
    let baseline: Option<String> = conn
        .query_row(
            "SELECT as_of_utc FROM baseline_imports WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(affected) = affected else {
        return rebuild_all(conn, baseline.as_deref());
    };
    let mut touched = HashSet::new();

    let mut flag = conn.prepare(
        "UPDATE activity_tiles SET is_new = ? WHERE activity_id = ? AND tile_kind = ? AND tile_id = ?",
    )?;
    let mut owner = conn.prepare("INSERT INTO global_tiles VALUES (?, ?, ?, ?)")?;
    for kind in KINDS {
        let mut tiles: Vec<&str> = affected.for_kind(kind).iter().map(String::as_str).collect();
        tiles.sort_unstable();
        for chunk in tiles.chunks(crate::db::SQL_CHUNK_SIZE) {
            let placeholders = crate::db::in_placeholders(chunk.len());
            let tile_params = || std::iter::once(kind.as_str()).chain(chunk.iter().copied());
            conn.execute(
                &format!(
                    "DELETE FROM global_tiles WHERE tile_kind = ? AND tile_id IN ({placeholders})"
                ),
                params_from_iter(tile_params()),
            )?;
            let mut baseline_seen = if baseline.is_some() {
                crate::db::baseline_tiles_in_chunk(conn, kind.as_str(), chunk)?
            } else {
                HashSet::new()
            };

            let mut stmt = conn.prepare(&format!(
                "SELECT at.tile_id, a.id, a.status, a.start_local,
                        (a.baseline_covered = 1 OR julianday(a.start_utc) > julianday(?)), at.is_new
                 FROM activity_tiles at JOIN activities a ON a.id = at.activity_id
                 WHERE at.tile_kind = ? AND at.tile_id IN ({placeholders})
                 ORDER BY julianday(a.start_utc), a.id"
            ))?;
            let rows = stmt
                .query_map(
                    params_from_iter(
                        std::iter::once(baseline.as_deref()).chain(tile_params().map(Some)),
                    ),
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, ActivityStatus>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<bool>>(4)?.unwrap_or(false),
                            row.get::<_, i64>(5)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut legacy_seen = HashSet::new();
            let mut owned = HashSet::new();
            for (tile, id, status, start_local, uses_baseline, old_is_new) in rows {
                touched.insert(id);
                let mut is_new = false;
                if status == ActivityStatus::Processed {
                    let legacy_new = legacy_seen.insert(tile.clone());
                    is_new = if baseline.is_some() && uses_baseline {
                        baseline_seen.insert(tile.clone())
                    } else {
                        legacy_new
                    };
                    if is_new && owned.insert(tile.clone()) {
                        owner.execute(params![kind, tile, id, start_local])?;
                    }
                }
                if i64::from(is_new) != old_is_new {
                    flag.execute(params![is_new, id, kind, tile])?;
                }
            }
        }
    }

    let mut counts = conn.prepare(
        "UPDATE activities SET (new_squadrat_count, new_squadratinho_count) = (
            SELECT COALESCE(SUM(CASE WHEN tile_kind = 'squadrat' THEN is_new END), 0),
                   COALESCE(SUM(CASE WHEN tile_kind = 'squadratinho' THEN is_new END), 0)
            FROM activity_tiles WHERE activity_id = activities.id
         ) WHERE id = ?",
    )?;
    for id in touched {
        counts.execute([id])?;
    }
    Ok(())
}

fn rebuild_all(conn: &Connection, baseline: Option<&str>) -> Result<()> {
    conn.execute("DELETE FROM global_tiles", [])?;
    conn.execute("UPDATE activity_tiles SET is_new = 0 WHERE is_new <> 0", [])?;
    let mut counts: HashMap<i64, [i64; 2], FxBuildHasher> = {
        let mut stmt = conn.prepare("SELECT id FROM activities")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, [0, 0])))?;
        rows.collect::<rusqlite::Result<_>>()?
    };

    let mut load_baseline =
        conn.prepare("SELECT tile_id FROM baseline_tiles WHERE tile_kind = ?")?;
    let ordered_processed = {
        let mut stmt = conn.prepare(
            "SELECT id, start_local,
                    (baseline_covered = 1 OR julianday(start_utc) > julianday(?))
             FROM activities
             WHERE status = 'processed'
             ORDER BY julianday(start_utc), id",
        )?;
        let rows = stmt.query_map([baseline], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                baseline.is_some() && row.get::<_, Option<bool>>(2)?.unwrap_or(false),
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut tiles =
        conn.prepare("SELECT tile_id FROM activity_tiles WHERE activity_id = ? AND tile_kind = ?")?;
    let mut flag = conn.prepare(
        "UPDATE activity_tiles SET is_new = 1 WHERE activity_id = ? AND tile_kind = ? AND tile_id = ?",
    )?;
    let mut owner = conn.prepare("INSERT INTO global_tiles VALUES (?, ?, ?, ?)")?;
    for (index, kind) in KINDS.into_iter().enumerate() {
        let baseline_tiles: HashSet<String, FxBuildHasher> = if baseline.is_some() {
            load_baseline
                .query_map([kind], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        } else {
            HashSet::default()
        };

        #[derive(Default)]
        struct Visits {
            legacy: bool,
            baseline: bool,
            owned: bool,
        }
        let mut seen = HashMap::<String, Visits, FxBuildHasher>::default();
        for (id, start_local, uses_baseline) in &ordered_processed {
            let mut rows = tiles.query(params![id, kind])?;
            while let Some(row) = rows.next()? {
                let tile = row.get_ref(0)?.as_str().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                let visit = if let Some(visit) = seen.get_mut(tile) {
                    visit
                } else {
                    seen.entry(tile.to_owned()).or_insert_with(|| Visits {
                        baseline: baseline_tiles.contains(tile),
                        ..Visits::default()
                    })
                };
                let legacy_new = !std::mem::replace(&mut visit.legacy, true);
                let is_new = if *uses_baseline {
                    !std::mem::replace(&mut visit.baseline, true)
                } else {
                    legacy_new
                };
                if is_new {
                    flag.execute(params![id, kind, tile])?;
                    counts.entry(*id).or_default()[index] += 1;
                    if !std::mem::replace(&mut visit.owned, true) {
                        owner.execute(params![kind, tile, id, start_local])?;
                    }
                }
            }
        }
    }

    let mut update_counts = conn.prepare(
        "UPDATE activities SET new_squadrat_count = ?, new_squadratinho_count = ? WHERE id = ?",
    )?;
    for (id, [large, small]) in counts {
        update_counts.execute(params![large, small, id])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Settings, db};

    fn counts(conn: &Connection) -> rusqlite::Result<Vec<(i64, i64)>> {
        let mut stmt = conn.prepare(
            "SELECT new_squadrat_count, new_squadratinho_count FROM activities ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    }

    #[test]
    fn incremental_matches_full_with_and_without_baseline() -> Result<()> {
        for baseline in [false, true] {
            let dir = tempfile::tempdir()?;
            let settings = Settings::with_data_dir(dir.path());
            db::migrate(&settings)?;
            let mut conn = db::get_db(&settings)?;
            let tx = conn.transaction()?;
            tx.execute_batch(
                "INSERT INTO activities (id, start_utc, start_local, status, baseline_covered)
                 VALUES (1, '2026-08-01T10:00:00Z', 'first', 'processed', 0),
                        (2, '2026-08-01T12:00:00+02:00', 'second', 'processed', 1),
                        (3, '2026-08-02T10:00:00Z', 'third', 'processed', 0),
                        (4, '2026-08-02T10:00:00Z', 'fourth', 'pending', 0),
                        (5, '2026-07-31T10:00:00Z', 'ignored', 'skipped_ignored_sport', 0);
                 INSERT INTO activity_tiles VALUES
                     (4, 'squadrat', 'shared', 1), (3, 'squadrat', 'shared', 0),
                     (2, 'squadrat', 'shared', 0), (1, 'squadrat', 'shared', 0),
                     (2, 'squadratinho', 'small', 0), (3, 'squadratinho', 'small', 0),
                     (5, 'squadrat', 'shared', 1);
                 INSERT INTO global_tiles VALUES ('squadrat', 'removed', 1, 'first');",
            )?;
            if baseline {
                tx.execute_batch(
                    "INSERT INTO baseline_imports VALUES (1, 'snapshot', 'hash', 'now', '2026-08-01T10:00:00Z', 0, 1);
                     INSERT INTO baseline_tiles VALUES (1, 'squadratinho', 'small');",
                )?;
            }
            let mut affected = AffectedTiles::default();
            for tile in ["shared", "removed"] {
                affected.add(TileKind::Squadrat, tile.into());
            }
            affected.add(TileKind::Squadratinho, "small".into());
            for i in 0..500 {
                affected.add(TileKind::Squadrat, format!("placeholder-{i}"));
            }
            rebuild_tile_history(&tx, Some(&affected))?;
            let expected = if baseline {
                vec![(1, 0), (1, 0), (0, 0), (0, 0), (0, 0)]
            } else {
                vec![(1, 0), (0, 1), (0, 0), (0, 0), (0, 0)]
            };
            assert_eq!(counts(&tx)?, expected);
            let globals = |conn: &Connection| -> rusqlite::Result<Vec<(String, i64, String)>> {
                let mut stmt = conn.prepare("SELECT tile_id, first_activity_id, first_seen_local FROM global_tiles ORDER BY tile_kind, tile_id")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                rows.collect()
            };
            let before = globals(&tx)?;
            assert_eq!(before[0], ("shared".into(), 1, "first".into()));
            assert_eq!(before.len(), if baseline { 1 } else { 2 });
            let novelty_flags = || -> rusqlite::Result<Vec<i64>> {
                let mut stmt = tx.prepare(
                    "SELECT is_new FROM activity_tiles ORDER BY activity_id, tile_kind, tile_id",
                )?;
                let rows = stmt.query_map([], |row| row.get(0))?;
                rows.collect()
            };
            let incremental_flags = novelty_flags()?;
            rebuild_tile_history(&tx, None)?;
            assert_eq!(novelty_flags()?, incremental_flags);
            assert_eq!(counts(&tx)?, expected);
            assert_eq!(globals(&tx)?, before);
            tx.rollback()?;
            assert!(counts(&conn)?.is_empty());
            assert!(globals(&conn)?.is_empty());
        }
        Ok(())
    }
}
