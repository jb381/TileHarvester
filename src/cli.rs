//! Command-line parsing and dispatch.

use std::env;
use std::ffi::OsString;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{Datelike, Days, Local, NaiveDate, Utc};
use clap::{Parser, Subcommand};
use rusqlite::{Connection, OpenFlags};

use crate::annotate::format_thousands;
use crate::backfill::backfill_with_conn;
use crate::config::{save_env_values, Settings};
use crate::db;
use crate::error::{Error, Result};
use crate::gpx::parse_gpx_file;
use crate::kml_baseline::{
    compare_kml_detailed_with_conn, effective_tile_count_with_conn, effective_tiles, get_baseline,
    import_baseline,
};
use crate::models::TileKind;
use crate::recompute::{recompute_all_with_conn, recompute_novelty_from_stored_tiles_with_conn};
use crate::refine::{refine_streams_with_conn, unrefined_count_with_conn};
use crate::squadrats_stats::{compute_grid_metrics, parse_tile_coord, tile_center_lat_lon};
use crate::strava_client::{classify_strava_error, HttpStravaClient, StravaApi};
use crate::sync::{
    clean_stream_segments, compute_historical_novelty_with_conn, needs_initial_backfill,
    retry_failed_with_conn, sync_once_with_conn, STREAM_KEYS,
};
use crate::systemd::{install_service_with_options, write_service_with_options, ServiceOptions};
use crate::tile_engine::{make_engine, TileEngine};

const VERSION: &str = concat!("tileharvester v", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Parser)]
#[command(
    name = "tileharvester",
    about = "TileHarvester - Automatically add Squadrats stats to Strava activities",
    arg_required_else_help = true
)]
pub struct Cli {
    /// Show version and exit
    #[arg(long)]
    version: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Interactive first-time setup (credentials, Strava login, history & optional service).
    Setup {
        /// Strava API Client ID
        #[arg(long)]
        client_id: Option<String>,
        /// Strava API Client Secret
        #[arg(long)]
        client_secret: Option<String>,
        /// Optional Squadrats KML export to seed an exact baseline
        #[arg(long, value_name = "PATH")]
        kml: Option<PathBuf>,
        /// UTC snapshot timestamp for --kml; defaults to now
        #[arg(long, value_name = "ISO8601")]
        as_of: Option<String>,
        /// Open browser for auth (default)
        #[arg(long, overrides_with = "no_open_browser")]
        open_browser: bool,
        /// Skip opening the browser for auth
        #[arg(long, overrides_with = "open_browser")]
        no_open_browser: bool,
        /// Listen on the local redirect URI to capture the OAuth callback automatically
        #[arg(long, overrides_with = "no_listen")]
        listen: bool,
        /// Do not bind a local OAuth callback listener; prompt to paste the callback URL
        #[arg(long, overrides_with = "listen")]
        no_listen: bool,
        /// Skip immediate backfill during setup (sync will backfill automatically on first run)
        #[arg(long)]
        no_backfill: bool,
        /// Install a rootless systemd user timer without prompting
        #[arg(long, overrides_with = "no_service")]
        install_service: bool,
        /// Skip systemd timer prompt
        #[arg(long, overrides_with = "install_service")]
        no_service: bool,
    },
    /// Authenticate with Strava.
    Auth {
        /// Authorization code or full callback URL from Strava
        #[arg(long)]
        code: Option<String>,
        /// Open browser for auth (default)
        #[arg(long, overrides_with = "no_open_browser")]
        open_browser: bool,
        /// Skip opening the browser for auth
        #[arg(long, overrides_with = "open_browser")]
        no_open_browser: bool,
        /// Listen on the local redirect URI to capture the OAuth callback automatically
        #[arg(long, overrides_with = "no_listen")]
        listen: bool,
        /// Do not bind a local OAuth callback listener; prompt to paste the callback URL
        #[arg(long, overrides_with = "listen")]
        no_listen: bool,
    },
    /// One-time backfill of historical activities into local DB.
    Backfill {
        #[arg(long, value_name = "N", help = "Limit number of activities to fetch")]
        limit: Option<usize>,
    },
    /// Sync new activities and annotate them.
    Sync {
        #[arg(long, help = "Run one sync cycle and exit")]
        once: bool,
        #[arg(long, help = "Emoji to use in annotation")]
        emoji: Option<String>,
        #[arg(
            long,
            allow_hyphen_values = true,
            help = "Offset to add to squadrat totals"
        )]
        offset: Option<i64>,
    },
    /// Retry failed activities.
    Retry,
    /// Refine historical activity tiles using full Strava GPS streams.
    Refine {
        #[arg(
            long,
            value_name = "N",
            help = "Defaults to TH_REFINE_DEFAULT_LIMIT; use 0 for all"
        )]
        limit: Option<usize>,
        #[arg(long, help = "Refine activities even if already stream-refined")]
        force: bool,
    },
    /// Show current status and stats.
    Status,
    /// Show detailed period stats.
    Stats,
    /// Install an immutable Squadrats KML baseline.
    ImportKml {
        path: PathBuf,
        #[arg(
            long,
            value_name = "ISO8601",
            help = "UTC snapshot timestamp; defaults to now"
        )]
        as_of: Option<String>,
    },
    /// Compare a Squadrats KML export with effective local tiles.
    CompareKml {
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        details: usize,
    },
    /// Inspect a GPX track and compare it with effective local tiles.
    InspectGpx { path: PathBuf },
    /// Recompute global tile novelty from stored activity tiles.
    Recompute,
    /// Rebuild global totals from stored tiles without fetching activities.
    RecomputeNovelty,
    /// Compute total and historical new tiles for an activity.
    Validate { activity_id: i64 },
    /// Check system health: DB, Strava auth, and rate limits.
    Health {
        /// Check Strava API rate limits (default, consumes one API call)
        #[arg(long, overrides_with = "no_check_rate_limit")]
        check_rate_limit: bool,
        /// Skip the Strava API rate limit check
        #[arg(long, overrides_with = "check_rate_limit")]
        no_check_rate_limit: bool,
    },
    /// Generate or install systemd service/timer files.
    Service {
        #[arg(default_value = "print", help = "Action: print, install")]
        action: String,
        #[arg(
            long,
            default_value_t = 5,
            value_name = "N",
            help = "Timer interval in minutes"
        )]
        interval: u64,
        #[arg(long, alias = "python", value_name = "PATH")]
        binary: Option<String>,
        #[arg(long, value_name = "PATH")]
        env_file: Option<String>,
        #[arg(
            long,
            help = "Generate/install a rootless systemd user timer (~/.config/systemd/user)"
        )]
        user: bool,
        #[arg(
            long,
            value_name = "USER",
            help = "Run system service as this user (system mode only)"
        )]
        run_as: Option<String>,
    },
    /// Write a consistent SQLite backup.
    Backup {
        path: PathBuf,
        #[arg(long, help = "Overwrite existing backup file")]
        force: bool,
    },
    /// Reset the local database. DESTRUCTIVE.
    ResetDb {
        #[arg(long, help = "Confirm destructive reset")]
        force: bool,
    },
}

pub fn parse_from<I, T>(itr: I) -> std::result::Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    Cli::try_parse_from(itr)
}

pub fn run() -> i32 {
    let cli = match parse_from(std::env::args_os()) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return error.exit_code();
        }
    };
    if cli.version {
        println!("{VERSION}");
        return 0;
    }

    let stdin_is_tty = io::stdin().is_terminal();
    let result = Settings::load().and_then(|mut settings| {
        let mut stdin = io::stdin().lock();
        let mut stdout = io::stdout().lock();
        let mut credentials_checked = false;
        if let Some(Command::Setup {
            ref client_id,
            ref client_secret,
            ..
        }) = cli.command
        {
            ensure_credentials(
                &mut settings,
                client_id.as_deref(),
                client_secret.as_deref(),
                &mut stdin,
                &mut stdout,
            )?;
            credentials_checked = true;
        }
        let api = HttpStravaClient::new(settings.clone())?;
        Ok(run_cli_with_context(
            cli,
            settings,
            &api,
            stdin_is_tty,
            credentials_checked,
            &mut stdin,
            &mut stdout,
        ))
    });
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

