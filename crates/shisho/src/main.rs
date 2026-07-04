mod commands;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use commands::encryption;

const DEFAULT_CONFIG_PATH: &str = "config/shisho.toml";

#[derive(Parser)]
#[command(
    name = "shisho",
    about = "A self-hosted library-aware manga recommendation service, exposed as a single remote MCP server",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the HTTP server
    Serve {
        #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// Run pending database migrations and exit
    Migrate {
        #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// Write the OpenAPI specification to a file
    Openapi {
        #[arg(short, long, default_value = "web/openapi.json")]
        output: PathBuf,
    },
    /// Manage at-rest database encryption keys
    Encryption {
        #[command(subcommand)]
        command: EncryptionCommands,
    },
    /// Run the MCP server over a transport
    Mcp {
        #[command(subcommand)]
        command: McpCommands,
    },
    /// Manage the MangaBaka offline metadata dump
    Mangabaka {
        #[command(subcommand)]
        command: MangabakaCommands,
    },
    /// Run a housekeeping sweep once and exit (the same jobs `serve` schedules)
    Sweep {
        #[command(subcommand)]
        command: SweepCommands,
    },
}

#[derive(Subcommand)]
enum SweepCommands {
    /// Reconcile verdict outcomes: resolve settled flags, flag started series
    Reconcile {
        #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// Scan warm library snapshots for series with unmappable external IDs
    Unresolvable {
        #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum MangabakaCommands {
    /// Download and ingest the offline dump (full pipeline, atomic promote)
    Refresh {
        #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum McpCommands {
    /// Serve the MCP protocol over stdin/stdout for a local client
    Stdio {
        #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum EncryptionCommands {
    /// Generate a fresh base64 KEK to paste into `encryption.key_vN`
    Gen,
    /// Re-wrap the data key under the current KEK version (`encryption.key_version`)
    RotateKek {
        #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// Report whether encryption is configured and which KEK version is active
    Status {
        #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { config } => commands::serve::run(config).await,
        Commands::Migrate { config } => commands::migrate::run(config).await,
        Commands::Openapi { output } => commands::openapi::run(&output),
        Commands::Encryption { command } => match command {
            EncryptionCommands::Gen => encryption::generate(),
            EncryptionCommands::RotateKek { config } => encryption::rotate_kek(config),
            EncryptionCommands::Status { config } => encryption::status(config),
        },
        Commands::Mcp { command } => match command {
            McpCommands::Stdio { config } => commands::mcp::stdio(config).await,
        },
        Commands::Mangabaka { command } => match command {
            MangabakaCommands::Refresh { config } => commands::mangabaka::refresh(config).await,
        },
        Commands::Sweep { command } => match command {
            SweepCommands::Reconcile { config } => commands::sweep::reconcile(config).await,
            SweepCommands::Unresolvable { config } => commands::sweep::unresolvable(config).await,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // Catches conflicting args, bad defaults, and malformed subcommands.
        Cli::command().debug_assert();
    }

    #[test]
    fn sweep_subcommands_parse_to_the_right_handlers() {
        match Cli::parse_from(["shisho", "sweep", "reconcile", "-c", "x.toml"]).command {
            Commands::Sweep {
                command: SweepCommands::Reconcile { config },
            } => assert_eq!(config, PathBuf::from("x.toml")),
            _ => panic!("expected `sweep reconcile`"),
        }
        match Cli::parse_from(["shisho", "sweep", "unresolvable"]).command {
            Commands::Sweep {
                command: SweepCommands::Unresolvable { config },
            } => assert_eq!(config, PathBuf::from(DEFAULT_CONFIG_PATH)),
            _ => panic!("expected `sweep unresolvable`"),
        }
    }
}
