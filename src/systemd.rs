//! Systemd service and timer generation.

use std::{env, fs, io::Write, path::Path, process::Command};

use crate::config::Settings;
use crate::error::{Error, Result};

#[derive(Debug, Default)]
pub struct ServiceOptions {
    pub user: bool,
    pub run_as: Option<String>,
}

pub fn generate_service(
    settings: &Settings,
    data_dir: Option<&str>,
    binary: Option<&str>,
    env_file: Option<&str>,
) -> String {
    generate_service_with_options(
        settings,
        data_dir,
        binary,
        env_file,
        &ServiceOptions::default(),
    )
    .expect("default systemd options are valid")
}

pub fn generate_service_with_options(
    settings: &Settings,
    data_dir: Option<&str>,
    binary: Option<&str>,
    env_file: Option<&str>,
    options: &ServiceOptions,
) -> Result<String> {
    if let Some(user) = &options.run_as {
        if options.user {
            return Err(Error::Validation(
                "--user and --run-as cannot be combined.".into(),
            ));
        }
        if user.is_empty()
            || user
                .chars()
                .any(|ch| !ch.is_ascii_alphanumeric() && !matches!(ch, '_' | '-' | '.'))
        {
            return Err(Error::Validation(
                "--run-as requires a valid system user name or numeric UID.".into(),
            ));
        }
    }
    let default_data_dir = settings.data_dir.to_string_lossy();
    let data_dir = data_dir
        .filter(|value| !value.is_empty())
        .unwrap_or(&default_data_dir);
    let binary = binary
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            env::current_exe()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "tileharvester".into())
        });
    let env_file = env_file
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            env::current_dir()
                .unwrap_or_default()
                .join(".env")
                .to_string_lossy()
                .into_owned()
        });
    let binary = unit_word(&binary);
    let environment = unit_word(&format!("TH_DATA_DIR={data_dir}"));
    let env_file = env_file.replace('%', "%%").replace(['\n', '\r'], "");
    let mut service = format!(
        "[Unit]\nDescription=TileHarvester Strava Sync\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nType=oneshot\nExecStart={binary} sync --once\nEnvironment={environment}\nEnvironmentFile=-{env_file}\nNoNewPrivileges=true\n"
    );
    if !options.user {
        service.push_str("ProtectSystem=strict\nPrivateTmp=true\n");
        service.push_str(&format!("ReadWritePaths={}\n", unit_word(data_dir)));
        if let Some(user) = &options.run_as {
            service.push_str(&format!("User={user}\n"));
        }
    }
    Ok(service)
}

fn unit_word(value: &str) -> String {
    let value = value.replace('%', "%%");
    if value
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '\'' | '\\'))
    {
        format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
        )
    } else {
        value
    }
}

pub fn generate_timer(interval_minutes: u64) -> String {
    format!(
        "[Unit]\nDescription=TileHarvester Sync Timer\n\n\
         [Timer]\nOnBootSec=2min\nOnUnitActiveSec={interval_minutes}min\nAccuracySec=1min\nPersistent=true\n\n\
         [Install]\nWantedBy=timers.target\n"
    )
}

pub fn print_service(
    settings: &Settings,
    data_dir: Option<&str>,
    binary: Option<&str>,
    interval: u64,
    env_file: Option<&str>,
) {
    let _ = write_service_with_options(
        settings,
        data_dir,
        binary,
        interval,
        env_file,
        &ServiceOptions::default(),
        &mut std::io::stdout(),
    );
}

pub fn write_service_with_options(
    settings: &Settings,
    data_dir: Option<&str>,
    binary: Option<&str>,
    interval: u64,
    env_file: Option<&str>,
    options: &ServiceOptions,
    output: &mut impl Write,
) -> Result<()> {
    writeln!(
        output,
        "=== tileharvester.service ===\n{}\n",
        generate_service_with_options(settings, data_dir, binary, env_file, options)?
    )?;
    writeln!(
        output,
        "=== tileharvester.timer ===\n{}\n",
        generate_timer(interval)
    )?;
    if options.user {
        writeln!(output, "Install with:\n  mkdir -p ~/.config/systemd/user/\n  cp tileharvester.service tileharvester.timer ~/.config/systemd/user/\n  systemctl --user daemon-reload\n  systemctl --user enable --now tileharvester.timer\nKeep the timer running after logout with `loginctl enable-linger \"$USER\"`.")?;
    } else {
        writeln!(output, "Install with:\n  sudo cp tileharvester.service tileharvester.timer /etc/systemd/system/\n  sudo systemctl daemon-reload\n  sudo systemctl enable --now tileharvester.timer")?;
    }
    Ok(())
}

