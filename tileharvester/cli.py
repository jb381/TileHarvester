"""CLI entry point using Typer."""
import re
import webbrowser
from datetime import datetime
from urllib.parse import parse_qs, urlparse

import typer

from tileharvester.config import settings
from tileharvester.db import migrate, reset, get_db
from tileharvester.strava_client import (
    build_auth_url,
    exchange_code,
    get_athlete,
    is_authenticated,
)
from tileharvester.sync import (
    backfill as sync_backfill,
    clean_stream_segments,
    compute_historical_novelty,
    refine_streams,
    recompute_all,
    retry_failed,
    sync_once,
)
from tileharvester.systemd import print_service, install_service
from tileharvester.tile_engine import make_engine

app = typer.Typer(help="TileHarvester - Automatically add Squadrats stats to Strava activities")


@app.callback()
def callback():
    """Run before every command."""
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
):
    """Authenticate with Strava."""
    if not settings.strava_client_id or not settings.strava_client_secret:
        typer.echo("Error: Set TH_STRAVA_CLIENT_ID and TH_STRAVA_CLIENT_SECRET")
        raise typer.Exit(1)

    if is_authenticated():
        typer.echo("Already authenticated. Use auth --code <new-code> to re-authenticate if needed.")
        return

    url = build_auth_url()

    if not code:
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
    except Exception as e:
        typer.echo(f"Authentication failed: {e}")
        raise typer.Exit(1)


@app.command()
def backfill(
    limit: int = typer.Option(None, help="Limit number of activities to fetch"),
):
    """One-time backfill of historical activities into local DB."""
    if not is_authenticated():
        typer.echo("Not authenticated. Run 'tileharvester auth' first.")
        raise typer.Exit(1)

    typer.echo("Starting backfill...")
    result = sync_backfill(limit=limit)
    typer.echo(f"Backfill complete: {result['stored']} stored, {result['processed']} processed")


@app.command()
def sync(
    once: bool = typer.Option(False, "--once", help="Run one sync cycle and exit"),
    emoji: str = typer.Option(None, "--emoji", help="Emoji to use in annotation (default: 🗺️)"),
    offset: int = typer.Option(None, "--offset", help="Offset to add to squadrat totals"),
):
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
        result = sync_once()
        typer.echo(
            f"Sync complete: {result['new_activities']} new, {result['processed']} processed, {result['annotated']} annotated"
        )
    else:
        import time

        typer.echo("Starting continuous sync loop (press Ctrl+C to stop)...")
        try:
            while True:
                result = sync_once()
                if result["annotated"] > 0:
                    typer.echo(f"Annotated {result['annotated']} activities")
                time.sleep(settings.poll_interval_minutes * 60)
        except KeyboardInterrupt:
            typer.echo("\nStopped.")


@app.command()
def retry():
    """Retry failed activities."""
    if not is_authenticated():
        typer.echo("Not authenticated. Run 'tileharvester auth' first.")
        raise typer.Exit(1)

    typer.echo("Retrying failed activities...")
    result = retry_failed()
    typer.echo(f"Retried {result['retried']}, succeeded {result['success']}")


@app.command()
def refine(
    limit: int = typer.Option(80, help="Max activities to refine this run; use 0 for all"),
    force: bool = typer.Option(False, "--force", help="Refine activities even if already stream-refined"),
):
    """Refine historical activity tiles using full Strava GPS streams."""
    if not is_authenticated():
        typer.echo("Not authenticated. Run 'tileharvester auth' first.")
        raise typer.Exit(1)

    result = refine_streams(limit=limit, force=force)
    typer.echo(
        f"Refine complete: {result['refined']}/{result['selected']} refined, "
        f"{result['failed']} failed, {result['splits']} GPS gaps split, {result['rebuilt']} rebuilt"
    )


