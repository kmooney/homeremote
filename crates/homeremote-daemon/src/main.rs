//! The Home Remote daemon and its CLI.
//!
//! The CLI exists so every protocol can be exercised against real hardware
//! before there is any UI to blame. `homeremote discover` is the first thing to
//! run on a new machine.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use homeremote::config::Config;
use homeremote::device::{Device, DeviceEvent};
use homeremote::discovery::DEFAULT_TIMEOUT;

mod activity;
mod api;
mod assets;
mod cli;
mod state;

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
    /// Send a command to a device.
    ///
    /// Examples:
    ///   homeremote send denon volume up
    ///   homeremote send denon volume 40%
    ///   homeremote send denon input MPLAY
    ///   homeremote send denon raw PSBAS UP
    #[command(verbatim_doc_comment)]
    Send {
        /// Device id, name, or vendor — any unambiguous fragment.
        device: String,
        /// The command, e.g. `volume up`.
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Print a device's state and follow it live.
    ///
    /// Changes made with the physical remote show up here too, which is the
    /// quickest way to confirm the event stream is working.
    #[command(verbatim_doc_comment)]
    Watch {
        /// Device id, name, or vendor.
        device: String,
        /// Stop after this long. Runs until interrupted if omitted.
        #[arg(long, value_name = "SECONDS")]
        seconds: Option<u64>,
    },
    /// Run the daemon and serve the remote to the network.
    Serve {
        /// Port to listen on.
        #[arg(long)]
        port: Option<u16>,
        /// Skip the discovery scan at startup and use saved addresses.
        #[arg(long)]
        no_scan: bool,
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
        Commands::Send { device, command } => send(&config_path, &device, &command).await,
        Commands::Watch { device, seconds } => watch(&config_path, &device, seconds).await,
        Commands::Serve { port, no_scan } => serve(&config_path, port, no_scan).await,
        Commands::Config => show_config(&config_path),
    }
}

/// Run the HTTP server until interrupted.
async fn serve(config_path: &std::path::Path, port: Option<u16>, no_scan: bool) -> Result<()> {
    let mut config = Config::load(config_path)?;
    let port = port.unwrap_or(config.server.port);

    // Re-resolve addresses at startup: a device that took a new DHCP lease
    // would otherwise be unreachable until someone noticed and rescanned.
    if !no_scan {
        eprintln!("Scanning for devices...");
        let found = homeremote::discover(DEFAULT_TIMEOUT).await;
        config.merge_discovered(&found);
        config.save(config_path)?;
        eprintln!("Found {} device(s).", found.len());
    }

    let state = state::AppState::new(config_path.to_path_buf(), config);
    state.open_devices().await;

    for snapshot in state.snapshots().await {
        match &snapshot.unavailable_because {
            Some(reason) => eprintln!("  {:<24} {}", snapshot.name, reason),
            None => eprintln!("  {:<24} ready", snapshot.name),
        }
    }

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("could not listen on port {port}"))?;

    eprintln!("\nRemote ready:");
    for addr in homeremote::discovery::local_ipv4_interfaces() {
        eprintln!("  http://{addr}:{port}");
    }
    eprintln!("  http://localhost:{port}");
    eprintln!("\nOpen one of these on a phone or laptop on the same network.");

    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("the server stopped unexpectedly")?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    eprintln!("\nShutting down.");
}

/// Open a saved device, waiting briefly for its first state to arrive.
///
/// Backends connect in the background, so acting the instant `open` returns
/// would race the connection and report "unreachable" for a device that is
/// simply still shaking hands.
async fn open(
    config_path: &std::path::Path,
    needle: &str,
) -> Result<std::sync::Arc<dyn Device>> {
    let config = Config::load(config_path)?;
    if config.devices.is_empty() {
        bail!(
            "no devices configured yet — run `homeremote discover --save` first\n\
             (config: {})",
            config_path.display()
        );
    }
    let entry = cli::resolve(&config, needle)?;
    let device = homeremote::open(entry)
        .with_context(|| format!("opening {}", entry.name))?;

    // Give the background connection a moment to land.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while tokio::time::Instant::now() < deadline && !device.state().await.reachable {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(device)
}

async fn send(config_path: &std::path::Path, needle: &str, words: &[String]) -> Result<()> {
    let command = cli::parse_command(words)?;
    let device = open(config_path, needle).await?;

    device
        .send(command.clone())
        .await
        .with_context(|| format!("sending {} to {}", command.describe(), device.name()))?;

    // The receiver reports the result of a command rather than acknowledging
    // it, so pause for the echo before printing what actually happened.
    tokio::time::sleep(Duration::from_millis(400)).await;
    print_state(device.as_ref()).await;
    Ok(())
}

async fn watch(config_path: &std::path::Path, needle: &str, seconds: Option<u64>) -> Result<()> {
    let device = open(config_path, needle).await?;
    let mut events = device.subscribe();
    print_state(device.as_ref()).await;
    eprintln!("\nWatching for changes — try the physical remote. Ctrl-C to stop.");

    let deadline = seconds.map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
    loop {
        let next = events.recv();
        let event = match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, next).await {
                Err(_) => return Ok(()),
                Ok(event) => event,
            },
            None => next.await,
        };
        match event {
            Ok(event) => println!("  {}", describe_event(&event)),
            // Lagged: a burst outran the buffer. Say so rather than pretending
            // the missed changes never happened.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                println!("  ... {n} change(s) missed");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

fn describe_event(event: &DeviceEvent) -> String {
    match event {
        DeviceEvent::Power(on) => format!("power    {}", if *on { "on" } else { "off" }),
        DeviceEvent::Volume(level) => format!("volume   {:.0}%", level * 100.0),
        DeviceEvent::Muted(muted) => format!("mute     {}", if *muted { "on" } else { "off" }),
        DeviceEvent::Input(input) => format!("input    {input}"),
        DeviceEvent::Reachability(up) => {
            format!("device   {}", if *up { "reachable" } else { "unreachable" })
        }
    }
}

async fn print_state(device: &dyn Device) {
    let state = device.state().await;
    println!("{}", device.name());
    if !state.reachable {
        println!("  unreachable");
        return;
    }
    let show = |label: &str, value: Option<String>| {
        if let Some(value) = value {
            println!("  {label:8} {value}");
        }
    };
    show("power", state.power.map(|p| if p { "on".into() } else { "off".into() }));
    show("volume", state.volume.map(|v| format!("{:.0}%", v * 100.0)));
    show("mute", state.muted.map(|m| if m { "on".into() } else { "off".into() }));
    show("input", state.input);
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
