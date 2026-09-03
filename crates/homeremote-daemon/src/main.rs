//! The Home Remote daemon and its CLI.
//!
//! The CLI exists so every protocol can be exercised against real hardware
//! before there is any UI to blame. `homeremote discover` is the first thing to
//! run on a new machine.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use homeremote::config::Config;
use homeremote::discovery::DEFAULT_TIMEOUT;

#[derive(Parser)]
#[command(name = "homeremote", version, about = "Control the TV, receiver, and Apple TV over IP")]
struct Cli {
    /// Override the config file location.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Print protocol-level detail.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scan the network for supported devices.
    Discover {
        /// How long to listen for replies.
        #[arg(long, default_value_t = DEFAULT_TIMEOUT.as_secs(), value_name = "SECONDS")]
        timeout: u64,

        /// Record what was found in the config file.
        #[arg(long)]
        save: bool,
    },
    /// Show the resolved config path and what's in it.
    Config,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let config_path = match &cli.config {
        Some(path) => path.clone(),
        None => Config::default_path().context("could not determine the config location")?,
    };

    match cli.command {
        Commands::Discover { timeout, save } => {
            discover(&config_path, Duration::from_secs(timeout), save).await
        }
        Commands::Config => show_config(&config_path),
    }
}

fn init_tracing(verbose: bool) {
    let default = if verbose { "homeremote=debug" } else { "homeremote=warn" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .init();
}

async fn discover(config_path: &std::path::Path, timeout: Duration, save: bool) -> Result<()> {
    eprintln!("Scanning for {timeout:.0?}...\n");
    let found = homeremote::discover(timeout).await;

    if found.is_empty() {
        eprintln!("No devices found.\n");
        eprintln!("Things worth checking:");
        eprintln!("  - the devices are powered on and on this network");
        eprintln!("  - this machine is on the same subnet (not a guest VLAN)");
        eprintln!("  - the firewall is allowing outbound multicast to 239.255.255.250");
        eprintln!("  - re-run with --verbose to see which interfaces were tried");
        return Ok(());
    }

    for device in &found {
        println!("{}", device.name);
        println!("  id      {}", device.id);
        println!("  kind    {:?}", device.kind());
        println!("  address {}", device.addr);
        if let Some(model) = &device.model {
            println!("  model   {model}");
        }
        if let Some(mac) = &device.mac {
            println!("  mac     {mac}");
        }
        println!();
    }
    println!("Found {} device(s).", found.len());

    if save {
        let mut config = Config::load(config_path)?;
        config.merge_discovered(&found);
        config.save(config_path)
            .with_context(|| format!("saving {}", config_path.display()))?;
        println!("Saved to {}", config_path.display());
    }
    Ok(())
}

fn show_config(config_path: &std::path::Path) -> Result<()> {
    println!("Config: {}", config_path.display());
    if !config_path.exists() {
        println!("(not created yet — run `homeremote discover --save`)");
        return Ok(());
    }
    let config = Config::load(config_path)?;
    println!("Devices:    {}", config.devices.len());
    println!("Activities: {}", config.activities.len());
    println!("\n{}", config.to_toml()?);
    Ok(())
}
