"""CLI entry point using Typer."""

import time
import webbrowser
from datetime import datetime
from importlib.metadata import PackageNotFoundError, version
from pathlib import Path
from urllib.parse import parse_qs, urlparse

import typer

from tileharvester.backfill import backfill as run_backfill
from tileharvester.config import settings
from tileharvester.db import get_db, migrate, reset
from tileharvester.recompute import recompute_all, recompute_novelty_from_stored_tiles
from tileharvester.refine import refine_streams
from tileharvester.strava_client import (
    build_auth_url,
    classify_strava_error,
    exchange_code,
    get_activity,
    get_activity_streams,
    get_rate_limit_status,
    is_authenticated,
)
from tileharvester.sync import (
    clean_stream_segments,
    compute_historical_novelty,
    retry_failed,
    sync_once,
)
from tileharvester.systemd import install_service, print_service
from tileharvester.tile_engine import make_engine


def _package_version() -> str:
    """Return installed package version, falling back to pyproject for source checkouts."""
    try:
        return version("tileharvester")
    except PackageNotFoundError:
        pyproject = Path(__file__).resolve().parents[1] / "pyproject.toml"
        if pyproject.exists():
            for line in pyproject.read_text().splitlines():
                if line.startswith("version = "):
                    return line.split("=", 1)[1].strip().strip('"')
        return "unknown"


__version__ = _package_version()

app = typer.Typer(
    help="TileHarvester - Automatically add Squadrats stats to Strava activities",
    no_args_is_help=True,
)


@app.callback(invoke_without_command=True)
def callback(
    show_version: bool = typer.Option(False, "--version", help="Show version and exit"),
) -> None:
    """Run before every command."""
    if show_version:
        typer.echo(f"tileharvester v{__version__}")
        raise typer.Exit()
    migrate()


def _extract_code(raw: str) -> str:
    """Extract authorization code from a raw string or full callback URL."""
    raw = raw.strip()
    # If it's a URL, extract the ?code=... parameter
    if raw.startswith("http://") or raw.startswith("https://"):
        parsed = urlparse(raw)
        params = parse_qs(parsed.query)
        codes = params.get("code", [])
        if codes:
            return codes[0]
    # Otherwise return as-is (assume it's already just the code)
    return raw


@app.command()
def auth(
    code: str = typer.Option(None, help="Authorization code or full callback URL from Strava"),
    open_browser: bool = typer.Option(True, help="Open browser for auth"),
) -> None:
    """Authenticate with Strava."""
    if is_authenticated() and not code:
        typer.echo(
            "Already authenticated. Use auth --code <new-code> to re-authenticate if needed."
        )
        return

    if not code:
        try:
            url = build_auth_url()
        except (Exception, BaseException) as e:
            typer.echo(f"Authentication failed: {classify_strava_error(e)}")
            raise typer.Exit(1) from e

        typer.echo(f"Open this URL in your browser:\n{url}")
        if open_browser:
            webbrowser.open(url)
        raw = typer.prompt("Paste the callback URL or authorization code")
        code = _extract_code(raw)

    # Also handle case where user passed --code with a full URL
    code = _extract_code(code)

    if not code:
        typer.echo("Error: No authorization code found.")
        raise typer.Exit(1)

    try:
        tokens = exchange_code(code)
        typer.echo(f"Authenticated successfully. Athlete ID: {tokens.get('athlete', {}).get('id')}")
    except (Exception, BaseException) as e:
        typer.echo(f"Authentication failed: {classify_strava_error(e)}")
        raise typer.Exit(1) from e


@app.command()
def backfill(
    limit: int = typer.Option(None, help="Limit number of activities to fetch"),
) -> None:
    """One-time backfill of historical activities into local DB."""
    if not is_authenticated():
        typer.echo("Not authenticated. Run 'tileharvester auth' first.")
        raise typer.Exit(1)

    typer.echo("Starting backfill...")
    try:
        result = run_backfill(limit=limit)
        typer.echo(f"Backfill complete: {result['stored']} stored, {result['processed']} processed")
    except (Exception, BaseException) as e:
        typer.echo(f"Backfill failed: {classify_strava_error(e)}")
        raise typer.Exit(1) from e


