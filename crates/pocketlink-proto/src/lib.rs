// SPDX-License-Identifier: GPL-3.0-or-later

//! The KDE Connect protocol (version 8), implemented from scratch.
//!
//! Pocketlink speaks the same protocol as KDE Connect so it works with the
//! existing phone apps, but shares no code with it.

pub mod bluetooth;
pub mod identity;
pub mod lan;
pub mod mux;
pub mod packet;
pub mod payload;
pub mod tls;

pub use identity::{DeviceInfo, Identity, PROTOCOL_VERSION};
pub use lan::{Connection, Lan, LanConfig};
pub use packet::Packet;
pub use tls::TlsConfigs;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("TLS: {0}")]
    Tls(#[from] rustls::Error),
    #[error("protocol error: {0}")]
    Protocol(&'static str),
    #[error("the other device took too long")]
    Timeout,
    #[error("the other device isn't the one we paired with")]
    Untrusted,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