@app.command()
def status():
    """Show current status and stats."""
    with get_db() as conn:
        total = conn.execute("SELECT COUNT(*) FROM activities").fetchone()[0]
        processed = conn.execute("SELECT COUNT(*) FROM activities WHERE status = 'processed'").fetchone()[0]
        pending = conn.execute("SELECT COUNT(*) FROM activities WHERE status = 'pending'").fetchone()[0]
        skipped_no_gps = conn.execute("SELECT COUNT(*) FROM activities WHERE status = 'skipped_no_gps'").fetchone()[0]
        skipped_ignored = conn.execute("SELECT COUNT(*) FROM activities WHERE status = 'skipped_ignored_sport'").fetchone()[0]
        failed = conn.execute("SELECT COUNT(*) FROM activities WHERE status = 'failed'").fetchone()[0]
        stream_refined = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE status = 'processed' AND tile_source = 'streams_clean'"
        ).fetchone()[0]
        needs_refinement = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE status = 'processed' AND COALESCE(tile_source, '') != 'streams_clean'"
        ).fetchone()[0]
        annotated = conn.execute("SELECT COUNT(*) FROM activities WHERE annotation_status = 'done'").fetchone()[0]
        annotation_failed = conn.execute(
            "SELECT COUNT(*) FROM activities WHERE annotation_status = 'failed'"
        ).fetchone()[0]
        stale = conn.execute(
            """
            SELECT COUNT(*) FROM activities
            WHERE annotation_status = 'done'
              AND processed_at IS NOT NULL
              AND id IN (
                  SELECT id FROM activities
                  WHERE start_local < (SELECT MAX(start_local) FROM activities WHERE status = 'processed')
              )
              AND new_squadrat_count != (
                  SELECT new_squadrat_count FROM activities a2 WHERE a2.id = activities.id
              )
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
    typer.echo(f"Stale annotations:         {stale}")
    typer.echo(f"Total unique Squadrats:    {total_squadrats}")
    typer.echo(f"Total unique Squadratinhos: {total_squadratinhos}")

    if not is_authenticated():
        typer.echo("\nWarning: Not authenticated with Strava.")


@app.command()
def stats():
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
def recompute():
    """Recompute global tile novelty from stored activity tiles."""
    typer.echo("Recomputing global tiles...")
    result = recompute_all()
    typer.echo(f"Rebuilt {result['rebuilt']} activities")


@app.command()
def validate(
    activity_id: int = typer.Argument(..., help="Strava activity ID to validate"),
):
    """Compute total and historical new tiles for an activity."""
    if not is_authenticated():
        typer.echo("Not authenticated. Run 'tileharvester auth' first.")
        raise typer.Exit(1)

    from tileharvester.strava_client import get_activity, get_activity_streams
    from tileharvester.tile_engine import make_engine

    with get_db() as conn:
        row = conn.execute("SELECT * FROM activities WHERE id = ?", (activity_id,)).fetchone()

    start_local = row["start_local"] if row else None
    if start_local is None:
        typer.echo(f"Fetching activity {activity_id} metadata...")
        try:
            activity = get_activity(activity_id)
            start_local = activity.get("start_date_local")
        except Exception as e:
            typer.echo(f"Failed to fetch metadata: {e}")

    typer.echo(f"Fetching activity {activity_id} GPS stream...")
    try:
        streams = get_activity_streams(activity_id, keys="latlng,time")
    except Exception as e:
        typer.echo(f"Failed to fetch: {e}")
        raise typer.Exit(1)

    segments, stream_stats = clean_stream_segments(streams)

    engine = make_engine()
    squadrats, squadratinhos = engine.tiles_for_segments(segments)

    typer.echo(f"Activity {activity_id}:")
    if start_local:
        typer.echo(f"  Start local:     {start_local}")
    typer.echo(f"  GPS points:      {stream_stats['points']}")
    typer.echo(f"  Track segments:  {stream_stats['segments']} ({stream_stats['splits']} GPS gaps split)")
    typer.echo(f"  Total Squadrats (this activity):     {len(squadrats)}")
    typer.echo(f"  Total Squadratinhos (this activity): {len(squadratinhos)}")
    typer.echo(f"  Ratio (inho:squadrat):               {len(squadratinhos)/max(len(squadrats),1):.1f}")

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
        typer.echo(f"    Unique Squadratinhos before:       {novelty['total_squadratinhos_before']}")
        typer.echo(f"    Already-seen Squadratinhos route:  {novelty['seen_squadratinhos']}")
        typer.echo(f"    New Squadratinhos from activity:   {novelty['new_squadratinhos']}")
        typer.echo(f"    Unique Squadratinhos after:        {novelty['total_squadratinhos_after']}")
        typer.echo("")
        typer.echo("  NOTE: Historical comparison only reflects activities already processed in this local DB.")
    else:
        typer.echo("")
        typer.echo("  Could not determine start date, so historical new-tile comparison was skipped.")

    if row:
        typer.echo("")
        typer.echo("  Stored in local DB:")
        typer.echo(f"    Status:                {row['status']}")
        typer.echo(f"    Stored Squadrats:      {row['squadrat_count']}")
        typer.echo(f"    Stored Squadratinhos:  {row['squadratinho_count']}")
        typer.echo(f"    Stored new Squadrats:  {row['new_squadrat_count']}")
        typer.echo(f"    Stored new Squadratinhos: {row['new_squadratinho_count']}")
        if novelty:
            squadrat_match = "yes" if row["new_squadrat_count"] == novelty["new_squadrats"] else "no"
            squadratinho_match = "yes" if row["new_squadratinho_count"] == novelty["new_squadratinhos"] else "no"
            typer.echo(f"    Recomputed new Squadrats match stored: {squadrat_match}")
            typer.echo(f"    Recomputed new Squadratinhos match stored: {squadratinho_match}")


@app.command()
def service(
    action: str = typer.Argument("print", help="Action: print, install"),
    interval: int = typer.Option(5, help="Timer interval in minutes"),
    python: str = typer.Option("/usr/bin/python3", help="Python executable path"),
):
    """Generate or install systemd service/timer files."""
    if action == "print":
        print_service(python=python)
    elif action == "install":
        try:
            install_service(python=python, interval=interval)
        except Exception as e:
            typer.echo(f"Install failed (may need sudo): {e}")
            raise typer.Exit(1)
    else:
        typer.echo(f"Unknown action: {action}")
        raise typer.Exit(1)


@app.command()
def reset_db(force: bool = typer.Option(False, "--force", help="Confirm destructive reset")):
    """Reset the local database. DESTRUCTIVE."""
    if not force:
        typer.echo("This will delete all local data. Use --force to confirm.")
        raise typer.Exit(1)
    reset()
    typer.echo("Database reset.")


def main():
    app()
