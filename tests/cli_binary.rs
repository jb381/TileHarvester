use assert_cmd::Command;
use predicates::str::contains;
use tempfile::TempDir;
use tileharvester::{
    config::Settings,
    db::{get_db, migrate},
    strava_client::save_tokens,
};

fn command(dir: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tileharvester"));
    command
        .current_dir(dir.path())
        .env_clear()
        .env("TH_DATA_DIR", dir.path());
    command
}

#[test]
fn help_and_version_do_not_create_database() {
    let dir = tempfile::tempdir().unwrap();
    command(&dir)
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("Usage:"))
        .stdout(contains("backfill"));
    command(&dir)
        .assert()
        .code(2)
        .stderr(contains("Usage:"))
        .stderr(contains("backfill"));
    command(&dir)
        .arg("--version")
        .assert()
        .success()
        .stdout("tileharvester v0.1.0\n");
    assert!(!Settings::with_data_dir(dir.path()).db_path().exists());
}

#[test]
fn commands_that_need_auth_fail_without_a_token() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        &["sync", "--once"][..],
        &["backfill"],
        &["refine"],
        &["retry"],
        &["validate", "1"],
    ] {
        command(&dir)
            .args(args)
            .assert()
            .code(1)
            .stdout(contains("Not authenticated"));
    }
}

#[test]
fn status_stats_baseline_import_compare_and_recompute() {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings::with_data_dir(dir.path());
    command(&dir)
        .arg("status")
        .assert()
        .success()
        .stdout(contains("Total activities in DB:    0"))
        .stdout(contains("Not authenticated"));
    let path = dir.path().join("snapshot.kml");
    let layer = |name: &str, coordinates: &str| {
        format!("<Placemark><name>{name}</name><Polygon><outerBoundaryIs><LinearRing><coordinates>{coordinates}</coordinates></LinearRing></outerBoundaryIs></Polygon></Placemark>")
    };
    let coarse = layer("squadrats", "0,0 0.022,0 0.022,0.022 0,0.022 0,0");
    let fine = layer(
        "squadratinhos",
        "0,0 0.00275,0 0.00275,0.00275 0,0.00275 0,0",
    );
    std::fs::write(
        &path,
        format!("<kml><Document>{coarse}{fine}</Document></kml>"),
    )
    .unwrap();
    let path = path.to_str().unwrap();
    command(&dir)
        .args(["import-kml", path, "--as-of", "2026-08-01T00:00:00Z"])
        .assert()
        .success()
        .stdout(contains("Baseline as of:"));
    command(&dir)
        .args(["import-kml", path])
        .assert()
        .code(1)
        .stdout(contains("already installed"));
    command(&dir)
        .args(["compare-kml", path])
        .assert()
        .success()
        .stdout(contains("Comparing Squadrats KML"));
    get_db(&settings).unwrap().execute("INSERT INTO activities (id,start_utc,start_local) VALUES (1,'2026-08-02T10:00:00Z','2026-08-02T10:00:00')",[]).unwrap();
    command(&dir)
        .arg("status")
        .assert()
        .success()
        .stdout(contains("Total activities in DB:    1"))
        .stdout(contains("snapshot.kml"))
        .stdout(contains("Total unique Squadrats:    1"));
    command(&dir)
        .arg("stats")
        .assert()
        .success()
        .stdout(contains("New Squadrats this week:"))
        .stdout(contains("New Squadrats this month:"));
    command(&dir)
        .arg("recompute")
        .assert()
        .success()
        .stdout(contains("Recomputing global tiles"));
    command(&dir)
        .arg("recompute-novelty")
        .assert()
        .success()
        .stdout(contains("Rebuilding global totals"));
}

