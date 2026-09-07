//! Controller settings plumbing. systemd remains the lifecycle authority.
use crate::config::{AppConfig, PrivateFileSnapshot, atomic_private_bytes};
use crate::paths::AppPaths;
use anyhow::{Context, Result, bail};
use std::path::Path;

pub fn save_settings(paths: &AppPaths, startup: bool, restart: bool, port: u16) -> Result<()> {
    let user_config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|value| std::path::PathBuf::from(value).join(".config"))
        })
        .context("User configuration directory is unavailable")?;
    save_settings_with(paths, &user_config, startup, restart, port, &mut |args| {
        let output = std::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .context("run systemctl")?;
        if !output.status.success() {
            bail!("systemctl {} failed: {}", args.join(" "), output.status);
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    })
}

fn save_settings_with(
    paths: &AppPaths,
    user_config: &Path,
    startup: bool,
    restart: bool,
    port: u16,
    systemctl: &mut impl FnMut(&[&str]) -> Result<String>,
) -> Result<()> {
    let mut config = AppConfig::load(paths)?;
    let old_port = config.web_port;
    if port != old_port {
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .with_context(|| format!("Port {port} is unavailable"))?;
    }
    let service_name = systemctl(&["show", "bashkitten-web.service", "--property=Id", "--value"])?;
    if !service_name.ends_with(".service") || service_name.contains(['/', '\n', '\r']) {
        bail!("Could not resolve the Web UI service");
    }
    let enabled = systemctl(&[
        "show",
        "bashkitten-controller.service",
        "--property=UnitFileState",
        "--value",
    ])?;
    let old_enabled = matches!(enabled.as_str(), "enabled" | "enabled-runtime");
    let old_active = systemctl(&[
        "show",
        "bashkitten-web.service",
        "--property=ActiveState",
        "--value",
    ])?;
    let dropin = user_config
        .join("systemd/user")
        .join(format!("{service_name}.d/restart.conf"));
    let previous_config = PrivateFileSnapshot::read(&paths.config_file())?;
    let previous_preset = PrivateFileSnapshot::read(&paths.config.join("llama-models.ini"))?;
    let previous_dropin = PrivateFileSnapshot::read(&dropin)?;
    config.start_at_login = startup;
    config.web_restart_on_failure = restart;
    config.web_port = port;
    config.web_port_fallback = (old_port != port).then_some(old_port);
    let mut enable_attempted = false;
    let mut restart_attempted = false;
    let result = (|| -> Result<()> {
        config.save(paths)?;
        atomic_private_bytes(
            &dropin,
            format!(
                "[Service]\nRestart={}\n",
                if restart { "on-failure" } else { "no" }
            )
            .as_bytes(),
        )?;
        enable_attempted = true;
        systemctl(&[
            if startup { "enable" } else { "disable" },
            "bashkitten-controller.service",
        ])?;
        systemctl(&["daemon-reload"])?;
        if old_port != port {
            restart_attempted = true;
            systemctl(&["restart", "bashkitten-web.service"])?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let mut failures = Vec::new();
        for (label, snapshot) in [
            ("configuration", &previous_config),
            ("model presets", &previous_preset),
            ("Web restart policy", &previous_dropin),
        ] {
            if let Err(error) = snapshot.restore() {
                failures.push(format!("{label}: {error:#}"));
            }
        }
        // Attempt every rollback stage even when an earlier one fails. Never
        // change the target, active controller, agent services or router service.
        if enable_attempted {
            let args = if enabled == "enabled-runtime" {
                // A successful persistent enable may have preceded a later
                // failure. Remove that link before restoring runtime-only state.
                if let Err(error) = systemctl(&["disable", "bashkitten-controller.service"]) {
                    failures.push(format!("login startup: {error:#}"));
                }
                vec!["enable", "--runtime", "bashkitten-controller.service"]
            } else {
                vec![
                    if old_enabled { "enable" } else { "disable" },
                    "bashkitten-controller.service",
                ]
            };
            if let Err(error) = systemctl(&args) {
                failures.push(format!("login startup: {error:#}"));
            }
        }
        // systemctl enable/disable may reload internally before returning an
        // error, so reload restored files after any enablement attempt too.
        if enable_attempted && let Err(error) = systemctl(&["daemon-reload"]) {
            failures.push(format!("service reload: {error:#}"));
        }
        if restart_attempted {
            let action = if matches!(old_active.as_str(), "active" | "activating" | "reloading") {
                "restart"
            } else {
                "stop"
            };
            if let Err(error) = systemctl(&[action, "bashkitten-web.service"]) {
                failures.push(format!("Web service: {error:#}"));
            }
        }
        if !failures.is_empty() {
            return Err(error.context(format!(
                "Settings rollback failed ({})",
                failures.join("; ")
            )));
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn failed_settings_apply_restores_bytes_and_only_the_web_service() {
        for failed_step in ["enable", "daemon-reload", "restart"] {
            for originally_active in [true, false] {
                let root = tempfile::tempdir().unwrap();
                let paths = AppPaths {
                    config: root.path().join("config"),
                    data: root.path().join("data"),
                    runtime: root.path().join("runtime"),
                };
                paths.ensure().unwrap();
                AppConfig::default().save(&paths).unwrap();
                let config_before = std::fs::read(paths.config_file()).unwrap();
                let user_config = root.path().join("xdg");
                let dropin = user_config.join("systemd/user/fixture-web.service.d/restart.conf");
                atomic_private_bytes(&dropin, b"# old policy\n[Service]\nRestart=on-failure\n")
                    .unwrap();
                let dropin_before = std::fs::read(&dropin).unwrap();
                let port = std::net::TcpListener::bind("127.0.0.1:0")
                    .unwrap()
                    .local_addr()
                    .unwrap()
                    .port();
                let commands = RefCell::new(Vec::<Vec<String>>::new());
                let mut failed = false;
                let error =
                    save_settings_with(&paths, &user_config, true, false, port, &mut |args| {
                        if args[0] == "show" {
                            return Ok(match args[2] {
                                "--property=Id" => "fixture-web.service",
                                "--property=UnitFileState" => "disabled",
                                _ if originally_active => "active",
                                _ => "inactive",
                            }
                            .into());
                        }
                        commands
                            .borrow_mut()
                            .push(args.iter().map(|s| s.to_string()).collect());
                        if args[0] == failed_step && !failed {
                            failed = true;
                            bail!("injected {failed_step} failure");
                        }
                        // A compensating restart must observe the old files.
                        if failed && args[0] == "restart" {
                            assert_eq!(std::fs::read(paths.config_file()).unwrap(), config_before);
                            assert_eq!(std::fs::read(&dropin).unwrap(), dropin_before);
                        }
                        Ok(String::new())
                    })
                    .unwrap_err();
                assert!(error.to_string().contains(failed_step));
                assert_eq!(std::fs::read(paths.config_file()).unwrap(), config_before);
                assert_eq!(std::fs::read(&dropin).unwrap(), dropin_before);
                let commands = commands.into_inner();
                assert!(
                    commands
                        .iter()
                        .any(|c| c == &["disable", "bashkitten-controller.service"])
                );
                if failed_step == "restart" {
                    assert_eq!(
                        commands.last().unwrap(),
                        &[
                            if originally_active { "restart" } else { "stop" },
                            "bashkitten-web.service"
                        ]
                    );
                }
                assert!(commands.iter().flatten().all(|s| !s.contains("target")
                    && !s.contains("agent")
                    && !s.contains("llama")));
            }
        }
    }

    #[test]
    fn rollback_failures_are_reported_and_missing_dropin_stays_absent() {
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            runtime: root.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let config = AppConfig::default();
        config.save(&paths).unwrap();
        let user_config = root.path().join("xdg");
        let error = save_settings_with(
            &paths,
            &user_config,
            true,
            false,
            config.web_port,
            &mut |args| {
                if args[0] == "show" {
                    return Ok(match args[2] {
                        "--property=Id" => "fixture-web.service",
                        "--property=UnitFileState" => "disabled",
                        _ => "active",
                    }
                    .into());
                }
                bail!("injected systemctl {}", args[0]);
            },
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("injected systemctl enable"));
        assert!(message.contains("rollback failed"));
        assert!(message.contains("injected systemctl disable"));
        assert!(
            !user_config
                .join("systemd/user/fixture-web.service.d/restart.conf")
                .exists()
        );
        assert!(!AppConfig::load(&paths).unwrap().start_at_login);
    }
}
