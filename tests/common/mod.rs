#![allow(dead_code)]

pub mod fixtures;
pub mod mock_strava;

pub fn isolated_db() -> (tempfile::TempDir, tileharvester::config::Settings) {
    let temp = tempfile::tempdir().expect("create isolated database directory");
    let settings = tileharvester::config::Settings::with_data_dir(temp.path());
    tileharvester::db::migrate(&settings).expect("migrate isolated database");
    (temp, settings)
}
