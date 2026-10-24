// SPDX-License-Identifier: GPL-3.0-or-later

//! Bluetooth transport: RFCOMM plus the multiplexer.
//!
//! There's no TLS over Bluetooth. The link is encrypted and authenticated by
//! Bluetooth pairing (bonding) instead, and identity packets carry the
//! certificate so it can be checked against the one pinned when pairing.
//!
//! Handshake: whoever *accepted* the RFCOMM connection sends its identity
//! first, the connecting side answers with its own.

use std::time::Duration;

use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::timeout;
use uuid::Uuid;

use crate::identity::{self, DeviceInfo, Identity};
use crate::mux::{Channel, LineReader, Mux};
use crate::packet::Packet;
use crate::{Error, Result};

/// The SDP service both KDE Connect and Pocketlink register.
pub const SERVICE_UUID: &str = "185f3df4-3268-4e3f-9fca-d4d5059915bd";
pub const SERVICE_NAME: &str = "KDE Connect";

const MAX_PACKET_SIZE: usize = 32 * 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// They connected to us; we speak first.
    Accepted,
    /// We connected to them; they speak first.
    Connected,
}

/// A Bluetooth link after the identity exchange.
pub struct BtConnection {
    pub info: DeviceInfo,
    pub cert: CertificateDer<'static>,
    pub mux: Mux,
    pub reader: LineReader,
    pub writer: Channel,
}

pub async fn handshake<S>(stream: S, role: Role, me: &DeviceInfo, identity: &Identity) -> Result<BtConnection>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let mux = Mux::new(stream);
    let writer = mux.default_channel();
    let mut reader = LineReader::new(mux.default_channel());

    let mut hello = me.to_packet();
    hello.set("certificate", identity.cert_pem());

    let exchange = async {
        if role == Role::Accepted {
            writer.write_all(&hello.to_line()).await?;
        }
        let line = reader
            .read_line(MAX_PACKET_SIZE)
            .await?
            .ok_or(Error::Protocol("closed before sending identity"))?;
        if role == Role::Connected {
            writer.write_all(&hello.to_line()).await?;
        }
        Ok::<_, Error>(line)
    };
    let line = match timeout(HANDSHAKE_TIMEOUT, exchange).await {
        Ok(Ok(line)) => line,
        Ok(Err(err)) => {
            mux.shutdown();
            return Err(err);
        }
        Err(_) => {
            mux.shutdown();
            return Err(Error::Timeout);
        }
    };

    let result = (|| {
        let packet = Packet::from_line(&line).ok_or(Error::Protocol("bad identity packet"))?;
        let info = DeviceInfo::from_packet(&packet).ok_or(Error::Protocol("bad identity packet"))?;
        if info.id == me.id {
            return Err(Error::Protocol("connection from ourselves"));
        }
        let cert = parse_certificate(packet.get_str("certificate").unwrap_or_default())
            .ok_or(Error::Protocol("identity packet has no usable certificate"))?;
        if identity::common_name(&cert).as_deref() != Some(info.id.as_str()) {
            return Err(Error::Protocol("certificate doesn't belong to this device id"));
        }
        Ok((info, cert))
    })();

    match result {
        Ok((info, cert)) => Ok(BtConnection {
            info,
            cert,
            mux,
            reader,
            writer,
        }),
        Err(err) => {
            mux.shutdown();
            Err(err)
        }
    }
}

/// Accepts a full PEM block or, like KDE Connect, just the base64 body.
fn parse_certificate(text: &str) -> Option<CertificateDer<'static>> {
    let text = text.trim();
    if text.starts_with("-----BEGIN CERTIFICATE-----") {
        CertificateDer::from_pem_slice(text.as_bytes()).ok()
    } else {
        let pem = format!("-----BEGIN CERTIFICATE-----\n{text}\n-----END CERTIFICATE-----\n");
        CertificateDer::from_pem_slice(pem.as_bytes()).ok()
    }
}

/// The channel a packet's payload arrives on.
pub fn payload_channel(packet: &Packet) -> Option<Uuid> {
    let id = packet.payload_transfer_info.as_ref()?.get("uuid")?.as_str()?;
    Uuid::parse_str(id).ok()
}