@app.command()
def sync(
    once: bool = typer.Option(False, "--once", help="Run one sync cycle and exit"),
    emoji: str = typer.Option(None, "--emoji", help="Emoji to use in annotation (default: 🗺️)"),
    offset: int = typer.Option(None, "--offset", help="Offset to add to squadrat totals"),
) -> None:
    """Sync new activities and annotate them."""
    if not is_authenticated():
        typer.echo("Not authenticated. Run 'tileharvester auth' first.")
        raise typer.Exit(1)

    if emoji:
        settings.description_emoji = emoji
    if offset is not None:
        settings.squadrat_offset = offset

    if once:
        typer.echo("Running sync...")
        try:
            result = sync_once()
            typer.echo(
                f"Sync complete: {result['new_activities']} new, {result['processed']} processed, {result['annotated']} annotated"
            )
        except (Exception, BaseException) as e:
            typer.echo(f"Sync failed: {classify_strava_error(e)}")
            raise typer.Exit(1) from e
    else:
        typer.echo("Starting continuous sync loop (press Ctrl+C to stop)...")
        try:
            while True:
                try:
                    result = sync_once()
                    if result["annotated"] > 0:
                        typer.echo(f"Annotated {result['annotated']} activities")
                except Exception as e:
                    typer.echo(f"Sync cycle failed: {classify_strava_error(e)}")
                    typer.echo("Waiting before retry...")
                time.sleep(settings.poll_interval_minutes * 60)
        except KeyboardInterrupt:
            typer.echo("\nStopped.")


@app.command()
def retry() -> None:
    """Retry failed activities."""
    if not is_authenticated():
        typer.echo("Not authenticated. Run 'tileharvester auth' first.")
        raise typer.Exit(1)

    typer.echo("Retrying failed activities...")
    try:
        result = retry_failed()
        typer.echo(f"Retried {result['retried']}, succeeded {result['success']}")
    except (Exception, BaseException) as e:
        typer.echo(f"Retry failed: {classify_strava_error(e)}")
        raise typer.Exit(1) from e


@app.command()
def refine(
    limit: int | None = typer.Option(
        None,
        help="Max activities to refine this run; defaults to TH_REFINE_DEFAULT_LIMIT; use 0 for all",
    ),
    force: bool = typer.Option(
        False, "--force", help="Refine activities even if already stream-refined"
    ),
) -> None:
    """Refine historical activity tiles using full Strava GPS streams."""
    if not is_authenticated():
        typer.echo("Not authenticated. Run 'tileharvester auth' first.")
        raise typer.Exit(1)

    try:
        result = refine_streams(limit=limit, force=force)
        typer.echo(
            f"Refine complete: {result['refined']}/{result['selected']} refined, "
            f"{result['failed']} failed, {result['splits']} GPS gaps split, {result['rebuilt']} rebuilt"
        )
    except (Exception, BaseException) as e:
        typer.echo(f"Refine failed: {classify_strava_error(e)}")
        raise typer.Exit(1) from e


@app.command()
def status() -> None:
    """Show current status and stats."""
    with get_db() as conn:
        total = conn.execute("SELECT COUNT(*) FROM activities").fetchone()[0]
        processed = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE status = 'processed'"
        ).fetchone()[0]
        pending = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE status = 'pending'"
        ).fetchone()[0]
        skipped_no_gps = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE status = 'skipped_no_gps'"
        ).fetchone()[0]
        skipped_ignored = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE status = 'skipped_ignored_sport'"
        ).fetchone()[0]
        failed = conn.execute("SELECT COUNT(*) FROM activities WHERE status = 'failed'").fetchone()[
            0
        ]
        stream_refined = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE status = 'processed' AND tile_source = 'streams_clean'"
        ).fetchone()[0]
        needs_refinement = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE status = 'processed' AND COALESCE(tile_source, '') != 'streams_clean'"
        ).fetchone()[0]
        annotated = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE annotation_status = 'done'"
        ).fetchone()[0]
        annotation_failed = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE annotation_status = 'failed'"
        ).fetchone()[0]
        unannotated_processed = conn.execute(
            """
            SELECT COUNT(*) FROM activities
            WHERE status = 'processed'
              AND (annotation_status IS NULL OR annotation_status = 'none')
            """
        ).fetchone()[0]

        total_squadrats = conn.execute(
            "SELECT COUNT(*) FROM global_tiles WHERE tile_kind = 'squadrat'"
        ).fetchone()[0]
        total_squadratinhos = conn.execute(
            "SELECT COUNT(*) FROM global_tiles WHERE tile_kind = 'squadratinho'"
        ).fetchone()[0]

    typer.echo("TileHarvester Status")
    typer.echo("=" * 40)
    typer.echo(f"Database:                  {settings.db_path}")
    typer.echo(f"Total activities in DB:    {total}")
    typer.echo(f"Processed:                 {processed}")
    typer.echo(f"Pending:                   {pending}")
    typer.echo(f"Skipped no GPS:            {skipped_no_gps}")
    typer.echo(f"Ignored sports:            {skipped_ignored}")
    typer.echo(f"Failed:                    {failed}")
    typer.echo(f"Stream-refined:            {stream_refined}")
    typer.echo(f"Needs stream refinement:   {needs_refinement}")
    typer.echo(f"Annotated:                 {annotated}")
    typer.echo(f"Annotation failed:         {annotation_failed}")
    typer.echo(f"Unannotated processed:     {unannotated_processed}")
    typer.echo(f"Total unique Squadrats:    {total_squadrats}")
    typer.echo(f"Total unique Squadratinhos: {total_squadratinhos}")

    if not is_authenticated():
        typer.echo("\nWarning: Not authenticated with Strava.")


