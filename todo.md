# TileHarvester Todo

## 🐛 Bugs & Critical Fixes

- [x] **Fix `NameError` in `sync.py` (`sync_once`)**
  - `skipped += cur.rowcount` on line 665 references `cur` which is never assigned in that branch
  - `skipped` variable is never initialized in `sync_once()`
  - This code path will crash at runtime when an existing activity has no summary polyline and is not an ignored sport
  - Fix: initialize `skipped = 0` and assign `cur = conn.execute(...)` before accessing `.rowcount`
  - **Done** — `skipped` initialized to 0, `cur = conn.execute(...)` assigned. `skipped` also added to return dict.
  - Also removed dead `elif summary and not existing["has_gps"]` branch (unreachable, already caught by `if summary:`).

- [x] **Fix `NameError` in `sync.py` (`sync_once`) - duplicate check**
  - Verify no other variables are used before assignment in the same function
  - **Done** — audited, no other uninitialized variables found.

## 🎨 Code Quality & Linting

- [x] **Add Ruff configuration**
  - Added `[tool.ruff]` section to `pyproject.toml` (line-length 100)
  - Enabled rule categories: `E`, `F`, `I`, `N`, `W`, `UP`, `B`, `C4`, `SIM`, `ARG`
  - Ran `ruff check .` and fixed all 31 issues (23 auto-fixed, 9 manual)
  - Added `ruff format` as formatter (quote-style double, indent-space)

- [x] **Add type checking with mypy**
  - Added `mypy>=1.11.0` to dev dependencies in `pyproject.toml`
  - Added `[tool.mypy]` to `pyproject.toml` with `strict = true`, `ignore_missing_imports = true`
  - Fixed all 50 type errors across 10 source files (generic dict, missing return types, missing param types, sqlite3.Row, resp.json() Any returns)
  - Zero mypy errors on `tileharvester/`

- [x] **Add pre-commit hooks**
  - Created `.pre-commit-config.yaml`
  - Includes: ruff (lint + format), mypy (local hook via `uv run`), trailing-whitespace, end-of-file-fixer, check-yaml, check-added-large-files
  - Ran `pre-commit install` and `pre-commit run --all-files` — all hooks pass

- [x] **Clean up import style**
  - Standardized on absolute imports throughout
  - Fixed lazy imports: `import time` → top of `cli.py`, `get_activity`/`get_activity_streams` → top of `cli.py`, `settings` → top of `tile_engine.py` (no circular dependency)
  - `make_engine` retains backward-compatible optional `config` parameter

## 🧪 Testing

- [ ] **Add tests for `sync.py` core logic**
  - Test `fetch_and_store_summaries` with mock Strava responses
  - Test `compute_activity_tiles` with mock GPS streams
  - Test `compute_historical_novelty` with seeded DB data
  - Test `annotate_activity` with mocked Strava API calls
  - Test `backfill` flow end-to-end with mocks
  - Test `sync_once` incremental sync logic

- [ ] **Add tests for `db.py`**
  - Test migrations run correctly on fresh DB
  - Test `get_setting` / `set_setting` roundtrip
  - Test `reset()` clears all tables
  - Test schema version tracking

- [ ] **Add tests for `strava_client.py`**
  - Mock `httpx` requests for all API calls
  - Test token refresh logic (expired vs valid tokens)
  - Test `_rate_limit_sleep` behavior
  - Test `exchange_code` saves tokens correctly
  - Test `is_authenticated` checks

- [ ] **Add tests for `cli.py` commands**
  - Use `typer.testing.CliRunner` to test each command
  - Test `auth` flow with mocked browser and code exchange
  - Test `status` output formatting
  - Test `reset_db --force` requires flag
  - Test command exit codes (auth required, etc.)

- [ ] **Add integration tests**
  - Full flow: store summary → process tiles → annotate → verify description updated
  - Use temporary SQLite DB and mocked Strava API

- [ ] **Set up test coverage reporting**
  - Add `pytest-cov` to dev dependencies
  - Add coverage config to `pyproject.toml` (target 80%+)
  - Add coverage badge to README

## 🏗️ Refactoring

- [x] **Break up `sync.py` (1,010 lines)**
  - Extract `backfill.py` — backfill-specific logic
  - Extract `annotate.py` — description annotation logic
  - Extract `refine.py` — stream refinement logic
  - Extract `recompute.py` — recompute and novelty rebuild logic
  - Keep `sync.py` for the core `sync_once` orchestration only

