use std::net::IpAddr;

/// Everything that can go wrong talking to a device.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("device {0} is not reachable")]
    Unreachable(IpAddr),

    #[error("device does not support this command: {0}")]
    Unsupported(&'static str),

    #[error("{0} control is not implemented yet")]
    NotImplemented(&'static str),

    /// The device is reachable but offers nothing to control it with.
    #[error("no IP control available — {0}")]
    NoIpControl(&'static str),

    #[error("device is not paired; run pairing first")]
    NotPaired,

    #[error("pairing was rejected on the device")]
    PairingRejected,

    #[error("timed out waiting for the device to respond")]
    Timeout,

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("config error: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
