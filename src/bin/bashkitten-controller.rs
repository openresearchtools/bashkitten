use anyhow::{Context, Result, bail};
use bashkitten::config::AppConfig;
use bashkitten::paths::AppPaths;
use gtk4::glib;
use gtk4::prelude::*;
use std::process::Command;

fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("run systemctl")?;
    if !status.success() {
        bail!("systemctl {} failed: {status}", args.join(" "));
    }
    Ok(())
}

fn save_settings(paths: &AppPaths, startup: bool, restart: bool, port: u16) -> Result<()> {
    let mut config = AppConfig::load(paths)?;
    let old_port = config.web_port;
    if port != old_port {
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .with_context(|| format!("Port {port} is unavailable"))?;
    }
    let service = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "bashkitten-web.service",
            "--property=Id",
            "--value",
        ])
        .output()
        .context("resolve Web UI service")?;
    let service_name = String::from_utf8(service.stdout)?.trim().to_owned();
    if !service.status.success()
        || !service_name.ends_with(".service")
        || service_name.contains('/')
    {
        bail!("Could not resolve the Web UI service");
    }
    let user_config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        })
        .context("User configuration directory is unavailable")?;
    let dropin = user_config
        .join("systemd/user")
        .join(format!("{service_name}.d/restart.conf"));
    config.start_at_login = startup;
    config.web_restart_on_failure = restart;
    config.web_port = port;
    config.save(paths)?;
    bashkitten::config::atomic_private_bytes(
        &dropin,
        format!(
            "[Service]\nRestart={}\n",
            if restart { "on-failure" } else { "no" }
        )
        .as_bytes(),
    )?;
    systemctl(&[
        if startup { "enable" } else { "disable" },
        "bashkitten-controller.service",
    ])?;
    systemctl(&["daemon-reload"])?;
    if old_port != port {
        systemctl(&["restart", "bashkitten-web.service"])?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let app = gtk4::Application::builder()
        .application_id("org.openresearchtools.BashKitten")
        .build();
    if !std::env::args().any(|argument| argument == "--service") {
        systemctl(&["start", "bashkitten-controller.service"])?;
        app.register(None::<&gtk4::gio::Cancellable>)?;
        if !app.is_remote() {
            bail!("The supervised BashKitten controller did not register its GTK application");
        }
        app.activate();
        return Ok(());
    }
    let paths = AppPaths::discover()?;
    paths.ensure()?;
    systemctl(&["start", "bashkitten.target"])?;
    app.connect_activate(move |app| build_window(app, paths.clone()));
    app.connect_shutdown(|_| {
        if let Err(error) = systemctl(&["stop", "bashkitten.target"]) {
            eprintln!("Could not stop BashKitten: {error}");
        }
    });
    let app_for_signal = app.clone();
    glib::unix_signal_add_local(libc::SIGTERM, move || {
        app_for_signal.quit();
        glib::ControlFlow::Break
    });
    let exit = app.run_with_args(&["bashkitten-controller"]);
    if exit != glib::ExitCode::SUCCESS {
        bail!("GTK controller exited with {exit:?}");
    }
    Ok(())
}

fn build_window(app: &gtk4::Application, paths: AppPaths) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }
    let config = AppConfig::load(&paths).unwrap_or_default();
    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .title("BashKitten Settings")
        .default_width(440)
        .default_height(390)
        .build();
    let panel = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    panel.set_margin_top(20);
    panel.set_margin_bottom(20);
    panel.set_margin_start(20);
    panel.set_margin_end(20);
    let title = gtk4::Label::new(Some("🐈 BashKitten"));
    title.add_css_class("title-1");
    panel.append(&title);
    let startup = gtk4::Switch::builder()
        .active(config.start_at_login)
        .halign(gtk4::Align::End)
        .build();
    let restart = gtk4::Switch::builder()
        .active(config.web_restart_on_failure)
        .halign(gtk4::Align::End)
        .build();
    let port = gtk4::SpinButton::with_range(1024.0, 65535.0, 1.0);
    port.set_value(config.web_port as f64);
    panel.append(&setting_row("Start at login", &startup));
    panel.append(&setting_row("Restart Web UI after a crash", &restart));
    panel.append(&setting_row("Web UI port", &port));
    let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let open = gtk4::Button::with_label("Open Web UI");
    let reset = gtk4::Button::with_label("Reset Web user");
    let about = gtk4::Button::with_label("About / License");
    actions.append(&open);
    actions.append(&reset);
    actions.append(&about);
    panel.append(&actions);
    let save = gtk4::Button::with_label("Save settings");
    save.add_css_class("suggested-action");
    panel.append(&save);
    let status = gtk4::Label::new(None);
    status.set_wrap(true);
    status.set_xalign(0.0);
    panel.append(&status);
    let quit = gtk4::Button::with_label("Quit BashKitten");
    quit.add_css_class("destructive-action");
    panel.append(&quit);
    window.set_child(Some(&panel));

    let path_for_save = paths.clone();
    let startup_for_save = startup.clone();
    let restart_for_save = restart.clone();
    let port_for_save = port.clone();
    let save_status = status.clone();
    save.connect_clicked(move |_| {
        match save_settings(
            &path_for_save,
            startup_for_save.is_active(),
            restart_for_save.is_active(),
            port_for_save.value_as_int() as u16,
        ) {
            Ok(()) => save_status.set_text("Settings saved."),
            Err(error) => save_status.set_text(&format!("Could not save settings: {error:#}")),
        }
    });
    let open_paths = paths.clone();
    let open_status = status.clone();
    open.connect_clicked(move |_| {
        let result = (|| -> Result<()> {
            let config = AppConfig::load(&open_paths)?;
            systemctl(&["start", "bashkitten-web.service"])?;
            Command::new("xdg-open")
                .arg(format!("http://127.0.0.1:{}", config.web_port))
                .spawn()?;
            Ok(())
        })();
        if let Err(error) = result {
            open_status.set_text(&format!("Could not open Web UI: {error:#}"));
        }
    });
    let reset_paths = paths.clone();
    reset.connect_clicked(move |_| match bashkitten::auth::reset(&reset_paths) {
        Ok(()) => status.set_text("Web user reset. The next visit will show signup."),
        Err(error) => status.set_text(&format!("Could not reset Web user: {error:#}")),
    });
    let about_parent = window.clone();
    about.connect_clicked(move |_| {
        let dialog = gtk4::AboutDialog::builder()
            .transient_for(&about_parent)
            .modal(true)
            .program_name("BashKitten")
            .version(bashkitten::VERSION)
            .comments("Minimal standalone Rust coding agent")
            .website("https://github.com/openresearchtools/bashkitten")
            .license_type(gtk4::License::Apache20)
            .build();
        dialog.present();
    });
    let app_for_quit = app.clone();
    quit.connect_clicked(move |_| {
        app_for_quit.quit();
    });
    let app_for_close = app.clone();
    window.connect_close_request(move |_| {
        app_for_close.quit();
        glib::Propagation::Proceed
    });
    window.present();
}

fn setting_row(label: &str, control: &impl IsA<gtk4::Widget>) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    let text = gtk4::Label::new(Some(label));
    text.set_hexpand(true);
    text.set_halign(gtk4::Align::Start);
    row.append(&text);
    row.append(control);
    row
}