- [x] **Fix lazy imports**
  - `tile_engine.py:make_engine()` imports `settings` at top-level — already fixed
  - `cli.py:validate()` imports `get_activity`, `get_activity_streams`, `make_engine` at top-level — already fixed
  - `cli.py:sync()` imports `time` at top-level — already fixed

- [x] **Fix `LINE_PATTERN` in `descriptions.py`**
  - Changed from module-level constant to `_get_line_pattern()` function
  - Builds regex on demand from current settings, supporting runtime emoji/prefix changes
  - Updated `update_description_line`, `remove_description_line`, `has_description_line` to call `_get_line_pattern()`

- [x] **Extract constants and magic numbers**
  - `sync_once` lookback: `timedelta(days=7)` → `settings.sync_lookback_days`
  - `sync_once` annotation window: `timedelta(days=1)` → `settings.sync_annotation_window_days`
  - `refine_streams` default limit of 80 → `settings.refine_default_limit`
  - Added all three to `Settings` model in `config.py`

## 🚀 CI/CD

- [ ] **Add GitHub Actions workflow**
  - Create `.github/workflows/ci.yml`
  - Run on push/PR to main branch
  - Steps:
    1. Checkout code
    2. Set up Python 3.12
    3. Install `uv`
    4. `uv sync --group dev`
    5. `uv run ruff check .`
    6. `uv run ruff format --check .`
    7. `uv run mypy tileharvester/`
    8. `uv run pytest --cov=tileharvester --cov-report=xml`
  - Upload coverage to Codecov or similar

- [ ] **Add release workflow**
  - Tag-based releases
  - Build wheel and sdist
  - Publish to PyPI (optional, but nice)

## 📝 Documentation

- [ ] **Add CONTRIBUTING.md**
  - Development setup (`uv sync --group dev`)
  - Running tests
  - Code style (ruff, mypy)
  - Pre-commit setup

- [ ] **Add architecture/decisions doc**
  - Why SQLite vs other DBs
  - Why two tile computation paths (summary vs streams)
  - Why Mapbox/XYZ tile system
  - Strava API rate limiting strategy

- [ ] **Add inline docstrings where missing**
  - `sync.py` functions lack docstrings (e.g., `_store_activity_tiles`, `compute_period_totals`)
  - `db.py` migration functions
  - Complex algorithms in `tile_engine.py`

## 🔒 Security & Robustness

- [ ] **Validate environment variables more strictly**
  - `strava_client_id` and `strava_client_secret` are empty strings by default — should fail fast with clear error
  - Add validation in `Settings` model or `config.py`

- [ ] **Add request retries for Strava API**
  - `httpx` client has no retry logic
  - Network blips or 5xx errors will fail permanently
  - Consider `httpx` transport with retries or wrap calls in retry decorator

- [ ] **Sanitize SQL inputs**
  - Most queries use parameterized statements (good!)
  - Audit any string interpolation in SQL (e.g., `IGNORED_SPORTS` placeholders)
  - Ensure `tile_id` values are safe before inserting

- [ ] **Handle edge cases in GPS stream cleaning**
  - Empty streams
  - Single-point streams
  - All points filtered out by speed/distance checks
  - Very long activities with thousands of points

## ✨ Polish

- [x] **Add `--version` flag to CLI**
  - Read version from `pyproject.toml` via `importlib.metadata`
  - Added `--version` option to callback; `invoke_without_command=True` + `no_args_is_help=True` for bare CLI invocation

- [x] **Improve error messages**
  - Added `StravaError` exception hierarchy with `classify_strava_error()` for auth/ratelimit/network/data/server errors
  - All CLI commands now catch and classify Strava API errors with actionable suggestions
  - Error storage in DB (annotate.py, sync.py) now uses classified error messages

- [x] **Add progress bars for long operations**
  - Replaced custom `_print_progress` ASCII bar with `rich.progress.track` in backfill.py, refine.py, recompute.py
  - Added progress bars to `sync_once` tile processing and annotation phases
  - Added progress bars to `retry_failed` with rich `console.log` for per-item errors
  - Added `rich>=13.0.0` to project dependencies

- [x] **Clean up repository**
  - Verified `.gitignore` works correctly (confirmed via `git ls-files`, `git status --ignored`)
  - Added `.idea/`, `.vscode/`, `*.swp`, `*.swo`, `*~` to `.gitignore`
  - No committed `.pyc` or `__pycache__` files found in the repo

- [x] **Add health check endpoint or command**
  - Added `health` command that verifies: DB accessibility, Strava authentication, API rate limit status
  - Added `--no-check-rate-limit` flag to skip rate limit check (saves API calls)
  - Added `get_rate_limit_status()` to strava_client.py for lightweight rate limit queries
