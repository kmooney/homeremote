//! Control a Samsung TV, a Denon AVR, and an Apple TV over IP.
//!
//! Built to replace a Logitech Harmony whose IR repeaters fire unreliably.
//! Every device here is already IP-addressable, so IR is the only unreliable
//! link — removing it is the whole point.
//!
//! # Layout
//!
//! - [`device`] — the command vocabulary every backend speaks.
//! - [`discovery`] — SSDP, mDNS, and Wake-on-LAN, so nobody types an IP.
//! - [`denon`], [`samsung`], [`appletv`] — the per-device backends.
//! - [`config`] — persisted device identities and pairing credentials.
//!
//! # Example
//!
//! ```no_run
//! # async fn run() -> homeremote::Result<()> {
//! let devices = homeremote::discover(homeremote::discovery::DEFAULT_TIMEOUT).await;
//! for device in devices {
//!     println!("{} at {}", device.name, device.addr);
//! }
//! # Ok(()) }
//! ```

use std::time::Duration;

pub mod config;
pub mod device;
pub mod discovery;
pub mod error;

#[cfg(feature = "appletv")]
pub mod appletv;
#[cfg(feature = "denon")]
pub mod denon;
#[cfg(feature = "samsung")]
pub mod samsung;

pub use device::{Command, Device, DeviceEvent, DeviceId, DeviceKind, DeviceState};
pub use discovery::{Discovered, MacAddr, Vendor};
pub use error::{Error, Result};

/// Devices are on the LAN and answer fast or not at all; a long timeout just
/// means waiting on something that is powered off.
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);

/// A shared HTTP client for device metadata endpoints.
///
/// Deliberately does not follow redirects or use a proxy: these are appliances
/// on the local network, and a system proxy would break discovery outright.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
}

/// Open a live connection to a configured device.
///
/// Returns immediately; backends connect in the background and reconnect on
/// loss, so this succeeding does not mean the device is currently reachable.
pub fn open(device: &config::DeviceConfig) -> Result<std::sync::Arc<dyn Device>> {
    let found = device.as_discovered()?;
    match device.vendor {
        #[cfg(feature = "denon")]
        Vendor::Denon => Ok(std::sync::Arc::new(denon::DenonReceiver::connect(&found))),
        #[cfg(not(feature = "denon"))]
        Vendor::Denon => Err(Error::Unsupported("Denon support is not compiled in")),
        Vendor::Samsung => Err(Error::Unsupported("Samsung control is not implemented yet")),
        Vendor::AppleTv => Err(Error::Unsupported("Apple TV control is not implemented yet")),
    }
}

/// Find every supported device on the network.
///
/// All three searches run concurrently, so the whole scan costs one `timeout`
/// rather than three. A backend that fails is logged and skipped rather than
/// failing the scan — finding two of three devices is far more useful than
/// finding none.
pub async fn discover(timeout: Duration) -> Vec<Discovered> {
    type Scan = std::pin::Pin<
        Box<dyn std::future::Future<Output = (&'static str, Result<Vec<Discovered>>)> + Send>,
    >;

    let mut scans: Vec<Scan> = Vec::new();
    #[cfg(feature = "denon")]
    scans.push(Box::pin(async move {
        ("Denon", denon::discover(timeout).await)
    }));
    #[cfg(feature = "samsung")]
    scans.push(Box::pin(async move {
        ("Samsung", samsung::discover(timeout).await)
    }));
    #[cfg(feature = "appletv")]
    scans.push(Box::pin(async move {
        ("Apple TV", appletv::discover(timeout).await)
    }));

    let mut all = Vec::new();
    for (backend, result) in futures_util::future::join_all(scans).await {
        match result {
            Ok(devices) => all.extend(devices),
            Err(e) => {
                tracing::warn!(backend, error = %e, "discovery failed for this backend");
            }
        }
    }
    all.sort_by(|a, b| a.id.cmp(&b.id));
    all
}
