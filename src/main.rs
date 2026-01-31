use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod protocol;
mod server;
mod client;

use server::ServerOpts;
use client::ClientOpts;

#[derive(Parser, Debug)]
#[command(name = "lan-speedtest")]
#[command(about = "LAN Speed Test - Server and Client", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the speed test server
    Server(ServerOpts),
    /// Run the speed test client
    Client(ClientOpts),
}

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Server(opts) => {
            server::run_server(opts).await;
        }
        Commands::Client(opts) => {
            client::run_client(opts).await;
        }
    }
}