fn write_unit(path: &Path, content: &str) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().expect("unit path has a parent"))?;
    file.write_all(content.as_bytes())?;
    match fs::metadata(path) {
        Ok(metadata) => file.as_file().set_permissions(metadata.permissions())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.as_file()
                    .set_permissions(fs::Permissions::from_mode(0o644))?;
            }
        }
        Err(error) => return Err(error.into()),
    }
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub fn install_service(
    settings: &Settings,
    data_dir: Option<&str>,
    binary: Option<&str>,
    interval: u64,
    env_file: Option<&str>,
) -> Result<()> {
    install_service_with_options(
        settings,
        data_dir,
        binary,
        interval,
        env_file,
        &ServiceOptions::default(),
        &mut std::io::stdout(),
    )
}

pub fn install_service_with_options(
    settings: &Settings,
    data_dir: Option<&str>,
    binary: Option<&str>,
    interval: u64,
    env_file: Option<&str>,
    options: &ServiceOptions,
    output: &mut impl Write,
) -> Result<()> {
    let service = generate_service_with_options(settings, data_dir, binary, env_file, options)?;
    let directory = if options.user {
        dirs::config_dir()
            .ok_or_else(|| {
                Error::Config("Could not determine the user configuration directory.".into())
            })?
            .join("systemd/user")
    } else {
        Path::new("/etc/systemd/system").to_owned()
    };
    fs::create_dir_all(&directory)?;
    write_unit(&directory.join("tileharvester.service"), &service)?;
    write_unit(
        &directory.join("tileharvester.timer"),
        &generate_timer(interval),
    )?;
    for args in [
        &["daemon-reload"][..],
        &["enable", "--now", "tileharvester.timer"],
    ] {
        let mut command = Command::new("systemctl");
        if options.user {
            command.arg("--user");
        }
        let status = command.args(args).status()?;
        if !status.success() {
            return Err(Error::Other(format!(
                "systemctl {} failed with {status}",
                args.join(" ")
            )));
        }
    }
    writeln!(output, "Systemd timer installed and started.")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_template_uses_native_binary_and_overrides() {
        assert_eq!(
            generate_service(
                &Settings::default(),
                Some("/data"),
                Some("/usr/local/bin/tileharvester"),
                Some("/config/.env"),
            ),
            "[Unit]\nDescription=TileHarvester Strava Sync\nAfter=network-online.target\nWants=network-online.target\n\n\
             [Service]\nType=oneshot\nExecStart=/usr/local/bin/tileharvester sync --once\n\
             Environment=TH_DATA_DIR=/data\nEnvironmentFile=-/config/.env\nNoNewPrivileges=true\nProtectSystem=strict\nPrivateTmp=true\nReadWritePaths=/data\n"
        );
    }

    #[test]
    fn service_defaults_to_running_binary_settings_and_current_directory() {
        let settings = Settings::with_data_dir("/configured data");
        let service = generate_service(&settings, None, None, None);
        let binary = env::current_exe().unwrap();
        let env_file = env::current_dir().unwrap().join(".env");
        assert!(service.contains(&format!(
            "ExecStart={} sync --once",
            unit_word(&binary.to_string_lossy())
        )));
        assert!(service.contains("Environment=\"TH_DATA_DIR=/configured data\"\n"));
        assert!(service.contains(&format!("EnvironmentFile=-{}\n", env_file.display())));
    }

    #[test]
    fn service_options_set_manager_specific_directives() -> Result<()> {
        let settings = Settings::with_data_dir("/my data");
        let system = generate_service_with_options(
            &settings,
            None,
            None,
            None,
            &ServiceOptions {
                run_as: Some("tileharvester".into()),
                ..ServiceOptions::default()
            },
        )?;
        assert!(system.contains("ReadWritePaths=\"/my data\"\n"));
        assert!(system.contains("User=tileharvester\n"));
        let options = ServiceOptions {
            user: true,
            run_as: None,
        };
        let mut printed = Vec::new();
        write_service_with_options(&settings, None, None, 5, None, &options, &mut printed)?;
        let user = String::from_utf8(printed).unwrap();
        assert!(user.contains("NoNewPrivileges=true\n"));
        for directive in ["ReadWritePaths=", "ProtectSystem=", "PrivateTmp=", "User="] {
            assert!(!user.contains(directive));
        }
        assert!(user.contains("systemctl --user daemon-reload"));
        assert!(user.contains("systemctl --user enable --now tileharvester.timer"));
        assert!(user.contains("loginctl enable-linger \"$USER\""));
        Ok(())
    }

    #[test]
    fn service_options_reject_invalid_users() {
        let settings = Settings::default();
        for (user, run_as) in [
            (false, ""),
            (false, "x\nUser=root"),
            (false, "x\t"),
            (true, "alice"),
        ] {
            let options = ServiceOptions {
                user,
                run_as: Some(run_as.into()),
            };
            assert!(generate_service_with_options(&settings, None, None, None, &options).is_err());
        }
    }

    #[test]
    fn timer_template_uses_requested_interval() {
        assert_eq!(
            generate_timer(12),
            "[Unit]\nDescription=TileHarvester Sync Timer\n\n\
             [Timer]\nOnBootSec=2min\nOnUnitActiveSec=12min\nAccuracySec=1min\nPersistent=true\n\n\
             [Install]\nWantedBy=timers.target\n"
        );
    }
}
