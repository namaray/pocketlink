// SPDX-License-Identifier: GPL-3.0-or-later

//! Who a device is: its identity packet and its TLS certificate.

use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::packet::{self, Packet};

/// Version 8 sends the identity a second time after TLS is up, and only that copy is trusted.
pub const PROTOCOL_VERSION: i64 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    /// "phone", "tablet", "desktop", "laptop", "tv".
    pub kind: String,
    pub protocol_version: i64,
    pub incoming: Vec<String>,
    pub outgoing: Vec<String>,
}

impl DeviceInfo {
    pub fn to_packet(&self) -> Packet {
        Packet::new(
            packet::IDENTITY,
            json!({
                "deviceId": self.id,
                "deviceName": self.name,
                "deviceType": self.kind,
                "protocolVersion": self.protocol_version,
                "incomingCapabilities": self.incoming,
                "outgoingCapabilities": self.outgoing,
            }),
        )
    }

    pub fn from_packet(packet: &Packet) -> Option<Self> {
        if !packet.is(packet::IDENTITY) {
            return None;
        }
        let id = packet.get_str("deviceId")?;
        let name = filter_name(packet.get_str("deviceName").unwrap_or_default());
        if !is_valid_device_id(id) || name.trim().is_empty() {
            return None;
        }
        Some(Self {
            id: id.to_owned(),
            name,
            kind: packet.get_str("deviceType").unwrap_or("unknown").to_owned(),
            protocol_version: packet.get_i64("protocolVersion").unwrap_or_default(),
            incoming: packet.get_str_list("incomingCapabilities"),
            outgoing: packet.get_str_list("outgoingCapabilities"),
        })
    }
}

/// Same rule as the official apps: 32–38 of `[a-zA-Z0-9_-]`.
pub fn is_valid_device_id(id: &str) -> bool {
    (32..=38).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Same rule as the official apps: no punctuation that breaks their UIs, max 32 chars.
pub fn filter_name(name: &str) -> String {
    name.chars()
        .filter(|c| !"\"',;:.!?()[]<>".contains(*c))
        .take(32)
        .collect()
}

/// Our long-lived TLS identity. The certificate's common name is the device id.
#[derive(Clone)]
pub struct Identity {
    pub device_id: String,
    pub cert: CertificateDer<'static>,
    key: Vec<u8>,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// A new EC P-256 key and self-signed certificate, like current KDE Connect.
    pub fn generate(device_id: &str) -> Result<Self, rcgen::Error> {
        let key = rcgen::KeyPair::generate()?;
        let mut params = rcgen::CertificateParams::default();
        let mut dn = rcgen::DistinguishedName::new();
        dn.push(rcgen::DnType::CommonName, device_id);
        dn.push(rcgen::DnType::OrganizationName, "Pocketlink");
        dn.push(rcgen::DnType::OrganizationalUnitName, "Pocketlink");
        params.distinguished_name = dn;
        params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        params.not_after = rcgen::date_time_ymd(2049, 12, 31);
        let cert = params.self_signed(&key)?;
        Ok(Self {
            device_id: device_id.to_owned(),
            cert: cert.der().clone(),
            key: key.serialize_der(),
        })
    }

    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Option<Self> {
        let cert = CertificateDer::from_pem_slice(cert_pem.as_bytes()).ok()?;
        let key = PrivatePkcs8KeyDer::from_pem_slice(key_pem.as_bytes()).ok()?;
        Some(Self {
            device_id: common_name(&cert)?,
            cert,
            key: key.secret_pkcs8_der().to_vec(),
        })
    }

    pub fn cert_pem(&self) -> String {
        pem("CERTIFICATE", &self.cert)
    }

    pub fn key_pem(&self) -> String {
        pem("PRIVATE KEY", &self.key)
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone()))
    }
}

/// The certificate's subject common name, which by convention is the device id.
pub fn common_name(cert_der: &[u8]) -> Option<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der).ok()?;
    let cn = cert.subject().iter_common_name().next()?;
    cn.as_str().ok().map(str::to_owned)
}

fn public_key_der(cert_der: &[u8]) -> Option<Vec<u8>> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der).ok()?;
    Some(cert.public_key().raw.to_vec())
}

/// The 8-character code both screens show while pairing, so people can check
/// nobody is in the middle. Matches KDE Connect's algorithm.
pub fn verification_key(our_cert: &[u8], their_cert: &[u8], timestamp: i64) -> Option<String> {
    let mut a = public_key_der(our_cert)?;
    let mut b = public_key_der(their_cert)?;
    if a < b {
        std::mem::swap(&mut a, &mut b);
    }
    let mut hash = Sha256::new();
    hash.update(&a);
    hash.update(&b);
    hash.update(timestamp.to_string().as_bytes());
    Some(hex::encode_upper(hash.finalize())[..8].to_owned())
}

fn pem(label: &str, der: &[u8]) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = b64
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).expect("base64 is ascii"))
        .collect();
    format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", lines.join("\n"))
}

/// SHA-256 fingerprint, for display.
pub fn fingerprint(cert_der: &[u8]) -> String {
    Sha256::digest(cert_der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids() {
        assert!(is_valid_device_id("22c2fbc5c30044c38fc27699bca7eed6"));
        assert!(!is_valid_device_id("short"));
        assert!(!is_valid_device_id("22c2fbc5c30044c38fc27699bca7eed6!"));
    }

    #[test]
    fn names() {
        assert_eq!(filter_name("Angkon's PC (work)"), "Angkons PC work");
    }

    #[test]
    fn identity_round_trip() {
        let id = "0123456789abcdef0123456789abcdef";
        let identity = Identity::generate(id).unwrap();
        assert_eq!(common_name(&identity.cert).as_deref(), Some(id));
        let again = Identity::from_pem(&identity.cert_pem(), &identity.key_pem()).unwrap();
        assert_eq!(again.cert, identity.cert);
        assert_eq!(again.device_id, id);
    }

    #[test]
    fn verification_key_is_symmetric() {
        let a = Identity::generate("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let b = Identity::generate("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let ab = verification_key(&a.cert, &b.cert, 1_700_000_000).unwrap();
        let ba = verification_key(&b.cert, &a.cert, 1_700_000_000).unwrap();
        assert_eq!(ab, ba);
        assert_eq!(ab.len(), 8);
    }

    #[test]
    fn identity_packet() {
        let info = DeviceInfo {
            id: "0123456789abcdef0123456789abcdef".into(),
            name: "My PC".into(),
            kind: "desktop".into(),
            protocol_version: PROTOCOL_VERSION,
            incoming: vec!["kdeconnect.ping".into()],
            outgoing: vec![],
        };
        let parsed = DeviceInfo::from_packet(&Packet::from_line(&info.to_packet().to_line()).unwrap());
        assert_eq!(parsed, Some(info));
    }
}
