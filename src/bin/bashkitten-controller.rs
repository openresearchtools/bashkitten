use anyhow::Result;
use bashkitten::config::AppConfig;
use bashkitten::paths::AppPaths;
use gtk4::glib;
use gtk4::prelude::*;
use std::fs;
use std::process::Command;

fn systemctl(args: &[&str]) {
    let _ = Command::new("systemctl").arg("--user").args(args).status();
}

fn main() -> Result<()> {
    let paths = AppPaths::discover()?;
    paths.ensure()?;
    systemctl(&["start", "bashkitten.target"]);
    let app = gtk4::Application::builder()
        .application_id("org.openresearchtools.BashKitten")
        .build();
    app.connect_activate(move |app| build_window(app, paths.clone()));
    app.run();
    Ok(())
}

fn build_window(app: &gtk4::Application, paths: AppPaths) {
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
    let quit = gtk4::Button::with_label("Quit BashKitten");
    quit.add_css_class("destructive-action");
    panel.append(&quit);
    window.set_child(Some(&panel));

    let path_for_save = paths.clone();
    let startup_for_save = startup.clone();
    let restart_for_save = restart.clone();
    let port_for_save = port.clone();
    save.connect_clicked(move |_| {
        if let Ok(mut config) = AppConfig::load(&path_for_save) {
            let old_port = config.web_port;
            config.start_at_login = startup_for_save.is_active();
            config.web_restart_on_failure = restart_for_save.is_active();
            config.web_port = port_for_save.value_as_int() as u16;
            let _ = config.save(&path_for_save);
            if config.start_at_login {
                systemctl(&["enable", "bashkitten-controller.service"]);
            } else {
                systemctl(&["disable", "bashkitten-controller.service"]);
            }
            let dropin = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| path_for_save.config.clone())
                .join(".config/systemd/user/bashkitten-web.service.d");
            let _ = fs::create_dir_all(&dropin);
            let _ = fs::write(
                dropin.join("restart.conf"),
                format!(
                    "[Service]\nRestart={}\n",
                    if config.web_restart_on_failure {
                        "on-failure"
                    } else {
                        "no"
                    }
                ),
            );
            systemctl(&["daemon-reload"]);
            if old_port != config.web_port {
                systemctl(&["restart", "bashkitten-web.service"]);
            }
        }
    });
    let open_config = config.clone();
    open.connect_clicked(move |_| {
        let _ = Command::new("xdg-open")
            .arg(format!("http://127.0.0.1:{}", open_config.web_port))
            .spawn();
    });
    let reset_paths = paths.clone();
    reset.connect_clicked(move |_| {
        let _ = bashkitten::auth::reset(&reset_paths);
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
        systemctl(&["stop", "bashkitten.target"]);
        app_for_quit.quit();
    });
    let app_for_close = app.clone();
    window.connect_close_request(move |_| {
        systemctl(&["stop", "bashkitten.target"]);
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
