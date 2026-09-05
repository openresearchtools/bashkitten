use anyhow::{Context, Result, bail};
use bashkitten::config::{AppConfig, GpuLayers};
use bashkitten::models;
use bashkitten::paths::AppPaths;
use bashkitten::session::{self, ControlRequest, Delivery, NewSession};
use clap::{Args, Parser, Subcommand};
use serde_json::Value;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

#[derive(Parser)]
#[command(version, about = "Minimal standalone Rust coding agent")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Models {
        #[arg(long)]
        json: bool,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Send(SendArguments),
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Llama {
        #[command(subcommand)]
        command: LlamaCommand,
    },
    Web,
}

#[derive(Subcommand)]
enum SessionCommand {
    Start(StartArguments),
    List {
        #[arg(long)]
        json: bool,
    },
    Show {
        id: String,
        #[arg(long)]
        segment: Option<u32>,
    },
    Stop {
        id: String,
    },
    Model {
        id: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        thinking: String,
    },
}

#[derive(Args)]
struct StartArguments {
    #[arg(long)]
    parent: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    thinking: Option<String>,
    #[arg(long)]
    prompt: String,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long = "attach")]
    attachments: Vec<PathBuf>,
}

#[derive(Args)]
struct SendArguments {
    id: String,
    #[arg(long, conflicts_with = "queue")]
    steer: bool,
    #[arg(long, conflicts_with = "steer")]
    queue: bool,
    message: String,
    #[arg(long = "attach")]
    attachments: Vec<PathBuf>,
}

#[derive(Subcommand)]
enum AuthCommand {
    Status,
    ResetWeb,
}

#[derive(Subcommand)]
enum LlamaCommand {
    Serve,
    Status,
    Restart,
}

fn codex_authenticated(paths: &AppPaths) -> bool {
    fs::read(paths.provider_auth_file())
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("openai-codex").cloned())
        .is_some()
}

fn llama_available() -> bool {
    std::path::Path::new("/usr/bin/llama-server").is_file()
}

