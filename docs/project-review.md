# Whole-project review

Reviewed `master` at `ddbac39`, the complete merge-base diffs for PRs #6 and #8,
all Python modules, existing and locally drafted tests, packaging, CI, Docker,
systemd generation, and setup documentation. The replacement branch incorporates
both PRs. Local draft tests were copied into `tests/extended` and updated for the
current interfaces; the original checkout and its uncommitted files were preserved.

This was a whole-project audit, so the findings below include existing defects as
well as problems in the proposed KML feature. Locations refer to the repaired code.
All listed findings have fixes and regression coverage unless otherwise stated.

## Findings and resolutions

### [P1] Stop traversal at an exact grid endpoint — `tileharvester/tile_engine.py:130`

A segment from tile coordinates `(1.5, 1.5)` to `(1.0, 2.0)` steps past the endpoint
on one axis and never terminates. Reproduced in a subprocess that exceeded its
timeout. Traversal now stops at the endpoint; forward, reverse, and transposed
cases have bounded regression tests.

### [P1] Preserve valid history when refinement fails — `tileharvester/sync.py:569`

A network error or empty GPS response changed a previously processed activity to
`failed` or `skipped_no_gps`. Queries then excluded its stored tiles, and a rebuild
removed them from global totals. Refinement now preserves the processed status and
tiles, records the error, and remains eligible for a later refinement attempt.

### [P2] Repair later novelty when older history arrives — `tileharvester/history.py:6`

Processing a later ride and then an earlier ride over the same tile left both with
`+1 new`, inflating period totals. Every tile mutation now reconciles the affected
tiles and every activity that visits them in the same transaction. Full recomputes
use the same logic. Tests compare incremental results with complete rebuilds.

### [P2] Remove obsolete ownership when replacing a route — `tileharvester/sync.py:514`

Replacing summary tiles with different stream tiles inserted new global entries
but left the old route's global entries behind. Reconciliation includes both the
removed and replacement tile sets and assigns any remaining owner correctly.

### [P2] Use chronological order and a stable tie-breaker — `tileharvester/history.py:67`

Wall-clock ordering can reverse activities recorded in different timezones. The
KML queries also compared timestamp strings for equality, so `10:00Z` and
`12:00+02:00` were both considered novel. Ownership and lifetime comparisons now
use UTC instants followed by activity ID, including equivalent timestamp strings.

### [P2] Reconcile sport eligibility changes — `tileharvester/sync.py:221`

Changing a processed ride to an ignored sport left its global tiles and later
novelty stale. Changing an ignored activity back without a summary could leave it
ignored forever; recompute could instead mark an activity processed without ever
computing tiles. Shared summary handling now repairs history, restores valid
stored data, or queues a stream check. Refinement also includes legacy NULL sports.

### [P2] Retry failed work during ordinary polling — `tileharvester/sync.py:694`

Once a transient processing or annotation error changed a status to `failed`, the
normal polling queries never selected it again. Polling now retries failed
processing and eligible recent failed annotations. `sync --once` returns a
nonzero exit status for partial failures. Successful annotations remain idempotent.

### [P2] Reject oversized streams without storing partial routes — `tileharvester/sync.py:141`

Streams exceeding 50,000 points were truncated and marked fully refined, silently
losing the rest of long rides. They now fail explicitly with instructions to raise
`TH_STREAM_MAX_POINTS` and retry. Existing valid history is preserved. Invalid
coordinates fail the individual activity instead of aborting an entire backfill.

### [P2] Respect both rate-limit families and reset windows — `tileharvester/strava_client.py:250`

The client ignored the 15-minute and non-upload limits and slept only 15 minutes
when approaching the daily limit. It now considers both header families and waits
until the applicable reset, including midnight UTC for daily limits. Behavior was
checked against [Strava's rate-limit documentation](https://developers.strava.com/docs/rate-limits/)
and verified with a mocked clock and sleep function.

### [P2] Write OAuth tokens privately and atomically — `tileharvester/strava_client.py:160`

Ordinary file creation can expose access and refresh tokens to other local users
under a typical umask. Truncating the existing token file also loses authentication
if a replacement write fails. Tokens are now written to a mode-0600 temporary file,
flushed, and atomically replaced. Failure tests verify the old token survives.

### [P2] Commit baseline import and novelty rebuild together — `tileharvester/kml_baseline.py:206`

PR #8 committed its immutable baseline before rebuilding novelty. If the rebuild
failed, the command reported failure but retry was blocked by the installed
baseline, and existing counts were inconsistent. Import and rebuild now share one
transaction; injected rebuild failures roll back the installation.

### [P2] Include the baseline when validating an unstored activity — `tileharvester/sync.py:404`

`validate <id>` fetches metadata without inserting an activity. PR #8 only applied
baseline-aware comparisons when a database row existed, reporting baseline tiles
as new for an unstored ride. Validation now passes the fetched UTC timestamp and
builds a transient comparison row without writing the activity to the database.

### [P2] Reject incomplete or incompatible authoritative snapshots — `tileharvester/kml_baseline.py:144`

PR #8 accepted a Squadrats-only KML as an authoritative snapshot with zero
Squadratinhos, replacing historical small-tile totals with zero. It also allowed
custom engine zooms to be mixed with fixed z14/z17 IDs. Both layers and compatible
zooms are now required, including checks after installation if configuration changes.

### [P2] Bound rasterization work as well as file size — `tileharvester/kml_baseline.py:92`

A small polygon can cover billions of z17 cells despite passing the KML byte-size
limit. The parser now bounds scan work and generated tiles before allocating an
unbounded set. The limits can reject unusually large or complex valid exports;
that failure is explicit and leaves the database unchanged.

### [P2] Preserve prose mentioning TileHarvester — `tileharvester/descriptions.py:8`

The old expression replaced an entire line such as “I tried TileHarvester: it
helped me explore.” Annotation matching now requires the configured prefix at the
start of the line, allowing leading emoji and whitespace. Ordinary prose survives.

## Existing PR fixes retained

- PR #6's persistent polling cursor recovers activities after downtime longer than
  the lookback window. A failed fetch does not advance the cursor.
- PR #6's week/month boundaries start at midnight, including morning activities on
  the first day of the period.
- PR #8's immutable KML baseline, hole-aware rasterization, read-only comparison,
  migrations, and historical compatibility remain available.
- The clone instructions now use the actual `TileHarvester` directory capitalization.

## Validation and remaining limits

- 201 tests passed on Python 3.10, 3.12, and 3.14. Python 3.12 statement coverage
  is 84%. The suite covers database migration, rollback, OAuth, annotation, parsing,
  tile traversal, sync, backfill, refinement, recompute, and CLI workflows.
- Test fixtures isolate storage and block real HTTP requests. No real Strava
  descriptions or production data were modified.
- Ruff lint, Ruff formatting, strict mypy, pre-commit hooks, source distribution,
  and wheel builds all passed. The installed wheel CLI version check passed from
  outside the source tree. CI runs Python 3.10, 3.12, and 3.14.
- Docker build/runtime could not be tested because the local Docker daemon was
  stopped. No systemd service was installed or started. Live Strava OAuth and API
  behavior, multi-process races, and large real-world KML exports remain integration
  test gaps. The KML examples used here are synthetic, without private location data.
- Existing annotations remain unchanged by default after history corrections;
  enable `TH_REWRITE_EXISTING_ANNOTATIONS=true` and recompute to request rewrites.
