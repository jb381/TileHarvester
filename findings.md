# TileHarvester — Deep Code Review Findings

**Date:** May 12, 2026
**Scope:** All Python source files, tests, config, docs
**Lines reviewed:** ~2,619 across 17 source files

---

## 🔴 Critical

### 1. Antimeridian tile crossing produces wildly wrong results (tile_engine.py:60–112)

**File:** `tileharvester/tile_engine.py`
**Lines:** 60–66 (DDA loop), 74–112 (segment computation)

The DDA raytracing algorithm in `_tiles_for_segment` cannot handle crossing the antimeridian (±180° longitude). At zoom 14 the world is 2¹⁴ = 16,384 tiles wide. When crossing from lon≈179.9 (x≈16383.9) to lon≈-179.9 (x≈0.06), `dx ≈ -16383.8`, step_x = -1, and the loop iterates across ~16,384 tiles — the *long way* around the world — instead of wrapping across the antimeridian (a 1-tile crossing).

**Impact:** Any route that crosses the Pacific dateline (e.g., flights between Oceania and the Americas, some long-distance cycling tours) will produce **millions of phantom tiles**, corrupting all counts permanently.

**Fix:** Detect antimeridian crossings in `_tiles_for_segment` and split the segment at ±180° longitude, processing each side independently.

### 2. `has_gps` creates permanent deadlock for activities without summary polyline (sync.py:166, 447–449, 474–480)

**File:** `tileharvester/sync.py`
**Lines:** 166 (has_gps detection), 447–449 (compute_activity_tiles guard), 474–480 (compute_activity_tiles_from_summary guard)

`has_gps = bool(summary_polyline)` at line 166. Strava does not provide summary polylines for all activities (some types, complex routes, or very long activities lack them). If `summary_polyline` is None/empty:
1. `has_gps` is set to 0 (stored as `skipped_no_gps`)
2. `compute_activity_tiles` short-circuits on `not row["has_gps"]`
3. `compute_activity_tiles_from_summary` also short-circuits

The activity is **permanently orphaned** — never processed, never refinable. No code path fetches the full GPS stream for these activities.

**Fix:** Remove the `has_gps` guard, or attempt stream fetching even when `has_gps` is False. Activities deserve at least one attempt to get full GPS data.

### 3. Stale annotation query is a logical no-op — always returns 0 (cli.py:256–269)

**File:** `tileharvester/cli.py`
**Lines:** 256–269 (stale count subqueries)

The two subqueries for the `stale` count are:
```sql
AND new_squadrat_count != (
    SELECT new_squadrat_count FROM activities a2 WHERE a2.id = activities.id
)
```
This compares `new_squadrat_count` with **itself** via a self-join — it is **always equal**. The `stale` counter in `tileharvester status` will perpetually show 0, giving false confidence that all annotations are current.

**Fix:** The intended query should compare the stored `new_squadrat_count` against a **recomputed value**, or join against a subquery that calculates current counts.

### 4. `temp_settings` fixture is not defined — test always fails (tests/test_descriptions.py:27)

**File:** `tests/test_descriptions.py`
**Lines:** 27–32

`test_replace_existing_with_different_emoji` accepts a `temp_settings` parameter and calls `monkeypatch.setattr(..., temp_settings)`, but no `conftest.py` exists and no fixture named `temp_settings` is defined anywhere. This test raises `NameError` on every single run.

**Fix:** Either define the fixture or remove the test dependency on it.

### 5. No tests for core logic — entire pipeline is unverified

**Missing tests for:**
- `sync.py` (0 tests) — sync orchestration, tile storage, period totals, novelty computation
- `strava_client.py` (0 tests) — retry logic, rate limit parsing, token refresh
- `backfill.py` (0 tests)
- `annotate.py` (0 tests)
- `recompute.py` (0 tests)
- `refine.py` (0 tests)
- `db.py` (0 tests) — migration execution, CRUD, edge cases

**Impact:** The entire Strava OAuth + pagination + tile computation pipeline has zero automated verification. Every deployment is a manual test.

---

## 🟡 High Priority

### 6. SQLite race conditions — no WAL mode (db.py:84–89)

**File:** `tileharvester/db.py`
**Lines:** 84–89 (`get_db()`)

`get_db()` does not enable WAL mode (`PRAGMA journal_mode=WAL`). Without WAL:
- Concurrent reads block during writes
- The token file (`strava_client.py:158–166`) uses `write_text`/`read_text` without file locking
- Two overlapping cron/systemd syncs can corrupt the token JSON or lose concurrent DB writes

**Fix:** Add `PRAGMA journal_mode=WAL` and `PRAGMA busy_timeout=5000` to `get_db()`.

### 7. Circular import path between sync.py and annotate.py (sync.py:691–695, annotate.py:14–17)

**File:** `tileharvester/annotate.py` (lines 14–17) + `tileharvester/sync.py` (lines 691–695)

