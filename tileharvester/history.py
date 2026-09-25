"""Keep tile ownership and novelty consistent when stored history changes."""

import sqlite3


def rebuild_tile_history(
    conn: sqlite3.Connection,
    affected: dict[str, set[str]] | None = None,
) -> None:
    """Reconcile all history, or only changed tiles, within the caller's transaction.

    Ordering uses UTC and activity ID, including equal instants expressed with
    different offsets. Baseline-era processed history retains its own novelty;
    baseline-aware activities compare against the snapshot and later observations.
    """
    baseline = conn.execute("SELECT * FROM baseline_imports WHERE id = 1").fetchone()
    touched: set[int] = set()
    if affected is None:
        conn.execute("DELETE FROM global_tiles")
        conn.execute("UPDATE activity_tiles SET is_new = 0")
        touched.update(row[0] for row in conn.execute("SELECT id FROM activities"))
        affected = {
            kind: {
                row[0]
                for row in conn.execute(
                    "SELECT DISTINCT tile_id FROM activity_tiles WHERE tile_kind = ?", (kind,)
                )
            }
            for kind in ("squadrat", "squadratinho")
        }

    for kind, tile_ids in affected.items():
        tile_list = sorted(tile_ids)
        for index in range(0, len(tile_list), 500):
            chunk = tile_list[index : index + 500]
            placeholders = ",".join("?" for _ in chunk)
            conn.execute(
                f"DELETE FROM global_tiles WHERE tile_kind = ? AND tile_id IN ({placeholders})",
                (kind, *chunk),
            )
            baseline_seen = {
                row[0]
                for row in conn.execute(
                    f"SELECT tile_id FROM baseline_tiles WHERE tile_kind = ? AND tile_id IN ({placeholders})",
                    (kind, *chunk),
                )
            }
            legacy_seen: set[str] = set()
            owned: set[str] = set()
            rows = conn.execute(
                f"""
                SELECT at.tile_id, a.id, a.status, a.start_local,
                       (a.baseline_covered = 1 OR julianday(a.start_utc) > julianday(?)) AS uses_baseline
                FROM activity_tiles at JOIN activities a ON a.id = at.activity_id
                WHERE at.tile_kind = ? AND at.tile_id IN ({placeholders})
                ORDER BY julianday(a.start_utc), a.id
                """,
                (baseline["as_of_utc"] if baseline else None, kind, *chunk),
            ).fetchall()
            flags = []
            owners = []
            for row in rows:
                tile_id = row["tile_id"]
                aid = row["id"]
                touched.add(aid)
                is_new = False
                if row["status"] == "processed":
                    uses_baseline = baseline is not None and bool(row["uses_baseline"])
                    seen = baseline_seen if uses_baseline else legacy_seen
                    is_new = tile_id not in seen
                    legacy_seen.add(tile_id)
                    if uses_baseline:
                        baseline_seen.add(tile_id)
                    if is_new and tile_id not in owned:
                        owned.add(tile_id)
                        owners.append((kind, tile_id, aid, row["start_local"]))
                flags.append((int(is_new), aid, kind, tile_id))
            conn.executemany(
                "UPDATE activity_tiles SET is_new = ? WHERE activity_id = ? AND tile_kind = ? AND tile_id = ?",
                flags,
            )
            conn.executemany("INSERT INTO global_tiles VALUES (?, ?, ?, ?)", owners)

    conn.executemany(
        """
        UPDATE activities SET
            new_squadrat_count = (SELECT COALESCE(SUM(is_new), 0) FROM activity_tiles
                                  WHERE activity_id = activities.id AND tile_kind = 'squadrat'),
            new_squadratinho_count = (SELECT COALESCE(SUM(is_new), 0) FROM activity_tiles
                                     WHERE activity_id = activities.id AND tile_kind = 'squadratinho')
        WHERE id = ?
        """,
        [(aid,) for aid in touched],
    )
