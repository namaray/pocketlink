// SPDX-License-Identifier: GPL-3.0-or-later

//! What the daemon remembers between runs: our identity and the devices we trust.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use pocketlink_proto::Identity;
use rustls_pki_types::CertificateDer;
use serde::{Deserialize, Serialize};

/// A device we paired with. Its certificate is pinned: a device claiming the
/// same id with any other certificate is refused.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trusted {
    pub name: String,
    pub kind: String,
    pub protocol_version: i64,
    /// Base64 DER.
    pub certificate: String,
}

impl Trusted {
    pub fn cert(&self) -> Option<CertificateDer<'static>> {
        B64.decode(&self.certificate).ok().map(CertificateDer::from)
    }

    pub fn encode_cert(cert: &CertificateDer<'_>) -> String {
        B64.encode(cert)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Shown on the phone. Defaults to the hostname.
    pub name: Option<String>,
    /// Addresses to announce ourselves to directly (VPNs, networks that drop broadcasts).
    #[serde(default)]
    pub addresses: Vec<String>,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// `$POCKETLINK_CONFIG_DIR`, or `~/.config/pocketlink`.
    pub fn open() -> io::Result<Self> {
        let dir = match std::env::var_os("POCKETLINK_CONFIG_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::config_dir()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no config directory"))?
                .join("pocketlink"),
        };
        fs::create_dir_all(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        Ok(Self { dir })
    }

    pub fn identity(&self) -> io::Result<Identity> {
        let cert_path = self.dir.join("certificate.pem");
        let key_path = self.dir.join("private-key.pem");
        if let (Ok(cert), Ok(key)) = (fs::read_to_string(&cert_path), fs::read_to_string(&key_path)) {
            if let Some(identity) = Identity::from_pem(&cert, &key) {
                return Ok(identity);
            }
            tracing::warn!("stored identity is unreadable, creating a new one");
        }

        let device_id = uuid::Uuid::new_v4().simple().to_string();
        let identity = Identity::generate(&device_id).map_err(io::Error::other)?;
        write_private(&key_path, identity.key_pem().as_bytes())?;
        fs::write(&cert_path, identity.cert_pem())?;
        tracing::info!(%device_id, "created a new identity");
        Ok(identity)
    }

    pub fn trusted(&self) -> HashMap<String, Trusted> {
        self.read_json("trusted.json").unwrap_or_default()
    }

    pub fn save_trusted(&self, trusted: &HashMap<String, Trusted>) {
        self.write_json("trusted.json", trusted);
    }

    pub fn settings(&self) -> Settings {
        self.read_json("settings.json").unwrap_or_default()
    }

    pub fn save_settings(&self, settings: &Settings) {
        self.write_json("settings.json", settings);
    }

    fn read_json<T: for<'de> Deserialize<'de>>(&self, name: &str) -> Option<T> {
        let text = fs::read_to_string(self.dir.join(name)).ok()?;
        serde_json::from_str(&text)
            .inspect_err(|err| tracing::warn!(%name, %err, "ignoring unreadable file"))
            .ok()
    }

    fn write_json<T: Serialize>(&self, name: &str, value: &T) {
        let path = self.dir.join(name);
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_vec_pretty(value).expect("settings serialize");
        let result = write_private(&tmp, &text).and_then(|()| fs::rename(&tmp, &path));
        if let Err(err) = result {
            tracing::error!(?path, %err, "couldn't save");
        }
    }
}

fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data)
}

/// This machine's name, for the phone's device list.
pub fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_owned())
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "Pocketlink".to_owned())
}

/// "laptop" when there's a battery, otherwise "desktop".
pub fn device_kind() -> &'static str {
    let has_battery = fs::read_dir("/sys/class/power_supply")
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("BAT"))
        })
        .unwrap_or(false);
    if has_battery { "laptop" } else { "desktop" }
}

/// `$POCKETLINK_DOWNLOAD_DIR`, or the user's Downloads folder.
pub fn download_dir() -> PathBuf {
    std::env::var_os("POCKETLINK_DOWNLOAD_DIR")
        .map(PathBuf::from)
        .or_else(dirs::download_dir)
        .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
        .unwrap_or_else(|| PathBuf::from("."))
}