`annotate.py` imports `compute_period_totals` and `compute_total_unique_squadrats_through` from `sync.py` at module level. `sync.py` lazily imports `annotate_activity` from `annotate.py` inside a backward-compat wrapper (line 693). While it works via lazy import, it's extremely fragile. Any change that causes `annotate.py` to load before `sync.py` finishes its own module-level imports causes a circular `ImportError`.

**Fix:** Extract shared types/utilities to a `common.py` module that neither `sync.py` nor `annotate.py` depends on.

### 8. Duplicated activity-fetching logic diverges (sync.py:154–237 vs 550–612)

**File:** `tileharvester/sync.py`
**Lines:** 154–237 (`fetch_and_store_summaries`), 550–612 (`sync_once` core)

The same activity upsert logic (summary polyline, has_gps, status cascade) is duplicated nearly identically. `sync_once` lacks the `updated` counter logic from `fetch_and_store_summaries`, and `fetch_and_store_summaries` doesn't commit per-batch like `sync_once`. Any bug fix to one must be manually ported to the other.

### 9. Backward-compat wrappers add no value (sync.py:691–725)

Five functions at the bottom of sync.py are one-line `from ... import ...; return ...()` wrappers. They exist solely so CLI commands can import from `sync` rather than the actual module. This inverses the dependency and created the circular import problem. No deprecation warning, no backward-incompatible change — these are pure indirection.

### 10. N+1 database connections in recompute (recompute.py:39, 195)

**File:** `tileharvester/recompute.py`
**Lines:** 39 (loop over activities), 195 (via `_store_activity_tiles`)

Inside the loop over activities, `with get_db() as conn:` opens a new SQLite connection on every single iteration. For 10,000 activities, that's 10,000 SQLite file open/close cycles.

**Fix:** Open one connection for the entire operation.

### 11. Network failures don't roll back DB (sync.py:550–612)

Activities are inserted/updated and committed (line 612) BEFORE stream fetching and tile computation. If stream fetching fails partway through, already-committed records are in an inconsistent state (stored but not processed). `retry_failed` can partially recover, but there's no transactional guarantee across the entire sync cycle.

### 12. 429 (rate limit) not retried (strava_client.py:28, 44–45)

The retry logic explicitly does not retry on 4xx, including 429. When a burst rate limit is hit, the entire operation fails immediately. `_rate_limit_sleep` is a preventive throttle but cannot predict all limits.

### 13. 15-minute rate limit never checked (strava_client.py:236–247)

`_rate_limit_sleep` only checks daily usage. Strava enforces both a 15-minute and a daily cap. The code parses 15-minute headers (line 149–150) but never uses them for throttling.

### 14. Blanket `Exception` catches mask real errors (multiple files)

**Files:** `backfill.py:136`, `cli.py:97,118,136,164,175,194,220,512`, `sync.py:452–456`

Every command and many internal functions catch `Exception` broadly. `KeyboardInterrupt`, `SystemExit`, or `MemoryError` are caught too. A user pressing Ctrl+C during `sync --once` gets "Sync failed: ..." instead of a clean exit.

### 15. OAuth tokens stored in plaintext JSON (strava_client.py:158–166)

`access_token`, `refresh_token`, and `expires_at` are written to `strava_tokens.json` as plain JSON. Any process or user with filesystem access to `~/.local/share/tileharvester/` can hijack the Strava session indefinitely via the refresh token.

### 16. Backfill counts `skipped_no_gps` as processed (backfill.py:64–69)

`result['status'] in ('processed', 'skipped_no_gps')` increments the `processed` counter for both statuses. But `skipped_no_gps` activities were NOT processed. They silently disappear from the final report totals because the `source` check (lines 66–69) misses both branches.

### 17. `_parse_local` fragile timezone parsing (sync.py:30–39)

Strips timezone by counting dashes in the date string. A format like `"2024-01-01T12:00:00-03:00"` has exactly 3 dashes (2 from date + 1 from offset), which matches the `count("-") > 2` check, so it works — but only accidentally. Adding microseconds changes the dash count. A proper datetime parser should be used.

### 18. `make_engine()` silently ignores its config parameter (tile_engine.py:145)

Signature is `def make_engine(config: dict[str, Any] | None = None)` but `config` is annotated with `# noqa: ARG001` — never read. The engine always reads from global `settings`. Misleading API.

---

## 🟡 Medium Priority

### 19. `_latlon_to_meters` is dead code (tile_engine.py:29–33)

Defined and unit-tested (`test_meters_conversion`) but never called anywhere. Wasted maintenance burden.

### 20. `_prior_activity_tiles` performs one query per 500-tile chunk (sync.py:268–271)

For an activity with 3,000 distinct tiles, this executes 6 separate SQL queries. Each joins `activity_tiles` with `activities` and uses `IN (...)` with 500 bind parameters. O(N × tiles/500) queries for a batch sync.

