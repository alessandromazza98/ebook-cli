mod api;
mod config;
mod core;
mod db;
mod epub;
mod error;
mod integrations;
mod selection;
mod torrent;
mod worker;

use clap::{Parser, Subcommand};
use error::{Error, Result};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "ebook",
    version,
    about = "Agent-first ebook search, download, repair and Kindle delivery"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "EBOOK_CONFIG",
        default_value = "config.json"
    )]
    config: PathBuf,
    #[arg(long, global = true)]
    human: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Write a private configuration template without overwriting existing files.
    Setup,
    /// Check MAM, qBittorrent and SMTP. Sends no email.
    Check {
        #[arg(long)]
        create_category: bool,
    },
    Search {
        query: String,
    },
    Select {
        release_id: String,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    Request {
        title: String,
        #[arg(long)]
        author: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    Jobs,
    Status {
        job_id: String,
    },
    Retry {
        job_id: String,
        #[arg(long)]
        acknowledge_possible_duplicate: bool,
    },
    ConfirmDelivery {
        job_id: String,
    },
    /// Foreground daemon; run under the supplied systemd unit.
    Worker {
        #[arg(long)]
        once: bool,
    },
    Prepare {
        input: PathBuf,
        output: PathBuf,
        #[arg(long)]
        language: String,
    },
    Serve {
        #[arg(long, default_value = "127.0.0.1:8791")]
        listen: std::net::SocketAddr,
    },
}
fn run(cli: &Cli) -> Result<Value> {
    if matches!(cli.command, Command::Setup) {
        config::Config::setup(&cli.config)?;
        return Ok(
            json!({"config_created":cli.config,"next":"Fill in the config, then run check --create-category"}),
        );
    }
    if let Command::Prepare {
        input,
        output,
        language,
    } = &cli.command
    {
        return epub::prepare(input, output, language);
    }
    let c = config::Config::load(&cli.config)?;
    let action = match &cli.command {
        Command::Search { query } => core::Action::Search {
            query: query.clone(),
        },
        Command::Select {
            release_id,
            language,
            idempotency_key,
        } => core::Action::Select {
            release_id: release_id.clone(),
            language: language.clone(),
            idempotency_key: idempotency_key
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        },
        Command::Request {
            title,
            author,
            language,
            idempotency_key,
        } => core::Action::Request {
            title: title.clone(),
            author: author.clone(),
            language: language.clone(),
            idempotency_key: idempotency_key
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        },
        Command::Jobs => core::Action::Jobs {},
        Command::Status { job_id } => core::Action::Status {
            job_id: job_id.clone(),
        },
        Command::Retry {
            job_id,
            acknowledge_possible_duplicate,
        } => core::Action::Retry {
            job_id: job_id.clone(),
            acknowledge_possible_duplicate: *acknowledge_possible_duplicate,
        },
        Command::ConfirmDelivery { job_id } => core::Action::ConfirmDelivery {
            job_id: job_id.clone(),
        },
        Command::Check { create_category } => {
            let mut checks = Vec::new();
            let qb = (|| {
                let qb = integrations::Qb::connect(&c)?;
                qb.category(*create_category)?;
                Ok::<_, Error>(())
            })();
            let mam = integrations::search(&c, "Heretics of Dune").map(|_| ());
            let mail = worker::smtp(&c)?
                .test_connection()
                .map_err(|_| Error::dependency("SMTP connection/authentication test"))
                .and_then(|ok| {
                    if ok {
                        Ok(())
                    } else {
                        Err(Error::dependency("SMTP NOOP"))
                    }
                });
            let mut failed = false;
            for (name, result) in [("qbittorrent", qb), ("mam", mam), ("smtp", mail)] {
                match result {
                    Ok(()) => checks.push(json!({"dependency":name,"ok":true})),
                    Err(e) => {
                        failed = true;
                        checks.push(json!({"dependency":name,"ok":false,"error":e}));
                    }
                }
            }
            if failed {
                return Err(Error::new(
                    "connection_check_failed",
                    serde_json::to_string(&checks)?,
                    4,
                ));
            }
            return Ok(
                json!({"checks":checks,"download_directory_exists":c.qbittorrent.local_dir.is_dir()}),
            );
        }
        Command::Worker { once } => {
            let _lock = worker::lock(&c)?;
            let db = db::Db::open(&c)?;
            db.recover()?;
            loop {
                let events = worker::tick(&c, &db)?;
                if *once {
                    return Ok(events);
                }
                if events["events"].as_array().is_some_and(|a| !a.is_empty()) {
                    eprintln!("{events}");
                }
                std::thread::sleep(std::time::Duration::from_secs(15));
            }
        }
        Command::Serve { listen } => {
            api::serve(c, *listen)?;
            return Ok(json!({"stopped":true}));
        }
        _ => unreachable!(),
    };
    core::execute(&c, action)
}
fn main() {
    // All newly created state, credentials and book copies are private.
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                print!("{e}");
                return;
            }
            println!(
                "{}",
                json!({"ok":false,"error":Error::invalid("Invalid command arguments; run ebook --help")})
            );
            std::process::exit(2);
        }
    };
    match run(&cli) {
        Ok(data) => {
            let clarify = data["status"] == "clarification_required";
            let v = json!({"ok":true,"data":data});
            if cli.human {
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!("{v}");
            }
            if clarify {
                std::process::exit(3);
            }
        }
        Err(e) => {
            let exit = e.exit;
            println!("{}", json!({"ok":false,"error":e}));
            std::process::exit(exit);
        }
    }
}

#[cfg(test)]
mod tests;