@app.command()
def stats() -> None:
    """Show detailed period stats."""
    with get_db() as conn:
        # This week
        week_start = datetime.now().replace(hour=0, minute=0, second=0, microsecond=0)
        while week_start.weekday() != 0:
            week_start = week_start.replace(day=week_start.day - 1)

        week_new = conn.execute(
            "SELECT COALESCE(SUM(new_squadrat_count), 0) FROM activities WHERE status = 'processed' AND start_local >= ?",
            (week_start.isoformat(),),
        ).fetchone()[0]

        # This month
        month_start = datetime.now().replace(day=1, hour=0, minute=0, second=0, microsecond=0)
        month_new = conn.execute(
            "SELECT COALESCE(SUM(new_squadrat_count), 0) FROM activities WHERE status = 'processed' AND start_local >= ?",
            (month_start.isoformat(),),
        ).fetchone()[0]

        total_new = conn.execute(
            "SELECT COUNT(*) FROM global_tiles WHERE tile_kind = 'squadrat'"
        ).fetchone()[0]

    typer.echo("TileHarvester Stats")
    typer.echo("=" * 40)
    typer.echo(f"New Squadrats this week:  {week_new}")
    typer.echo(f"New Squadrats this month: {month_new}")
    typer.echo(f"Total unique Squadrats:   {total_new}")


@app.command()
def recompute() -> None:
    """Recompute global tile novelty from stored activity tiles."""
    typer.echo("Recomputing global tiles...")
    result = recompute_all()
    typer.echo(f"Rebuilt {result['rebuilt']} activities, {result.get('preserved', 0)} preserved")


@app.command()
def recompute_novelty() -> None:
    """Rebuild global totals from stored tiles without re-fetching anything. Safe to run anytime."""
    typer.echo("Rebuilding global totals from stored tiles...")
    result = recompute_novelty_from_stored_tiles()
    typer.echo(f"Rebuilt {result['rebuilt']} activities")


@app.command()
def validate(
    activity_id: int = typer.Argument(..., help="Strava activity ID to validate"),
) -> None:
    """Compute total and historical new tiles for an activity."""
    if not is_authenticated():
        typer.echo("Not authenticated. Run 'tileharvester auth' first.")
        raise typer.Exit(1)

    with get_db() as conn:
        row = conn.execute("SELECT * FROM activities WHERE id = ?", (activity_id,)).fetchone()

    start_local = row["start_local"] if row else None
    if start_local is None:
        typer.echo(f"Fetching activity {activity_id} metadata...")
        try:
            activity = get_activity(activity_id)
            start_local = activity.get("start_date_local")
        except Exception as e:
            typer.echo(f"Failed: {classify_strava_error(e)}")
            raise typer.Exit(1) from e

    typer.echo(f"Fetching activity {activity_id} GPS stream...")
    try:
        streams = get_activity_streams(activity_id, keys="latlng,time")
    except (Exception, BaseException) as e:
        typer.echo(f"Failed: {classify_strava_error(e)}")
        raise typer.Exit(1) from e

    segments, stream_stats = clean_stream_segments(streams)

    engine = make_engine()
    squadrats, squadratinhos = engine.tiles_for_segments(segments)

    typer.echo(f"Activity {activity_id}:")
    if start_local:
        typer.echo(f"  Start local:     {start_local}")
    typer.echo(f"  GPS points:      {stream_stats['points']}")
    typer.echo(
        f"  Track segments:  {stream_stats['segments']} ({stream_stats['splits']} GPS gaps split)"
    )
    typer.echo(f"  Total Squadrats (this activity):     {len(squadrats)}")
    typer.echo(f"  Total Squadratinhos (this activity): {len(squadratinhos)}")
    typer.echo(
        f"  Ratio (inho:squadrat):               {len(squadratinhos) / max(len(squadrats), 1):.1f}"
    )

    novelty = None
    if start_local:
        novelty = compute_historical_novelty(activity_id, start_local, squadrats, squadratinhos)
        typer.echo("")
        typer.echo("  Historical comparison from local DB:")
        typer.echo(f"    Processed activities before:       {novelty['processed_before']}")
        typer.echo(f"    Unique Squadrats before:           {novelty['total_squadrats_before']}")
        typer.echo(f"    Already-seen Squadrats on route:   {novelty['seen_squadrats']}")
        typer.echo(f"    New Squadrats from this activity:  {novelty['new_squadrats']}")
        typer.echo(f"    Unique Squadrats after:            {novelty['total_squadrats_after']}")
        typer.echo(
            f"    Unique Squadratinhos before:       {novelty['total_squadratinhos_before']}"
        )
        typer.echo(f"    Already-seen Squadratinhos route:  {novelty['seen_squadratinhos']}")
        typer.echo(f"    New Squadratinhos from activity:   {novelty['new_squadratinhos']}")
        typer.echo(f"    Unique Squadratinhos after:        {novelty['total_squadratinhos_after']}")
        typer.echo("")
        typer.echo(
            "  NOTE: Historical comparison only reflects activities already processed in this local DB."
        )
    else:
        typer.echo("")
        typer.echo(
            "  Could not determine start date, so historical new-tile comparison was skipped."
        )

    if row:
        typer.echo("")
        typer.echo("  Stored in local DB:")
        typer.echo(f"    Status:                {row['status']}")
        typer.echo(f"    Stored Squadrats:      {row['squadrat_count']}")
        typer.echo(f"    Stored Squadratinhos:  {row['squadratinho_count']}")
        typer.echo(f"    Stored new Squadrats:  {row['new_squadrat_count']}")
        typer.echo(f"    Stored new Squadratinhos: {row['new_squadratinho_count']}")
        if novelty:
            squadrat_match = (
                "yes" if row["new_squadrat_count"] == novelty["new_squadrats"] else "no"
            )
            squadratinho_match = (
                "yes" if row["new_squadratinho_count"] == novelty["new_squadratinhos"] else "no"
            )
            typer.echo(f"    Recomputed new Squadrats match stored: {squadrat_match}")
            typer.echo(f"    Recomputed new Squadratinhos match stored: {squadratinho_match}")