### 21. All pending activity IDs loaded into memory (sync.py:615–618, backfill.py:51–54)

`fetchall()` loads every row into a Python list before iteration. For databases with hundreds of thousands of activities, this consumes significant memory.

### 22. `_get_line_pattern()` rebuilds regex on every call (descriptions.py:8–13)

The regex `SQUADRAT_PATTERN` is rebuilt every time `update_description_line`, `remove_description_line`, or `has_description_line` is called. Compile once with `functools.lru_cache` or `re.compile` at module level.

### 23. MySQL has no `conftest.py`

No shared fixtures for database mocking, temporary directories, settings overrides, or HTTP response mocking. Each test would need to reinvent infrastructure.

### 24. No Strava API mocking in dev deps (pyproject.toml)

No `pytest-httpx`, `respx`, or `responses` dependency. All API-dependent code is untestable without real credentials.

### 25. Zero-division risk in speed calc (mitigated) (sync.py:109)

`speed = distance / dt` — if two consecutive GPS timestamps are identical, `dt=0`. Guard `if dt and dt > 0` on line 108 protects it, but any refactor could expose it.

### 26. `utcnow()` deprecation for Python 3.12+ (sync.py:422,545,628; annotate.py:62)

Uses `datetime.utcnow()` which is deprecated since Python 3.12. Use `datetime.now(tz=datetime.UTC)` instead.

### 27. `compute_total_unique_squadrats_through` ambiguous ordering (sync.py:526–540)

Uses `start_local < ? OR a.id = ?` for ordering. Multiple activities with the same `start_local` timestamp but different IDs silently ignore non-target activities in novelty calculation, inflating "new" counts.

### 28. Schema version table created twice (db.py:71–74 + 115)

Migration #5 creates `schema_version` table, and `migrate()` function creates it too. The migration entry is redundant.

### 29. `_pending_status` misnamed (sync.py:51–54)

Name suggests it returns a "pending" status, but it returns `"pending"`, `"skipped_ignored_sport"`, or `"skipped_no_gps"`. `_initial_status` would be clearer.

### 30. Backfill has no progress model (cli.py:122–137)

Backfill paginates through activities with no `rich.progress` bar. User sees raw text lines with no ETA.

### 31. `reset-db` has no summary or dry-run mode (cli.py:520–529)

No count of what will be deleted. No `--dry-run` mode. Destructive with only `--force`.

### 32. `_month_start` missing docstring (sync.py:27)

`_week_start` has a docstring but `_month_start` doesn't.

### 33. UPSERT requires SQLite ≥ 3.24.0 but not documented (sync.py:385–406)

The `ON CONFLICT ... DO UPDATE SET WHERE` pattern requires SQLite ≥ 3.24.0. Minimum version is never documented or checked anywhere.

### 34. No CHECK constraint on status/tile_source columns (db.py:17–35)

Both `status` and `tile_source` are unconstrained text columns. A typo like `"skiped_no_gps"` silently creates a new status value instead of being caught.

### 35. Migration partial-failure risk (db.py:121–127)

`executescript` runs multiple SQL statements. If a later statement fails, earlier partial changes persist. Schema version is only written after success, so retry would see old version, but partial corrupt data remains.

---

## 🔵 Low Priority

### 36. `_summary_polyline` double conversion (sync.py:42–44)

`summary = (activity.get("map") or {}).get("summary_polyline")` followed by `return summary or None`. Could be one line: `return (activity.get("map") or {}).get("summary_polyline") or None`.

### 37. `exchange_code` exposes client_secret in POST body (strava_client.py:183–200)

Standard OAuth2, but the secret is a static credential. If the token file is compromised, the refresh_token allows indefinite access with no rotation mechanism.

### 38. Blocking sleep for 15 minutes on rate limit (strava_client.py:247)

`time.sleep(900)` is unconditional. No progress indicator, no interruptibility.

### 39. Auth callback URL could leak in error messages (cli.py:68–79)

Malformed callback URLs parsed incorrectly could cause the raw URL (containing `?code=`) to be returned as-is and potentially logged.

### 40. `test_meters_conversion` tests dead code (test_tile_engine.py)

Tests `_latlon_to_meters` which is defined but never called anywhere in the codebase.

---

## ✅ Looks Good

- Clean CLI structure with 13 well-documented commands via Typer + Rich
- Excellent `.env.example` with all 58 env vars documented
- Ruff + mypy + pre-commit configured with zero errors
- Multi-stage Dockerfile using `astral-sh/uv` image (fast, reproducible builds)
- Good error classification with `StravaAPIError` hierarchy (rate limit, auth, parse, not found, quota)
- SQLite migrations with version tracking (10 migrations)
- Idempotent annotation via regex — no duplicate lines on re-runs
- Systemd service/timer generation built in
- Todo.md documenting roadmap and completed refactoring
- Good separation of concerns after the refactoring (backfill, annotate, refine, recompute extracted from monolithic sync.py)