#[test]
fn service_configuration_and_validation() {
    let dir = tempfile::tempdir().unwrap();
    command(&dir)
        .args([
            "service",
            "print",
            "--interval",
            "10",
            "--env-file",
            "/tmp/custom.env",
        ])
        .assert()
        .success()
        .stdout(contains("OnUnitActiveSec=10min"))
        .stdout(contains("/tmp/custom.env"))
        .stdout(contains("sync --once"))
        .stdout(contains("NoNewPrivileges=true"))
        .stdout(contains("ProtectSystem=strict"))
        .stdout(contains("PrivateTmp=true"))
        .stdout(contains("ReadWritePaths="));
    command(&dir)
        .args(["service", "print", "--run-as", "tileharvester"])
        .assert()
        .success()
        .stdout(contains("User=tileharvester"));
    let user = command(&dir)
        .args(["service", "print", "--user"])
        .assert()
        .success()
        .stdout(contains("~/.config/systemd/user/"))
        .stdout(contains("systemctl --user daemon-reload"))
        .stdout(contains(
            "systemctl --user enable --now tileharvester.timer",
        ))
        .stdout(contains("loginctl enable-linger"));
    let output = String::from_utf8_lossy(&user.get_output().stdout);
    assert!(output.contains("NoNewPrivileges=true"));
    for directive in ["ReadWritePaths=", "ProtectSystem=", "PrivateTmp=", "User="] {
        assert!(!output.contains(directive));
    }
    command(&dir)
        .args(["service", "print", "--user", "--run-as", "tileharvester"])
        .assert()
        .code(1)
        .stdout(contains("--user and --run-as cannot be combined"));
    command(&dir)
        .args(["service", "bogus"])
        .assert()
        .code(1)
        .stdout(contains("bogus"));
}

#[test]
fn health_checks_token_presence_without_network() {
    let dir = tempfile::tempdir().unwrap();
    command(&dir)
        .args(["health", "--no-check-rate-limit"])
        .assert()
        .code(1)
        .stdout(contains("Database:                  OK"));
    save_tokens(
        &Settings::with_data_dir(dir.path()),
        &serde_json::json!({"access_token":"fake"}),
    )
    .unwrap();
    command(&dir)
        .args(["health", "--no-check-rate-limit"])
        .assert()
        .success()
        .stdout(contains("All checks passed"));
}

#[test]
fn reset_requires_force_and_clears_database() {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings::with_data_dir(dir.path());
    migrate(&settings).unwrap();
    tileharvester::db::set_setting(&settings, "test", &serde_json::json!(true)).unwrap();
    command(&dir)
        .arg("reset-db")
        .assert()
        .code(1)
        .stdout(contains("--force"));
    assert_eq!(
        tileharvester::db::get_setting(&settings, "test").unwrap(),
        Some(serde_json::json!(true))
    );
    command(&dir)
        .args(["reset-db", "--force"])
        .assert()
        .success()
        .stdout(contains("Database reset"));
    migrate(&settings).unwrap();
    assert_eq!(
        tileharvester::db::get_setting(&settings, "test").unwrap(),
        None
    );
}

#[test]
fn backup_is_readable_and_overwrite_requires_force() {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings::with_data_dir(dir.path());
    migrate(&settings).unwrap();
    let dest = "snapshots/backup.db";
    let snapshot_value = || {
        rusqlite::Connection::open(dir.path().join(dest))
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key = 'example'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    };
    tileharvester::db::set_setting(&settings, "example", &serde_json::json!(1)).unwrap();
    command(&dir)
        .args(["backup", dest])
        .assert()
        .success()
        .stdout(contains(format!("Backup written to {dest}")));
    tileharvester::db::set_setting(&settings, "example", &serde_json::json!(2)).unwrap();
    command(&dir)
        .args(["backup", dest])
        .assert()
        .code(1)
        .stdout(contains("--force"));
    assert_eq!(snapshot_value(), "1");
    command(&dir)
        .args(["backup", dest, "--force"])
        .assert()
        .success();
    assert_eq!(snapshot_value(), "2");
    command(&dir)
        .args(["backup", "./tileharvester.db", "--force"])
        .assert()
        .code(1)
        .stdout(contains("live database"));
}

#[test]
fn loads_persisted_env_from_data_dir_regardless_of_cwd() {
    let data_dir = tempfile::tempdir().unwrap();
    let other_cwd = tempfile::tempdir().unwrap();
    tileharvester::config::save_env_values(
        &data_dir.path().join(".env"),
        &[
            ("TH_STRAVA_CLIENT_ID", "99999"),
            ("TH_STRAVA_CLIENT_SECRET", "super-secret"),
            ("TH_AUTO_REFINE_PER_CYCLE", "7"),
        ],
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tileharvester"));
    cmd.current_dir(other_cwd.path())
        .env_clear()
        .env("TH_DATA_DIR", data_dir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(contains("Auto-refine:               enabled (7/cycle"));

    let mut help = Command::new(env!("CARGO_BIN_EXE_tileharvester"));
    help.current_dir(other_cwd.path())
        .env_clear()
        .env("TH_DATA_DIR", data_dir.path())
        .args(["setup", "--help"])
        .assert()
        .success()
        .stdout(contains("--kml"))
        .stdout(contains("--install-service"));
}