pub fn run_cli_with<R: BufRead, W: Write>(
    cli: Cli,
    settings: Settings,
    api: &dyn StravaApi,
    input: &mut R,
    output: &mut W,
) -> i32 {
    run_cli_with_context(cli, settings, api, false, false, input, output)
}

fn run_cli_with_context<R: BufRead, W: Write>(
    cli: Cli,
    settings: Settings,
    api: &dyn StravaApi,
    stdin_is_tty: bool,
    credentials_checked: bool,
    input: &mut R,
    output: &mut W,
) -> i32 {
    let result = if cli.version {
        writeln!(output, "{VERSION}")
            .map(|()| 0)
            .map_err(Error::from)
    } else if let Some(Command::InspectGpx { path }) = cli.command {
        with_context(
            inspect_gpx(&settings, &path, output),
            "GPX inspection failed",
        )
        .map(|()| 0)
    } else if let Some(command) = cli.command {
        db::get_db(&settings).and_then(|mut conn| {
            db::migrate_with_conn(&mut conn)?;
            dispatch(
                command,
                settings,
                &mut conn,
                api,
                stdin_is_tty,
                credentials_checked,
                input,
                output,
            )
        })
    } else {
        Ok(0)
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(output, "{error}");
            1
        }
    }
}

fn with_context<T>(result: Result<T>, context: &str) -> Result<T> {
    result.map_err(|error| Error::Other(format!("{context}: {error}")))
}