fn require_model(
    paths: &AppPaths,
    config: &AppConfig,
    full_id: &str,
    thinking: &str,
) -> Result<()> {
    let model = models::find_model(
        config,
        full_id,
        codex_authenticated(paths),
        llama_available(),
    )
    .with_context(|| format!("Unknown or unavailable model: {full_id}"))?;
    if !model.thinking_levels.iter().any(|level| level == thinking) {
        bail!("Thinking level {thinking} is not supported by {full_id}");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::discover()?;
    paths.ensure()?;
    let config = AppConfig::load(&paths)?;
    match cli.command {
        Commands::Models { json } => {
            let list = models::all_models(&config, codex_authenticated(&paths), llama_available());
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                for model in list {
                    println!(
                        "{}\t{}\tthinking={}\t{}",
                        model.full_id(),
                        if model.available {
                            "available"
                        } else {
                            "unavailable"
                        },
                        model.thinking_levels.join(","),
                        model.name
                    );
                }
            }
        }
        Commands::Session { command } => match command {
            SessionCommand::Start(args) => {
                let model = args.model.unwrap_or_else(|| config.default_model.clone());
                let thinking = args
                    .thinking
                    .unwrap_or_else(|| config.default_thinking.clone());
                require_model(&paths, &config, &model, &thinking)?;
                let cwd = args.cwd.unwrap_or_else(|| config.default_cwd.clone());
                let request = NewSession {
                    cwd,
                    model,
                    thinking,
                    prompt: args.prompt.clone(),
                    attachments: args.attachments.clone(),
                    parent: args.parent,
                };
                let id = session::create(&paths, &request)?;
                let attachments = session::copy_attachments(&paths, &id, &args.attachments)?;
                session::start_worker(&paths, &id)?;
                session::send(
                    &paths,
                    &id,
                    &ControlRequest::Send {
                        delivery: Delivery::Queue,
                        content: args.prompt,
                        attachments,
                        source_session: std::env::var("BASHKITTEN_SESSION_ID").ok(),
                    },
                )?;
                println!("{id}");
            }
            SessionCommand::List { json } => {
                let sessions = session::list(&paths)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&sessions)?);
                } else {
                    for item in sessions {
                        println!(
                            "{}\t{}\t{}",
                            item.id,
                            if item.running { "running" } else { "finished" },
                            item.title
                        );
                    }
                }
            }
            SessionCommand::Show { id, segment } => {
                let number = match segment {
                    Some(n) => n,
                    None => session::current_segment(&paths.session_dir(&id))?.0,
                };
                for entry in session::read_segment(&paths, &id, number)? {
                    println!("{}", serde_json::to_string(&entry)?);
                }
            }
            SessionCommand::Stop { id } => session::stop_worker(&paths, &id)?,
            SessionCommand::Model {
                id,
                model,
                thinking,
            } => {
                require_model(&paths, &config, &model, &thinking)?;
                let socket = session::control_socket(&paths, &id)?;
                if !session::socket_is_live(&socket) {
                    session::start_worker(&paths, &id)?;
                }
                let reply = session::send(
                    &paths,
                    &id,
                    &ControlRequest::ChangeModel { model, thinking },
                )?;
                println!("{}", reply.message);
            }
        },
        Commands::Send(args) => {
            let delivery = if args.steer {
                Delivery::Steer
            } else {
                Delivery::Queue
            };
            let attachments = session::copy_attachments(&paths, &args.id, &args.attachments)?;
            let socket = session::control_socket(&paths, &args.id)?;
            if !session::socket_is_live(&socket) {
                session::start_worker(&paths, &args.id)?;
            }
            let reply = session::send(
                &paths,
                &args.id,
                &ControlRequest::Send {
                    delivery,
                    content: args.message,
                    attachments,
                    source_session: std::env::var("BASHKITTEN_SESSION_ID").ok(),
                },
            )?;
            println!("{}", reply.message);
        }
        Commands::Auth { command } => match command {
            AuthCommand::Status => println!(
                "OpenAI subscription: {}",
                if codex_authenticated(&paths) {
                    "authenticated"
                } else {
                    "not authenticated"
                }
            ),
            AuthCommand::ResetWeb => {
                bashkitten::auth::reset(&paths)?;
                println!("Web UI user reset.");
            }
        },
        Commands::Llama { command } => match command {
            LlamaCommand::Serve => exec_llama(&config)?,
            LlamaCommand::Status => {
                let status = Command::new("systemctl")
                    .args(["--user", "is-active", "bashkitten-llama.service"])
                    .status()?;
                std::process::exit(if status.success() { 0 } else { 3 });
            }
            LlamaCommand::Restart => {
                let status = Command::new("systemctl")
                    .args(["--user", "restart", "bashkitten-llama.service"])
                    .status()?;
                if !status.success() {
                    bail!("Could not restart llama.cpp service");
                }
            }
        },
        Commands::Web => bashkitten::web::serve(paths, config).await?,
    }
    Ok(())
}

fn exec_llama(config: &AppConfig) -> Result<()> {
    if !config.llama.enabled {
        bail!("llama.cpp is disabled in BashKitten settings");
    }
    if !llama_available() {
        bail!("Neither llama-cpp nor llama-cpp-cuda is installed");
    }
    let mut command = Command::new("/usr/bin/llama-server");
    command
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            &config.llama.port.to_string(),
            "--models-dir",
        ])
        .arg(&config.llama.models_dir)
        .args([
            "--jinja",
            "--ctx-size",
            &config.llama.context_size.to_string(),
            "--batch-size",
            &config.llama.batch_size.to_string(),
            "--parallel",
            &config.llama.parallel_slots.to_string(),
        ]);
    if config.llama.cpu_threads > 0 {
        command.args(["--threads", &config.llama.cpu_threads.to_string()]);
    }
    let layers = match config.llama.gpu_layers {
        GpuLayers::Auto => 999,
        GpuLayers::Cpu => 0,
        GpuLayers::Count(n) => n,
    };
    command.args([
        "--gpu-layers",
        &layers.to_string(),
        "--flash-attn",
        if config.llama.flash_attention {
            "on"
        } else {
            "off"
        },
    ]);
    command.arg(if config.llama.mmap {
        "--mmap"
    } else {
        "--no-mmap"
    });
    if config.llama.mlock {
        command.arg("--mlock");
    }
    if !config.llama.api_key.is_empty() {
        command.args(["--api-key", &config.llama.api_key]);
    }
    command.args(&config.llama.extra_arguments);
    let error = command.exec();
    Err(error).context("execute llama-server")
}