@app.command()
def health(
    check_rate_limit: bool = typer.Option(
        True, help="Check Strava API rate limit status (consumes one API call)"
    ),
) -> None:
    """Check system health: DB, Strava auth, and rate limits."""
    import sqlite3

    issues: list[str] = []
    typer.echo("TileHarvester Health Check")
    typer.echo("=" * 40)

    # DB check
    try:
        with get_db() as conn:
            conn.execute("SELECT 1").fetchone()
        typer.echo("Database:                  OK")
    except sqlite3.Error as e:
        issues.append(f"Database: {e}")
        typer.echo(f"Database:                  FAILED - {e}")

    # DB path
    typer.echo(f"DB location:               {settings.db_path}")
    typer.echo(f"Data directory:            {settings.data_dir}")

    # Strava auth check
    if is_authenticated():
        typer.echo("Strava auth:               Authenticated")
    else:
        issues.append("Strava auth: not authenticated")
        typer.echo("Strava auth:               NOT AUTHENTICATED (run 'tileharvester auth')")

    # Rate limit check
    if check_rate_limit and is_authenticated():
        typer.echo("Checking Strava rate limits...")
        rate_status = get_rate_limit_status()
        if rate_status.get("ok"):
            daily_limit = rate_status.get("daily_limit", "?")
            daily_used = rate_status.get("daily_used", "?")
            fifteen_min_limit = rate_status.get("fifteen_min_limit", "")
            fifteen_min_used = rate_status.get("fifteen_min_used", "")
            typer.echo(f"  Daily usage:             {daily_used}/{daily_limit}")
            if fifteen_min_limit and fifteen_min_used:
                typer.echo(f"  15-min usage:            {fifteen_min_used}/{fifteen_min_limit}")
        else:
            err = rate_status.get("error", "Unknown error")
            issues.append(f"Rate limit check: {err}")
            typer.echo(f"  Rate limit check:        FAILED - {err}")

    # Summary
    typer.echo("")
    if issues:
        typer.echo(f"Issues found: {len(issues)}")
        for issue in issues:
            typer.echo(f"  - {issue}")
        raise typer.Exit(1)
    else:
        typer.echo("All checks passed.")


@app.command()
def service(
    action: str = typer.Argument("print", help="Action: print, install"),
    interval: int = typer.Option(5, help="Timer interval in minutes"),
    python: str = typer.Option("/usr/bin/python3", help="Python executable path"),
) -> None:
    """Generate or install systemd service/timer files."""
    if action == "print":
        print_service(python=python)
    elif action == "install":
        try:
            install_service(python=python, interval=interval)
        except (Exception, BaseException) as e:
            typer.echo(f"Install failed (may need sudo): {e}")
            raise typer.Exit(1) from e
    else:
        typer.echo(f"Unknown action: {action}")
        raise typer.Exit(1)


@app.command()
def reset_db(
    force: bool = typer.Option(False, "--force", help="Confirm destructive reset"),
) -> None:
    """Reset the local database. DESTRUCTIVE."""
    if not force:
        typer.echo("This will delete all local data. Use --force to confirm.")
        raise typer.Exit(1)
    reset()
    typer.echo("Database reset.")


def main() -> None:
    app()