fn strava_context<T>(result: Result<T>, context: &str) -> Result<T> {
    result.map_err(|error| Error::Other(format!("{context}: {}", classify_strava_error(&error))))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListenMode {
    Auto,
    Force,
    Disabled,
}

impl ListenMode {
    fn from_flags(listen: bool, no_listen: bool) -> Self {
        if listen {
            Self::Force
        } else if no_listen {
            Self::Disabled
        } else {
            Self::Auto
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    command: Command,
    mut settings: Settings,
    conn: &mut rusqlite::Connection,
    api: &dyn StravaApi,
    stdin_is_tty: bool,
    credentials_checked: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<i32> {
    if matches!(
        command,
        Command::Backfill { .. }
            | Command::Sync { .. }
            | Command::Retry
            | Command::Refine { .. }
            | Command::Validate { .. }
    ) && !api.is_authenticated()
    {
        return Err(Error::NotAuthenticated);
    }

    match command {
        Command::Setup {
            client_id,
            client_secret,
            kml,
            as_of,
            open_browser,
            no_open_browser,
            listen,
            no_listen,
            no_backfill,
            install_service,
            no_service,
        } => {
            let options = SetupOptions {
                client_id,
                client_secret,
                kml,
                as_of,
                open_browser: open_browser || !no_open_browser,
                listen_mode: ListenMode::from_flags(listen, no_listen),
                no_backfill,
                install_service,
                no_service,
                stdin_is_tty,
                credentials_checked,
            };
            setup(&mut settings, conn, api, options, input, output)?;
        }
        Command::Auth {
            code,
            open_browser,
            no_open_browser,
            listen,
            no_listen,
        } => {
            auth(
                api,
                code,
                open_browser || !no_open_browser,
                ListenMode::from_flags(listen, no_listen),
                stdin_is_tty,
                &settings.strava_redirect_uri,
                input,
                output,
            )?;
        }
        Command::Backfill { limit } => {
            writeln!(output, "Starting backfill...")?;
            let result = strava_context(
                backfill_with_conn(&settings, conn, api, limit),
                "Backfill failed",
            )?;
            writeln!(
                output,
                "Backfill complete: {} stored, {} processed",
                result.stored, result.processed
            )?;
        }
        Command::Sync {
            once,
            emoji,
            offset,
        } => {
            if let Some(emoji) = emoji.filter(|emoji| !emoji.is_empty()) {
                settings.description_emoji = emoji;
            }
            if let Some(offset) = offset {
                settings.squadrat_offset = offset;
            }
            return sync(&settings, conn, api, once, output);
        }
        Command::Retry => {
            writeln!(output, "Retrying failed activities...")?;
            let result =
                strava_context(retry_failed_with_conn(&settings, conn, api), "Retry failed")?;
            writeln!(
                output,
                "Retried {}, succeeded {}",
                result.retried, result.success
            )?;
        }
        Command::Refine { limit, force } => {
            let result = strava_context(
                refine_streams_with_conn(&settings, conn, api, limit, force),
                "Refine failed",
            )?;
            writeln!(
                output,
                "Refine complete: {}/{} refined, {} failed, {} GPS gaps split, {} rebuilt",
                result.refined, result.selected, result.failed, result.splits, result.rebuilt
            )?;
        }
        Command::Status => status(&settings, conn, api, output)?,
        Command::Stats => stats(&settings, conn, output)?,
        Command::ImportKml { path, as_of } => {
            import_kml(&settings, &path, as_of.as_deref(), output)?
        }
        Command::CompareKml { path, details } => compare(&settings, conn, &path, details, output)?,
        Command::InspectGpx { .. } => unreachable!("GPX inspection bypasses database migration"),
        Command::Recompute => {
            writeln!(output, "Recomputing global tiles...")?;
            let result = recompute_all_with_conn(&settings, conn)?;
            writeln!(
                output,
                "Rebuilt {} activities, {} preserved, {} failed",
                result.rebuilt, result.preserved, result.failed
            )?;
            return Ok(i32::from(result.failed > 0));
        }
        Command::RecomputeNovelty => {
            writeln!(output, "Rebuilding global totals from stored tiles...")?;
            let result = recompute_novelty_from_stored_tiles_with_conn(&settings, conn)?;
            writeln!(output, "Rebuilt {} activities", result.rebuilt)?;
        }
        Command::Validate { activity_id } => validate(&settings, conn, api, activity_id, output)?,
        Command::Health {
            check_rate_limit,
            no_check_rate_limit,
        } => {
            return health(
                &settings,
                conn,
                api,
                check_rate_limit || !no_check_rate_limit,
                output,
            );
        }
        Command::Service {
            action,
            interval,
            binary,
            env_file,
            user,
            run_as,
        } => {
            let options = ServiceOptions { user, run_as };
            let binary = binary.as_deref();
            let env_file = env_file.as_deref();
            match action.as_str() {
                "print" => write_service_with_options(
                    &settings, None, binary, interval, env_file, &options, output,
                )?,
                "install" => with_context(
                    install_service_with_options(
                        &settings, None, binary, interval, env_file, &options, output,
                    ),
                    if user {
                        "Install failed"
                    } else {
                        "Install failed (may need sudo)"
                    },
                )?,
                _ => return Err(Error::Other(format!("Unknown action: {action}"))),
            }
        }
        Command::Backup { path, force } => {
            db::backup_with_conn(&settings, conn, &path, force)?;
            writeln!(output, "Backup written to {}", path.display())?;
        }
        Command::ResetDb { force } => {
            if !force {
                return Err(Error::Other(
                    "This will delete all local data. Use --force to confirm.".into(),
                ));
            }
            db::reset_with_conn(conn)?;
            writeln!(output, "Database reset.")?;
        }
    }
    Ok(0)
}

pub fn extract_code(raw: &str) -> String {
    let raw = raw.trim();
    if raw.starts_with("http://") || raw.starts_with("https://") {
        if let Ok(url) = url::Url::parse(raw) {
            if let Some((_, code)) = url
                .query_pairs()
                .find(|(key, value)| key == "code" && !value.is_empty())
            {
                return code.into_owned();
            }
        }
    }
    raw.to_owned()
}

struct SetupOptions {
    client_id: Option<String>,
    client_secret: Option<String>,
    kml: Option<PathBuf>,
    as_of: Option<String>,
    open_browser: bool,
    listen_mode: ListenMode,
    no_backfill: bool,
    install_service: bool,
    no_service: bool,
    stdin_is_tty: bool,
    credentials_checked: bool,
}

fn prompt_line(prompt: &str, input: &mut impl BufRead, output: &mut impl Write) -> Result<String> {
    write!(output, "{prompt}")?;
    output.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

fn ensure_credentials(
    settings: &mut Settings,
    client_id: Option<&str>,
    client_secret: Option<&str>,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    let mut updated = false;
    if let Some(id) = client_id.map(str::trim).filter(|v| !v.is_empty()) {
        settings.strava_client_id = id.to_owned();
        updated = true;
    }
    if let Some(secret) = client_secret.map(str::trim).filter(|v| !v.is_empty()) {
        settings.strava_client_secret = secret.to_owned();
        updated = true;
    }

    let missing_id = settings.strava_client_id.trim().is_empty();
    let missing_secret = settings.strava_client_secret.trim().is_empty();
    if missing_id || missing_secret {
        writeln!(
            output,
            "Strava API credentials are required.\n\
             Create or open your Strava API app at: https://www.strava.com/settings/api\n\
             Set \"Authorization Callback Domain\" to: localhost"
        )?;
        if missing_id {
            settings.strava_client_id = prompt_line("Strava Client ID: ", input, output)?;
        }
        if missing_secret {
            settings.strava_client_secret = prompt_line("Strava Client Secret: ", input, output)?;
        }
        updated = true;
    }

    settings.validate_strava_credentials()?;
    if updated {
        let env_path = settings.env_path();
        save_env_values(
            &env_path,
            &[
                ("TH_STRAVA_CLIENT_ID", settings.strava_client_id.trim()),
                (
                    "TH_STRAVA_CLIENT_SECRET",
                    settings.strava_client_secret.trim(),
                ),
            ],
        )?;
        writeln!(output, "Saved Strava credentials to {}", env_path.display())?;
    } else {
        writeln!(output, "Strava credentials: already configured.")?;
    }
    Ok(())
}

fn is_container_or_remote() -> bool {
    Path::new("/.dockerenv").exists()
        || env::var_os("container").is_some()
        || env::var_os("SSH_CONNECTION").is_some()
        || env::var_os("SSH_TTY").is_some()
}

fn is_loopback_http_redirect(uri: &str) -> bool {
    url::Url::parse(uri).is_ok_and(|redirect| {
        matches!(
            (redirect.scheme(), redirect.host_str()),
            ("http", Some("localhost" | "127.0.0.1" | "[::1]"))
        )
    })
}

fn can_auto_listen(redirect_uri: &str, stdin_is_tty: bool) -> bool {
    stdin_is_tty && is_loopback_http_redirect(redirect_uri) && !is_container_or_remote()
}

fn setup(
    settings: &mut Settings,
    conn: &mut rusqlite::Connection,
    api: &dyn StravaApi,
    options: SetupOptions,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    writeln!(output, "TileHarvester Setup\n{}", "=".repeat(40))?;
    if !options.credentials_checked {
        ensure_credentials(
            settings,
            options.client_id.as_deref(),
            options.client_secret.as_deref(),
            input,
            output,
        )?;
    }

    if api.is_authenticated() {
        writeln!(output, "Strava authentication: already connected.")?;
    } else {
        auth(
            api,
            None,
            options.open_browser && !is_container_or_remote(),
            options.listen_mode,
            options.stdin_is_tty,
            &settings.strava_redirect_uri,
            input,
            output,
        )?;
    }

    if let Some(kml_path) = options.kml {
        import_kml(settings, &kml_path, options.as_of.as_deref(), output)?;
    } else if needs_initial_backfill(settings, conn)? && !options.no_backfill {
        let interactive_kml = if options.stdin_is_tty {
            let answer = prompt_line(
                "Optional Squadrats KML export path (press Enter to auto-backfill from Strava): ",
                input,
                output,
            )?;
            (!answer.is_empty()).then(|| PathBuf::from(answer))
        } else {
            None
        };
        if let Some(kml_path) = interactive_kml {
            import_kml(settings, &kml_path, options.as_of.as_deref(), output)?;
        } else {
            writeln!(output, "Building your local tile history from Strava...")?;
            let result = strava_context(
                backfill_with_conn(settings, conn, api, None),
                "Backfill failed",
            )?;
            let remaining = unrefined_count_with_conn(settings, conn)?;
            writeln!(
                output,
                "Backfill complete: {} stored, {} processed.",
                result.stored, result.processed
            )?;
            if remaining > 0 && settings.auto_refine {
                writeln!(
                    output,
                    "{remaining} historical activities will refine to full-GPS accuracy automatically during background sync."
                )?;
            }
        }
    } else {
        writeln!(output, "Local tile history: already initialized.")?;
    }

    let in_container = Path::new("/.dockerenv").exists() || env::var_os("container").is_some();
    if options.install_service {
        let svc_options = ServiceOptions {
            user: true,
            run_as: None,
        };
        with_context(
            install_service_with_options(
                settings,
                None,
                None,
                settings.poll_interval_minutes,
                None,
                &svc_options,
                output,
            ),
            "Install failed",
        )?;
    } else if cfg!(target_os = "linux")
        && options.stdin_is_tty
        && !in_container
        && !options.no_service
    {
        let answer = prompt_line(
            &format!(
                "Install background systemd user timer (syncs every {} min)? [y/N]: ",
                settings.poll_interval_minutes
            ),
            input,
            output,
        )?;
        if matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
            let svc_options = ServiceOptions {
                user: true,
                run_as: None,
            };
            with_context(
                install_service_with_options(
                    settings,
                    None,
                    None,
                    settings.poll_interval_minutes,
                    None,
                    &svc_options,
                    output,
                ),
                "Install failed",
            )?;
            writeln!(
                output,
                "Tip: run `loginctl enable-linger \"$USER\"` so the timer keeps running after logout."
            )?;
        } else {
            writeln!(
                output,
                "Setup complete! Start syncing anytime with `tileharvester sync`."
            )?;
        }
    } else if in_container {
        writeln!(
            output,
            "Setup complete! Start background syncing with `docker compose up -d`."
        )?;
    } else {
        writeln!(
            output,
            "Setup complete! Start syncing anytime with `tileharvester sync`."
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn auth(
    api: &dyn StravaApi,
    code: Option<String>,
    open_browser: bool,
    listen_mode: ListenMode,
    stdin_is_tty: bool,
    redirect_uri: &str,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    let code = if let Some(code) = code.filter(|code| !code.is_empty()) {
        extract_code(&code)
    } else {
        if api.is_authenticated() {
            writeln!(
                output,
                "Already authenticated. Use auth --code <new-code> to re-authenticate if needed."
            )?;
            return Ok(());
        }

        let url = strava_context(api.build_auth_url(), "Authentication failed")?;
        let callback = match listen_mode {
            ListenMode::Force => Some(bind_oauth_callback(redirect_uri)?),
            ListenMode::Auto if can_auto_listen(redirect_uri, stdin_is_tty) => {
                match bind_oauth_callback(redirect_uri) {
                    Ok(bound) => Some(bound),
                    Err(error) => {
                        writeln!(
                            output,
                            "Note: {error}; falling back to manual callback paste."
                        )?;
                        None
                    }
                }
            }
            ListenMode::Auto | ListenMode::Disabled => None,
        };
        writeln!(output, "Open this URL in your browser:\n{url}")?;
        if open_browser {
            let _ = open::that(&url);
        }
        if let Some((listener, redirect)) = callback {
            writeln!(
                output,
                "Listening for Strava callback on http://{}{} ...",
                listener.local_addr()?,
                redirect.path()
            )?;
            output.flush()?;
            receive_oauth_callback(&listener, &redirect)?
        } else {
            write!(output, "Paste the callback URL or authorization code: ")?;
            output.flush()?;
            let mut raw = String::new();
            input.read_line(&mut raw)?;
            extract_code(&raw)
        }
    };

    if code.is_empty() {
        return Err(Error::Other("Error: No authorization code found.".into()));
    }
    let tokens = strava_context(api.exchange_code(&code), "Authentication failed")?;
    let athlete_id = match &tokens["athlete"]["id"] {
        serde_json::Value::Null => "None".to_owned(),
        serde_json::Value::String(id) => id.clone(),
        id => id.to_string(),
    };
    writeln!(
        output,
        "Authenticated successfully. Athlete ID: {athlete_id}"
    )?;
    Ok(())
}

fn bind_oauth_callback(uri: &str) -> Result<(TcpListener, url::Url)> {
    let invalid_redirect = || {
        Error::Validation(
            "auth --listen requires an HTTP redirect URI on localhost, 127.0.0.1, or [::1].".into(),
        )
    };
    let redirect = url::Url::parse(uri).map_err(|_| invalid_redirect())?;
    let address = match (redirect.scheme(), redirect.host_str()) {
        ("http", Some("localhost" | "127.0.0.1")) => "127.0.0.1",
        ("http", Some("[::1]")) => "::1",
        _ => return Err(invalid_redirect()),
    };
    let port = redirect.port().unwrap_or(80);
    let listener = TcpListener::bind((address, port)).map_err(|error| {
        Error::Other(format!(
            "Could not listen for Strava callback on {address} port {port}: {error}"
        ))
    })?;
    listener.set_nonblocking(true)?;
    Ok((listener, redirect))
}

fn read_callback_target(stream: &mut TcpStream, deadline: Instant) -> Option<String> {
    let mut request = [0; 8192];
    let mut length = 0;
    while length < request.len() {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        stream
            .set_read_timeout(Some(remaining.min(Duration::from_secs(1))))
            .ok()?;
        let bytes = stream.read(&mut request[length..]).ok()?;
        if bytes == 0 {
            return None;
        }
        length += bytes;
        if request[..length]
            .windows(4)
            .any(|window| window == b"\r\n\r\n")
        {
            let line = request[..length].split(|byte| *byte == b'\n').next()?;
            let mut words = std::str::from_utf8(line).ok()?.split_ascii_whitespace();
            if words.next()? == "GET" {
                return Some(words.next()?.to_owned());
            }
            return None;
        }
    }
    None
}

fn receive_oauth_callback(listener: &TcpListener, redirect: &url::Url) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        stream.set_nonblocking(false)?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        let target = read_callback_target(&mut stream, deadline);
        let (path, query) = target
            .as_deref()
            .unwrap_or_default()
            .split_once('?')
            .unwrap_or((target.as_deref().unwrap_or_default(), ""));
        if path != redirect.path() {
            let _ =
                write_callback_response(&mut stream, "404 Not Found", "Callback path not found.");
            continue;
        }
        let params = url::form_urlencoded::parse(query.as_bytes());
        if let Some((_, error)) = params.clone().find(|(key, _)| key == "error") {
            let _ = write_callback_response(
                &mut stream,
                "400 Bad Request",
                "Strava authorization failed. Check TileHarvester for details.",
            );
            return Err(Error::Other(format!(
                "Strava authorization failed: {error}"
            )));
        }
        if let Some((_, code)) = params
            .into_iter()
            .find(|(key, value)| key == "code" && !value.is_empty())
        {
            let _ = write_callback_response(
                &mut stream,
                "200 OK",
                "TileHarvester authorized — you can close this tab.",
            );
            return Ok(code.into_owned());
        }
        let _ = write_callback_response(
            &mut stream,
            "400 Bad Request",
            "Missing authorization code.",
        );
    }
    Err(Error::Other(
        "Timed out waiting for the Strava callback.".into(),
    ))
}

fn write_callback_response(stream: &mut TcpStream, status: &str, message: &str) -> io::Result<()> {
    let body = format!("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>TileHarvester</title><p>{message}</p></html>");
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

fn sync_timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn sync(
    settings: &Settings,
    conn: &mut rusqlite::Connection,
    api: &dyn StravaApi,
    once: bool,
    output: &mut impl Write,
) -> Result<i32> {
    if once {
        writeln!(output, "Running sync...")?;
        let result = strava_context(
            sync_once_with_conn(settings, conn, api, output),
            "Sync failed",
        )?;
        if result.auto_recomputed {
            writeln!(
                output,
                "Detected updated ignored sport types; recomputed local tile history."
            )?;
        }
        if result.backfilled {
            writeln!(
                output,
                "Auto-backfilled {} historical activities.",
                result.new_activities
            )?;
        }
        writeln!(
            output,
            "Sync complete: {} new, {} processed, {} annotated",
            result.new_activities, result.processed, result.annotated
        )?;
        if result.refined > 0 {
            writeln!(
                output,
                "Auto-refined {} historical activities ({} remaining).",
                result.refined, result.unrefined_remaining
            )?;
        }
        if result.failed > 0 {
            writeln!(
                output,
                "{} processing or annotation failures; see status/retry.",
                result.failed
            )?;
            return Ok(1);
        }
        return Ok(0);
    }

    static STOPPED: AtomicBool = AtomicBool::new(false);
    static HANDLER: std::sync::OnceLock<std::result::Result<(), ctrlc::Error>> =
        std::sync::OnceLock::new();
    STOPPED.store(false, Ordering::Relaxed);
    HANDLER
        .get_or_init(|| ctrlc::set_handler(|| STOPPED.store(true, Ordering::Relaxed)))
        .as_ref()
        .map_err(|error| Error::Other(format!("Could not handle Ctrl+C: {error}")))?;
    writeln!(
        output,
        "{} Starting continuous sync loop (press Ctrl+C to stop)...",
        sync_timestamp()
    )?;
    output.flush()?;
    let interval = Duration::from_secs(settings.poll_interval_minutes.saturating_mul(60));
    while !STOPPED.load(Ordering::Relaxed) {
        match sync_once_with_conn(settings, conn, api, output) {
            Ok(result) => {
                if result.auto_recomputed {
                    writeln!(
                        output,
                        "{} Recomputed local tile history after ignored sport changes",
                        sync_timestamp()
                    )?;
                }
                if result.backfilled {
                    writeln!(
                        output,
                        "{} Auto-backfilled {} historical activities",
                        sync_timestamp(),
                        result.new_activities
                    )?;
                }
                if result.annotated > 0 {
                    writeln!(
                        output,
                        "{} Annotated {} activities",
                        sync_timestamp(),
                        result.annotated
                    )?;
                }
                if result.refined > 0 {
                    writeln!(
                        output,
                        "{} Auto-refined {} historical activities ({} remaining)",
                        sync_timestamp(),
                        result.refined,
                        result.unrefined_remaining
                    )?;
                }
            }
            Err(error) => {
                writeln!(
                    output,
                    "{} Sync cycle failed: {}. Waiting before retry...",
                    sync_timestamp(),
                    classify_strava_error(&error)
                )?;
            }
        }
        output.flush()?;
        let start = Instant::now();
        while !STOPPED.load(Ordering::Relaxed) {
            let remaining = interval.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(remaining.min(Duration::from_millis(100)));
        }
    }
    writeln!(output, "\n{} Stopped.", sync_timestamp())?;
    output.flush()?;
    Ok(0)
}

fn status(
    settings: &Settings,
    conn: &rusqlite::Connection,
    api: &dyn StravaApi,
    output: &mut impl Write,
) -> Result<()> {
    let baseline = get_baseline(settings, conn)?;
    let unrefined = unrefined_count_with_conn(settings, conn)?;
    writeln!(output, "TileHarvester Status\n{}", "=".repeat(40))?;
    writeln!(
        output,
        "Database:                  {}",
        settings.db_path().display()
    )?;
    for (label, filter) in [
        ("Total activities in DB:", ""),
        ("Processed:", "WHERE status = 'processed'"),
        ("Pending:", "WHERE status = 'pending'"),
        ("Skipped no GPS:", "WHERE status = 'skipped_no_gps'"),
        ("Ignored sports:", "WHERE status = 'skipped_ignored_sport'"),
        ("Failed:", "WHERE status = 'failed'"),
        (
            "Stream-refined:",
            "WHERE status = 'processed' AND tile_source = 'streams_clean'",
        ),
    ] {
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM activities {filter}"),
            [],
            |row| row.get(0),
        )?;
        writeln!(output, "{label:<27}{count}")?;
    }
    writeln!(output, "{:<27}{unrefined}", "Needs stream refinement:")?;
    for (label, filter) in [
        ("Annotated:", "WHERE annotation_status = 'done'"),
        ("Annotation skipped:", "WHERE annotation_status = 'skipped'"),
        ("Annotation failed:", "WHERE annotation_status = 'failed'"),
        ("Unannotated processed:", "WHERE status = 'processed' AND (annotation_status IS NULL OR annotation_status = 'none')"),
    ] {
        let count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM activities {filter}"), [], |row| row.get(0))?;
        writeln!(output, "{label:<27}{count}")?;
    }
    writeln!(
        output,
        "Total unique Squadrats:    {}",
        effective_tile_count_with_conn(settings, conn, crate::models::TileKind::Squadrat.as_str())?
    )?;
    writeln!(
        output,
        "Total unique Squadratinhos: {}",
        effective_tile_count_with_conn(
            settings,
            conn,
            crate::models::TileKind::Squadratinho.as_str()
        )?
    )?;
    if settings.auto_refine && settings.auto_refine_per_cycle > 0 {
        writeln!(
            output,
            "Auto-refine:               enabled ({}/cycle, {}% quota reserve)",
            settings.auto_refine_per_cycle, settings.auto_refine_rate_limit_reserve_pct
        )?;
    } else {
        writeln!(output, "Auto-refine:               disabled")?;
    }
    if let Some(baseline) = baseline {
        writeln!(
            output,
            "KML baseline:              {}",
            baseline.source_name
        )?;
        writeln!(output, "Baseline as of (UTC):      {}", baseline.as_of_utc)?;
    }
    if !api.is_authenticated() {
        writeln!(output, "\nWarning: Not authenticated with Strava.")?;
    }
    Ok(())
}

fn stats(settings: &Settings, conn: &rusqlite::Connection, output: &mut impl Write) -> Result<()> {
    let today = Local::now().naive_local().date();
    let week = today - Days::new(today.weekday().num_days_from_monday().into());
    let month = today.with_day(1).expect("every month has a first day");
    let tx = conn.unchecked_transaction()?;
    let total = |start: NaiveDate| -> rusqlite::Result<(i64, i64)> {
        tx.query_row(
            "SELECT COALESCE(SUM(new_squadrat_count), 0), COALESCE(SUM(new_squadratinho_count), 0)
             FROM activities WHERE status = 'processed' AND start_local >= ?",
            [format!("{start}T00:00:00")],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    };
    let (week, week_inhos) = total(week)?;
    let (month, month_inhos) = total(month)?;
    let tiles = effective_tiles(settings, &tx, TileKind::Squadrat.as_str())?;
    let inhos = effective_tiles(settings, &tx, TileKind::Squadratinho.as_str())?;
    let large = compute_grid_metrics(tiles.iter().map(String::as_str), settings.squadrat_zoom);
    let small = compute_grid_metrics(inhos.iter().map(String::as_str), settings.squadratinho_zoom);
    writeln!(output, "TileHarvester Stats\n{}", "=".repeat(40))?;
    writeln!(output, "New Squadrats this week:  {week}")?;
    writeln!(output, "New Squadrats this month: {month}")?;
    writeln!(output, "Total unique Squadrats:   {}", tiles.len())?;
    writeln!(output, "Squadrats Yard:           {}", large.yard)?;
    writeln!(
        output,
        "Übersquadrat:             {0}x{0}",
        large.ubersquadrat
    )?;
    writeln!(output, "New Squadratinhos week:   {week_inhos}")?;
    writeln!(output, "New Squadratinhos month:  {month_inhos}")?;
    writeln!(output, "Total unique Squadratinhos: {}", inhos.len())?;
    writeln!(output, "Squadratinhos Yardinho:   {}", small.yard)?;
    writeln!(
        output,
        "Übersquadratinho:         {0}x{0}",
        small.ubersquadrat
    )?;
    Ok(())
}

fn kml_file_name(path: &Path) -> Result<String> {
    if !path.metadata()?.is_file() {
        return Err(Error::Validation(format!(
            "{} is not a file",
            path.display()
        )));
    }
    Ok(path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned())
}

fn import_kml(
    settings: &Settings,
    path: &Path,
    as_of: Option<&str>,
    output: &mut impl Write,
) -> Result<()> {
    let name = with_context(kml_file_name(path), "KML import failed")?;
    writeln!(output, "Reading Squadrats KML: {name}")?;
    let result = with_context(import_baseline(settings, path, as_of), "KML import failed")?;
    writeln!(
        output,
        "Imported {} Squadrats and {} Squadratinhos",
        format_thousands(result.squadrats as i64),
        format_thousands(result.squadratinhos as i64)
    )?;
    writeln!(output, "Baseline as of: {}", result.as_of_utc)?;
    writeln!(
        output,
        "Rebuilt novelty for {} stored activities",
        result.rebuilt
    )?;
    writeln!(
        output,
        "Future exports can be checked with 'tileharvester compare-kml'."
    )?;
    Ok(())
}

fn compare(
    settings: &Settings,
    conn: &Connection,
    path: &Path,
    details: usize,
    output: &mut impl Write,
) -> Result<()> {
    let name = with_context(kml_file_name(path), "KML comparison failed")?;
    writeln!(output, "Comparing Squadrats KML: {name}")?;
    let result = with_context(
        compare_kml_detailed_with_conn(settings, conn, path),
        "KML comparison failed",
    )?;
    writeln!(
        output,
        "Kind             KML      Local     Shared   KML-only Local-only"
    )?;
    let diffs = [
        ("Squadrats", result.squadrat),
        ("Squadratinhos", result.squadratinho),
    ];
    for (label, diff) in &diffs {
        let row = &diff.counts;
        let [kml, local, shared, kml_only, local_only] =
            [row.kml, row.local, row.shared, row.kml_only, row.local_only]
                .map(|count| format_thousands(count as i64));
        writeln!(
            output,
            "{label:<16} {kml:>8} {local:>10} {shared:>10} {kml_only:>10} {local_only:>10}"
        )?;
    }
    if details > 0 {
        for (label, diff) in diffs {
            for (side, tiles) in [
                ("KML-only", diff.kml_only_tiles),
                ("Local-only", diff.local_only_tiles),
            ] {
                writeln!(output, "{label} {side}:")?;
                for tile in tiles.iter().take(details) {
                    if let Some(coord) = parse_tile_coord(tile) {
                        let (lat, lon) = tile_center_lat_lon(coord);
                        writeln!(output, "  {tile} ({lat:.7}, {lon:.7})")?;
                    } else {
                        writeln!(output, "  {tile}")?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn inspect_gpx(settings: &Settings, path: &Path, output: &mut impl Write) -> Result<()> {
    let track = parse_gpx_file(path)?;
    let mut segments = Vec::new();
    let mut points = 0;
    let mut splits = 0;
    let mut spikes = 0;
    let mut stationary = 0;
    for segment in track.segments {
        let (cleaned, stats) = clean_stream_segments(settings, &segment.to_stream_json())?;
        points += segment.points.len();
        splits += stats.splits;
        spikes += stats.spikes_dropped;
        stationary += stats.stationary_dropped;
        segments.extend(cleaned);
    }
    let (squadrats, squadratinhos) = make_engine(settings).tiles_for_segments(&segments)?;
    let db_path = settings.db_path();
    let (new_squadrats, new_squadratinhos) = if db_path.try_exists()? {
        let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let tx = conn.unchecked_transaction()?;
        let local_squadrats = effective_tiles(settings, &tx, TileKind::Squadrat.as_str())?;
        let local_inhos = effective_tiles(settings, &tx, TileKind::Squadratinho.as_str())?;
        (
            squadrats.difference(&local_squadrats).count(),
            squadratinhos.difference(&local_inhos).count(),
        )
    } else {
        (squadrats.len(), squadratinhos.len())
    };
    let large = compute_grid_metrics(squadrats.iter().map(String::as_str), settings.squadrat_zoom);
    let small = compute_grid_metrics(
        squadratinhos.iter().map(String::as_str),
        settings.squadratinho_zoom,
    );

    if let Some(name) = track.name {
        writeln!(output, "Track name: {name}")?;
    }
    writeln!(output, "GPS points: {points}")?;
    writeln!(output, "Cleaned segments: {}", segments.len())?;
    writeln!(output, "GPS gaps split: {splits}")?;
    writeln!(output, "Spikes dropped: {spikes}")?;
    writeln!(output, "Stationary points dropped: {stationary}")?;
    writeln!(output, "Total Squadrats: {}", large.total)?;
    writeln!(output, "New Squadrats vs local DB: {new_squadrats}")?;
    writeln!(output, "Squadrats Yard: {}", large.yard)?;
    writeln!(output, "Übersquadrat: {0}x{0}", large.ubersquadrat)?;
    writeln!(output, "Total Squadratinhos: {}", small.total)?;
    writeln!(output, "New Squadratinhos vs local DB: {new_squadratinhos}")?;
    writeln!(output, "Squadratinhos Yardinho: {}", small.yard)?;
    writeln!(output, "Übersquadratinho: {0}x{0}", small.ubersquadrat)?;
    Ok(())
}

fn validate(
    settings: &Settings,
    conn: &rusqlite::Connection,
    api: &dyn StravaApi,
    activity_id: i64,
    output: &mut impl Write,
) -> Result<()> {
    let row = db::get_activity_row(conn, activity_id)?;
    let metadata = if row.as_ref().is_none_or(|row| row.start_local.is_empty()) {
        writeln!(output, "Fetching activity {activity_id} metadata...")?;
        Some(strava_context(api.get_activity(activity_id), "Failed")?)
    } else {
        None
    };
    let (start_local, start_utc) = if let Some(metadata) = &metadata {
        (
            metadata["start_date_local"].as_str(),
            metadata["start_date"].as_str(),
        )
    } else {
        let row = row.as_ref().expect("stored row supplied the timestamps");
        (Some(row.start_local.as_str()), Some(row.start_utc.as_str()))
    };

    writeln!(output, "Fetching activity {activity_id} GPS stream...")?;
    let streams = strava_context(api.get_activity_streams(activity_id, STREAM_KEYS), "Failed")?;
    let (segments, stats) = with_context(
        clean_stream_segments(settings, &streams),
        "Invalid GPS stream",
    )?;
    let (squadrats, squadratinhos) = with_context(
        make_engine(settings).tiles_for_segments(&segments),
        "Invalid GPS stream",
    )?;
    writeln!(output, "Activity {activity_id}:")?;
    let start_local = start_local.filter(|date| !date.is_empty());
    if let Some(start_local) = start_local {
        writeln!(output, "  Start local:     {start_local}")?;
    }
    writeln!(output, "  GPS points:      {}", stats.points)?;
    writeln!(
        output,
        "  Track segments:  {} ({} GPS gaps split)",
        stats.segments, stats.splits
    )?;
    writeln!(
        output,
        "  Total Squadrats (this activity):     {}",
        squadrats.len()
    )?;
    writeln!(
        output,
        "  Total Squadratinhos (this activity): {}",
        squadratinhos.len()
    )?;
    writeln!(
        output,
        "  Ratio (inho:squadrat):               {:.1}",
        squadratinhos.len() as f64 / squadrats.len().max(1) as f64
    )?;

    let novelty = if let Some(start_local) = start_local {
        let novelty = compute_historical_novelty_with_conn(
            settings,
            conn,
            activity_id,
            start_local,
            &squadrats,
            &squadratinhos,
            start_utc,
        )?;
        writeln!(output, "\n  Historical comparison from local DB:")?;
        writeln!(
            output,
            "    Processed activities before:       {}",
            novelty.processed_before
        )?;
        writeln!(
            output,
            "    Unique Squadrats before:           {}",
            novelty.total_squadrats_before
        )?;
        writeln!(
            output,
            "    Already-seen Squadrats on route:   {}",
            novelty.seen_squadrats
        )?;
        writeln!(
            output,
            "    New Squadrats from this activity:  {}",
            novelty.new_squadrats
        )?;
        writeln!(
            output,
            "    Unique Squadrats after:            {}",
            novelty.total_squadrats_after
        )?;
        writeln!(
            output,
            "    Unique Squadratinhos before:       {}",
            novelty.total_squadratinhos_before
        )?;
        writeln!(
            output,
            "    Already-seen Squadratinhos route:  {}",
            novelty.seen_squadratinhos
        )?;
        writeln!(
            output,
            "    New Squadratinhos from activity:   {}",
            novelty.new_squadratinhos
        )?;
        writeln!(
            output,
            "    Unique Squadratinhos after:        {}",
            novelty.total_squadratinhos_after
        )?;
        writeln!(output, "\n  NOTE: Historical comparison only reflects activities already processed in this local DB.")?;
        Some(novelty)
    } else {
        writeln!(
            output,
            "\n  Could not determine start date, so historical new-tile comparison was skipped."
        )?;
        None
    };

    if let Some(row) = row {
        writeln!(output, "\n  Stored in local DB:")?;
        writeln!(output, "    Status:                {}", row.status)?;
        writeln!(output, "    Stored Squadrats:      {}", row.squadrat_count)?;
        writeln!(
            output,
            "    Stored Squadratinhos:  {}",
            row.squadratinho_count
        )?;
        writeln!(
            output,
            "    Stored new Squadrats:  {}",
            row.new_squadrat_count
        )?;
        writeln!(
            output,
            "    Stored new Squadratinhos: {}",
            row.new_squadratinho_count
        )?;
        if let Some(novelty) = novelty {
            let matches = |stored, computed| {
                if usize::try_from(stored) == Ok(computed) {
                    "yes"
                } else {
                    "no"
                }
            };
            writeln!(
                output,
                "    Recomputed new Squadrats match stored: {}",
                matches(row.new_squadrat_count, novelty.new_squadrats)
            )?;
            writeln!(
                output,
                "    Recomputed new Squadratinhos match stored: {}",
                matches(row.new_squadratinho_count, novelty.new_squadratinhos)
            )?;
        }
    }
    Ok(())
}

fn health(
    settings: &Settings,
    conn: &rusqlite::Connection,
    api: &dyn StravaApi,
    check_rate_limit: bool,
    output: &mut impl Write,
) -> Result<i32> {
    let mut issues = Vec::new();
    writeln!(output, "TileHarvester Health Check\n{}", "=".repeat(40))?;
    match conn
        .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
        .map_err(Error::from)
    {
        Ok(_) => writeln!(output, "Database:                  OK")?,
        Err(error) => {
            issues.push(format!("Database: {error}"));
            writeln!(output, "Database:                  FAILED - {error}")?;
        }
    }
    writeln!(
        output,
        "DB location:               {}",
        settings.db_path().display()
    )?;
    writeln!(
        output,
        "Data directory:            {}",
        settings.data_dir.display()
    )?;

    let authenticated = api.is_authenticated();
    if authenticated {
        writeln!(output, "Strava auth:               Authenticated")?;
    } else {
        issues.push("Strava auth: not authenticated".into());
        writeln!(
            output,
            "Strava auth:               NOT AUTHENTICATED (run 'tileharvester auth')"
        )?;
    }
    if check_rate_limit && authenticated {
        writeln!(output, "Checking Strava rate limits...")?;
        let status = api.get_rate_limit_status();
        if status.ok {
            let rate = status.rate_limit;
            let daily = |value: Option<i64>| {
                value.map_or_else(|| "None".to_owned(), |value| value.to_string())
            };
            writeln!(
                output,
                "  Daily usage:             {}/{}",
                daily(rate.daily_used),
                daily(rate.daily_limit)
            )?;
            if let (Some(used), Some(limit)) = (
                rate.fifteen_min_used.filter(|&used| used != 0),
                rate.fifteen_min_limit.filter(|&limit| limit != 0),
            ) {
                writeln!(output, "  15-min usage:            {used}/{limit}")?;
            }
        } else {
            let error = status.error.as_deref().unwrap_or("Unknown error");
            issues.push(format!("Rate limit check: {error}"));
            writeln!(output, "  Rate limit check:        FAILED - {error}")?;
        }
    }
    writeln!(output)?;
    if issues.is_empty() {
        writeln!(output, "All checks passed.")?;
        Ok(0)
    } else {
        writeln!(output, "Issues found: {}", issues.len())?;
        for issue in issues {
            writeln!(output, "  - {issue}")?;
        }
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strava_client::RateLimitStatus;
    use serde_json::{json, Value};

    #[derive(Default)]
    struct MockApi {
        authenticated: bool,
        streams: Value,
    }

    impl StravaApi for MockApi {
        fn is_authenticated(&self) -> bool {
            self.authenticated
        }

        fn build_auth_url(&self) -> Result<String> {
            Ok("https://example.test/authorize".into())
        }

        fn exchange_code(&self, code: &str) -> Result<Value> {
            assert_eq!(code, "a+b");
            Ok(json!({"athlete": {"id": 42}}))
        }

        fn get_athlete(&self) -> Result<Value> {
            unreachable!()
        }

        fn get_activities(
            &self,
            _: u32,
            _: u32,
            _: Option<i64>,
            _: Option<i64>,
        ) -> Result<Vec<Value>> {
            Ok(vec![])
        }

        fn get_activity(&self, id: i64) -> Result<Value> {
            assert_eq!(id, 42);
            Ok(json!({
                "start_date": "2026-08-02T10:00:00Z",
                "start_date_local": "2026-08-02T10:00:00"
            }))
        }

        fn get_activity_streams(&self, _: i64, keys: &str) -> Result<Value> {
            assert_eq!(keys, STREAM_KEYS);
            Ok(self.streams.clone())
        }

        fn update_activity_description(&self, _: i64, _: &str) -> Result<Value> {
            unreachable!()
        }

        fn get_rate_limit_status(&self) -> RateLimitStatus {
            panic!("rate limit check should be skipped")
        }
    }

    fn invoke(settings: &Settings, api: &MockApi, args: &[&str], input: &str) -> (i32, String) {
        let cli = parse_from(std::iter::once("tileharvester").chain(args.iter().copied())).unwrap();
        let mut output = Vec::new();
        let exit = run_cli_with(
            cli,
            settings.clone(),
            api,
            &mut input.as_bytes(),
            &mut output,
        );
        (exit, String::from_utf8(output).unwrap())
    }

    #[test]
    fn version_does_not_migrate() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::with_data_dir(dir.path());
        assert_eq!(
            invoke(&settings, &MockApi::default(), &["--version"], ""),
            (0, format!("{VERSION}\n"))
        );
        assert!(!settings.db_path().exists());
    }

    #[test]
    fn extracts_raw_and_percent_encoded_codes() {
        assert_eq!(extract_code("  raw+code  "), "raw+code");
        assert_eq!(
            extract_code("https://example.test/?code=&state=x&code=a%2Bb#code=no"),
            "a+b"
        );
        assert_eq!(
            extract_code("https://example.test/?state=x"),
            "https://example.test/?state=x"
        );
    }

    #[test]
    fn auth_prompts_with_bare_no_open_browser_flag() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::with_data_dir(dir.path());
        let (exit, output) = invoke(
            &settings,
            &MockApi::default(),
            &["auth", "--open-browser", "--no-open-browser"],
            "https://example.test/?code=a%2Bb\n",
        );
        assert_eq!(exit, 0, "{output}");
        assert!(output.contains("Paste the callback URL or authorization code: Authenticated successfully. Athlete ID: 42"));
    }

    #[test]
    fn auth_listen_ignores_favicon_and_exchanges_decoded_code() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::with_data_dir(dir.path());
        settings.strava_redirect_uri = "http://127.0.0.1:0/callback".into();
        let cli = parse_from(["tileharvester", "auth", "--listen", "--no-open-browser"]).unwrap();

        struct CallbackOutput {
            bytes: Vec<u8>,
            address: std::sync::mpsc::Sender<String>,
        }

        impl Write for CallbackOutput {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.bytes.write(bytes)
            }

            fn flush(&mut self) -> io::Result<()> {
                if let Some(address) = String::from_utf8_lossy(&self.bytes)
                    .lines()
                    .find_map(|line| line.strip_prefix("Listening for Strava callback on http://"))
                    .and_then(|line| line.strip_suffix("/callback ..."))
                {
                    let _ = self.address.send(address.to_owned());
                }
                Ok(())
            }
        }

        let (address, receiver) = std::sync::mpsc::channel();
        let mut output = CallbackOutput {
            bytes: Vec::new(),
            address,
        };
        let client = thread::spawn(move || {
            let address = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            let request = |target| {
                let mut stream = TcpStream::connect(&address).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                write!(stream, "GET {target} HTTP/1.1\r\nHost: {address}\r\n\r\n").unwrap();
                let mut response = String::new();
                stream.read_to_string(&mut response).unwrap();
                response
            };
            let favicon = request("/favicon.ico");
            let callback = request("/callback?code=a%2Bb");
            assert!(favicon.starts_with("HTTP/1.1 404 Not Found"));
            assert!(callback.starts_with("HTTP/1.1 200 OK"));
            assert!(callback.contains("TileHarvester authorized — you can close this tab."));
        });
        assert_eq!(
            run_cli_with(
                cli,
                settings,
                &MockApi::default(),
                &mut "".as_bytes(),
                &mut output
            ),
            0
        );
        client.join().unwrap();
        assert!(String::from_utf8(output.bytes)
            .unwrap()
            .contains("Authenticated successfully. Athlete ID: 42"));
    }

    #[test]
    fn sync_once_exits_nonzero_on_a_processing_failure() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::with_data_dir(dir.path());
        db::migrate(&settings).unwrap();
        db::get_db(&settings).unwrap().execute(
            "INSERT INTO activities (id, start_utc, start_local) VALUES (1, '2026-08-02T10:00:00Z', '2026-08-02T10:00:00')",
            [],
        ).unwrap();
        let api = MockApi {
            authenticated: true,
            ..Default::default()
        };
        let (exit, output) = invoke(&settings, &api, &["sync", "--once", "--offset", "-3"], "");
        assert_eq!(exit, 1, "{output}");
        assert!(output.contains("Sync complete: 0 new, 0 processed, 0 annotated"));
        assert!(output.contains("1 processing or annotation failures; see status/retry."));
    }

    #[test]
    fn imports_compares_and_validates_against_a_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::with_data_dir(dir.path());
        let path = dir.path().join("snapshot.kml");
        let polygon = "<Polygon><outerBoundaryIs><LinearRing><coordinates>0,0 0.022,0 0.022,-0.022 0,-0.022 0,0</coordinates></LinearRing></outerBoundaryIs></Polygon>";
        std::fs::write(&path, format!(
            "<kml><Placemark><name>squadrats</name>{polygon}</Placemark><Placemark><name>squadratinhos</name>{polygon}</Placemark></kml>"
        )).unwrap();
        let (exit, output) = invoke(
            &settings,
            &MockApi::default(),
            &["compare-kml", path.to_str().unwrap(), "--details", "5"],
            "",
        );
        assert_eq!(exit, 0, "{output}");
        assert!(output.contains("Squadrats KML-only:\n  14:"));
        let inho_details = output.split_once("Squadratinhos KML-only:\n").unwrap().1;
        let tiles: Vec<_> = inho_details
            .lines()
            .take_while(|line| line.starts_with("  "))
            .collect();
        assert_eq!(tiles.len(), 5);
        assert!(tiles
            .iter()
            .all(|tile| tile.contains(" (") && tile.ends_with(')')));
        assert!(tiles.windows(2).all(|pair| pair[0] < pair[1]));

        let (exit, output) = invoke(
            &settings,
            &MockApi::default(),
            &[
                "import-kml",
                path.to_str().unwrap(),
                "--as-of",
                "2026-08-01T00:00:00Z",
            ],
            "",
        );
        assert_eq!(exit, 0, "{output}");
        assert!(output.contains("Imported 1 Squadrats and 64 Squadratinhos"));

        let (exit, output) = invoke(
            &settings,
            &MockApi::default(),
            &["compare-kml", path.to_str().unwrap()],
            "",
        );
        assert_eq!(exit, 0, "{output}");
        assert!(output
            .contains("Squadrats               1          1          1          0          0"));

        let api = MockApi {
            authenticated: true,
            streams: json!({"latlng": {"data": [[-0.0101, 0.0101], [-0.0102, 0.0102]]}}),
        };
        let (exit, output) = invoke(&settings, &api, &["validate", "42"], "");
        assert_eq!(exit, 0, "{output}");
        assert!(output.contains("Unique Squadrats before:           1"));
        assert!(output.contains("New Squadrats from this activity:  0"));
        let (exit, output) = invoke(&settings, &MockApi::default(), &["status"], "");
        assert_eq!(exit, 0, "{output}");
        assert!(output.contains("KML baseline:              snapshot.kml"));
    }

    #[test]
    fn inspects_gpx_without_creating_a_database_and_rejects_dtd() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::with_data_dir(dir.path().join("data"));
        let path = dir.path().join("track.gpx");
        std::fs::write(
            &path,
            r#"<gpx><trk><name>Morning ride</name><trkseg>
            <trkpt lat="0.001" lon="0.001"/><trkpt lat="0.0011" lon="0.0011"/>
            </trkseg></trk></gpx>"#,
        )
        .unwrap();
        let args = ["inspect-gpx", path.to_str().unwrap()];
        let (exit, output) = invoke(&settings, &MockApi::default(), &args, "");
        assert_eq!(exit, 0, "{output}");
        for line in [
            "Track name: Morning ride",
            "GPS points: 2",
            "Cleaned segments: 1",
            "Total Squadrats: 1",
            "Total Squadratinhos: 1",
            "New Squadrats vs local DB: 1",
            "New Squadratinhos vs local DB: 1",
        ] {
            assert!(output.contains(line), "{output}");
        }
        assert!(!settings.db_path().exists());
        db::migrate(&settings).unwrap();
        db::get_db(&settings).unwrap().execute(
            "INSERT INTO global_tiles VALUES ('squadrat', '14:8192:8191', 1, '2026-01-01T00:00:00'),
                                             ('squadratinho', '17:65536:65535', 1, '2026-01-01T00:00:00')",
            [],
        ).unwrap();
        let (exit, output) = invoke(&settings, &MockApi::default(), &args, "");
        assert_eq!(exit, 0, "{output}");
        assert!(output.contains("New Squadrats vs local DB: 0"));
        assert!(output.contains("New Squadratinhos vs local DB: 0"));

        std::fs::write(&path, "<!DOCTYPE gpx><gpx/>").unwrap();
        let (exit, output) = invoke(&settings, &MockApi::default(), &args, "");
        assert_eq!(exit, 1, "{output}");
        assert!(output.starts_with("GPX inspection failed:"));
    }

    #[test]
    fn validate_reports_malformed_streams() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::with_data_dir(dir.path());
        let api = MockApi {
            authenticated: true,
            streams: json!({"latlng": null}),
        };
        let (exit, output) = invoke(&settings, &api, &["validate", "42"], "");
        assert_eq!(exit, 1, "{output}");
        assert!(output.contains("Invalid GPS stream:"));
    }

    #[test]
    fn status_stats_and_reset_use_the_configured_database() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::with_data_dir(dir.path());
        let api = MockApi::default();
        db::migrate(&settings).unwrap();
        let now = Local::now()
            .naive_local()
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        db::get_db(&settings).unwrap().execute(
            "INSERT INTO activities (id, start_utc, start_local, status, new_squadrat_count, new_squadratinho_count) VALUES (1, ?1, ?1, 'processed', 7, 11)",
            [&now],
        ).unwrap();
        let conn = db::get_db(&settings).unwrap();
        for (kind, zoom) in [("squadrat", 14), ("squadratinho", 17)] {
            for x in 0..3 {
                for y in 0..3 {
                    conn.execute(
                        "INSERT INTO global_tiles VALUES (?1, ?2, 1, ?3)",
                        [kind, &format!("{zoom}:{x}:{y}"), &now],
                    )
                    .unwrap();
                }
            }
        }
        let (exit, output) = invoke(&settings, &api, &["reset-db"], "");
        assert_eq!(exit, 1);
        assert_eq!(
            output,
            "This will delete all local data. Use --force to confirm.\n"
        );
        let (exit, output) = invoke(&settings, &api, &["status"], "");
        assert_eq!(exit, 0);
        assert!(output.contains("Processed:                 1"));
        assert!(output.contains("Warning: Not authenticated with Strava."));
        let (exit, output) = invoke(&settings, &api, &["stats"], "");
        assert_eq!(exit, 0);
        assert!(output.contains("New Squadrats this week:  7"));
        assert!(output.contains("New Squadrats this month: 7"));
        for line in [
            "Squadrats Yard:           1",
            "Übersquadrat:             3x3",
            "New Squadratinhos week:   11",
            "New Squadratinhos month:  11",
            "Total unique Squadratinhos: 9",
            "Squadratinhos Yardinho:   1",
            "Übersquadratinho:         3x3",
        ] {
            assert!(output.contains(line), "{output}");
        }
        assert_eq!(
            invoke(&settings, &api, &["reset-db", "--force"], ""),
            (0, "Database reset.\n".into())
        );
        let (_, output) = invoke(&settings, &api, &["status"], "");
        assert!(output.contains("Total activities in DB:    0"));
    }

    #[test]
    fn health_skips_rate_limit_with_bare_flag() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::with_data_dir(dir.path());
        let api = MockApi {
            authenticated: true,
            ..Default::default()
        };
        let (exit, output) = invoke(&settings, &api, &["health", "--no-check-rate-limit"], "");
        assert_eq!(exit, 0);
        assert!(output.contains("Database:                  OK"));
        assert!(output.ends_with("All checks passed.\n"));
    }

    #[test]
    fn service_print_uses_requested_options() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::with_data_dir(dir.path());
        let (exit, output) = invoke(
            &settings,
            &MockApi::default(),
            &[
                "service",
                "print",
                "--interval",
                "8",
                "--binary",
                "/bin/tileharvester",
                "--env-file",
                "/etc/tileharvester.env",
            ],
            "",
        );
        assert_eq!(exit, 0);
        assert!(output.contains("ExecStart=/bin/tileharvester sync --once"));
        assert!(output.contains("EnvironmentFile=-/etc/tileharvester.env"));
        assert!(output.contains("OnUnitActiveSec=8min"));
    }

    #[test]
    fn setup_accepts_kml_flag_and_auth_no_listen_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::with_data_dir(dir.path());
        let kml = dir.path().join("snapshot.kml");
        let polygon = "<Polygon><outerBoundaryIs><LinearRing><coordinates>0,0 0.022,0 0.022,-0.022 0,-0.022 0,0</coordinates></LinearRing></outerBoundaryIs></Polygon>";
        std::fs::write(
            &kml,
            format!("<kml><Placemark><name>squadrats</name>{polygon}</Placemark><Placemark><name>squadratinhos</name>{polygon}</Placemark></kml>"),
        )
        .unwrap();
        let (exit, output) = invoke(
            &settings,
            &MockApi::default(),
            &[
                "setup",
                "--client-id",
                "111",
                "--client-secret",
                "222",
                "--kml",
                kml.to_str().unwrap(),
                "--as-of",
                "2026-08-01T00:00:00Z",
                "--no-open-browser",
                "--no-listen",
                "--no-service",
            ],
            "https://example.test/?code=a%2Bb\n",
        );
        assert_eq!(exit, 0, "{output}");
        assert!(output.contains("Saved Strava credentials"), "{output}");
        assert!(output.contains("Authenticated successfully"), "{output}");
        assert!(
            output.contains("Imported 1 Squadrats and 64 Squadratinhos"),
            "{output}"
        );
        assert!(output.contains("Setup complete!"), "{output}");
    }

    #[test]
    fn auto_listen_falls_back_to_paste_when_port_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let busy = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = busy.local_addr().unwrap().port();
        let mut settings = Settings::with_data_dir(dir.path());
        settings.strava_redirect_uri = format!("http://127.0.0.1:{port}/callback");
        let mut output = Vec::new();
        let mut input = &b"https://example.test/?code=a%2Bb\n"[..];
        auth(
            &MockApi::default(),
            None,
            false,
            ListenMode::Auto,
            true,
            &settings.strava_redirect_uri,
            &mut input,
            &mut output,
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(
            text.contains("Authenticated successfully. Athlete ID: 42"),
            "{text}"
        );
        drop(busy);
    }
}