/// Mark `packet` as carrying a payload on `channel`.
pub fn set_payload_channel(packet: &mut Packet, channel: &Channel) {
    let mut info = serde_json::Map::new();
    info.insert("uuid".into(), channel.id().to_string().into());
    packet.payload_transfer_info = Some(info);
}

/// Read exactly `size` payload bytes from `channel` into `sink`.
pub async fn receive<W: tokio::io::AsyncWrite + Unpin>(
    channel: Channel,
    mut sink: W,
    size: u64,
    mut progress: impl FnMut(u64),
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut buf = vec![0u8; 4096];
    let mut received = 0u64;
    while received < size {
        let want = buf.len().min((size - received) as usize);
        let n = channel.read(&mut buf[..want]).await?;
        if n == 0 {
            return Err(Error::Protocol("payload ended early"));
        }
        sink.write_all(&buf[..n]).await?;
        received += n as u64;
        progress(received);
    }
    sink.flush().await?;
    channel.close();
    Ok(())
}

/// Stream `size` bytes from `source` into `channel`, then close it.
pub async fn send<R: tokio::io::AsyncRead + Unpin>(
    channel: Channel,
    mut source: R,
    size: u64,
    mut progress: impl FnMut(u64),
) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 4096];
    let mut sent = 0u64;
    let result = async {
        while sent < size {
            let want = buf.len().min((size - sent) as usize);
            let n = source.read(&mut buf[..want]).await?;
            if n == 0 {
                return Err(Error::Protocol("file shrank while sending"));
            }
            channel.write_all(&buf[..n]).await?;
            sent += n as u64;
            progress(sent);
        }
        Ok(())
    }
    .await;
    channel.close();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTOCOL_VERSION;

    fn device(id: &str, name: &str) -> (DeviceInfo, Identity) {
        let info = DeviceInfo {
            id: id.into(),
            name: name.into(),
            kind: "phone".into(),
            protocol_version: PROTOCOL_VERSION,
            incoming: vec![],
            outgoing: vec![],
        };
        (info, Identity::generate(id).unwrap())
    }

    #[tokio::test]
    async fn handshake_and_payload() {
        let (a_info, a_id) = device("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "PC");
        let (b_info, b_id) = device("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "Phone");
        let (a_stream, b_stream) = tokio::io::duplex(1 << 16);

        let (a, b) = tokio::join!(
            handshake(a_stream, Role::Accepted, &a_info, &a_id),
            handshake(b_stream, Role::Connected, &b_info, &b_id),
        );
        let (mut a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.info.name, "Phone");
        assert_eq!(a.cert, b_id.cert);
        assert_eq!(b.info.name, "PC");
        assert_eq!(b.cert, a_id.cert);

        // B sends A a 50 KB file announced by a packet.
        let data: Vec<u8> = (0..50_000u32).map(|i| (i % 7) as u8).collect();
        let channel = b.mux.open_channel();
        let mut packet = Packet::new("kdeconnect.share.request", serde_json::json!({ "filename": "x" }));
        packet.payload_size = Some(data.len() as i64);
        set_payload_channel(&mut packet, &channel);
        b.writer.write_all(&packet.to_line()).await.unwrap();
        let sender = {
            let data = data.clone();
            tokio::spawn(async move { send(channel, &data[..], 50_000, |_| {}).await })
        };

        let line = a.reader.read_line(1 << 20).await.unwrap().unwrap();
        let got_packet = Packet::from_line(&line).unwrap();
        let id = payload_channel(&got_packet).unwrap();
        let input = a.mux.take_channel(id).expect("channel was opened before the packet");
        let mut got = Vec::new();
        receive(input, &mut got, 50_000, |_| {}).await.unwrap();
        sender.await.unwrap().unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn certificates_with_or_without_pem_armour() {
        let id = Identity::generate("cccccccccccccccccccccccccccccccc").unwrap();
        let pem = id.cert_pem();
        assert_eq!(parse_certificate(&pem), Some(id.cert.clone()));
        let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        assert_eq!(parse_certificate(&body), Some(id.cert));
    }
}
