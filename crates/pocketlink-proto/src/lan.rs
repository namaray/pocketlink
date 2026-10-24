// SPDX-License-Identifier: GPL-3.0-or-later

//! Wi-Fi / LAN transport.
//!
//! Discovery: each device broadcasts its identity over UDP (port 1716) with the
//! TCP port it listens on. Whoever *receives* the broadcast opens a TCP
//! connection to the sender and sends its own identity in plain text.
//!
//! Then TLS starts with the roles flipped: the TCP client is the TLS server and
//! the TCP server is the TLS client. Both sides then send their identity again,
//! encrypted, and only that copy is trusted (protocol version 8).

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls_pki_types::{CertificateDer, ServerName};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};

use crate::identity::{self, DeviceInfo, PROTOCOL_VERSION};
use crate::packet::Packet;
use crate::tls::TlsConfigs;
use crate::{Error, Result};

pub const UDP_PORT: u16 = 1716;
pub const TCP_PORTS: RangeInclusive<u16> = 1716..=1764;

const MAX_IDENTITY_SIZE: u64 = 512 * 1024;
const MAX_PACKET_SIZE: u64 = 32 * 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const RATE_LIMIT: Duration = Duration::from_millis(1000);

pub type Stream = TlsStream<TcpStream>;

/// An authenticated (but not necessarily trusted) connection to a device.
pub struct Connection {
    pub info: DeviceInfo,
    pub cert: CertificateDer<'static>,
    pub peer: SocketAddr,
    pub reader: BufReader<ReadHalf<Stream>>,
    pub writer: WriteHalf<Stream>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("info", &self.info)
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct LanConfig {
    /// Where we listen for announcements. 0 picks a free port (tests).
    pub udp_port: u16,
    /// Where we send announcements.
    pub announce_port: u16,
    pub tcp_ports: RangeInclusive<u16>,
    pub bind: IpAddr,
    /// Addresses to announce ourselves to directly, for networks that drop broadcasts.
    pub extra_targets: Vec<IpAddr>,
    /// Accept loopback peers and announce on loopback only. For tests.
    pub loopback: bool,
}

impl Default for LanConfig {
    fn default() -> Self {
        Self {
            udp_port: UDP_PORT,
            announce_port: UDP_PORT,
            tcp_ports: TCP_PORTS,
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            extra_targets: Vec::new(),
            loopback: false,
        }
    }
}

/// The running LAN transport. New connections arrive on the channel passed to [`Lan::start`].
#[derive(Clone)]
pub struct Lan {
    inner: Arc<Inner>,
}

struct Inner {
    config: LanConfig,
    me: watch::Receiver<DeviceInfo>,
    tls: TlsConfigs,
    udp: UdpSocket,
    tcp_port: u16,
    seen: Mutex<HashMap<IpAddr, Instant>>,
    connections: mpsc::Sender<Connection>,
}

impl Lan {
    pub async fn start(
        config: LanConfig,
        me: watch::Receiver<DeviceInfo>,
        tls: TlsConfigs,
        connections: mpsc::Sender<Connection>,
    ) -> io::Result<Self> {
        let udp = bind_udp(config.bind, config.udp_port)?;
        let (listener, tcp_port) = bind_tcp(config.bind, config.tcp_ports.clone()).await?;
        let udp_port = udp.local_addr()?.port();
        tracing::info!(udp = udp_port, tcp = tcp_port, "LAN transport listening");

        let lan = Lan {
            inner: Arc::new(Inner {
                config,
                me,
                tls,
                udp,
                tcp_port,
                seen: Mutex::new(HashMap::new()),
                connections,
            }),
        };

        tokio::spawn(lan.clone().accept_loop(listener));
        tokio::spawn(lan.clone().udp_loop());
        Ok(lan)
    }

    pub fn tcp_port(&self) -> u16 {
        self.inner.tcp_port
    }

    /// Announce ourselves to everyone on the local networks.
    pub async fn broadcast(&self) {
        let mut targets: Vec<IpAddr> = if self.inner.config.loopback {
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
        } else {
            broadcast_addresses()
        };
        targets.extend(self.inner.config.extra_targets.iter().copied());
        for ip in targets {
            self.announce_to(SocketAddr::new(ip, self.inner.config.announce_port))
                .await;
        }
    }

    pub fn udp_port(&self) -> u16 {
        self.inner.udp.local_addr().map(|a| a.port()).unwrap_or_default()
    }

    /// Announce ourselves to one address, so that device connects back to us.
    pub async fn announce_to(&self, target: SocketAddr) {
        let mut packet = self.inner.me.borrow().to_packet();
        packet.set("tcpPort", self.inner.tcp_port);
        if let Err(err) = self.inner.udp.send_to(&packet.to_line(), target).await {
            tracing::debug!(%target, %err, "UDP announce failed");
        }
    }

    async fn accept_loop(self, listener: TcpListener) {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(err) => {
                    tracing::warn!(%err, "TCP accept failed");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            };
            if !self.allowed(peer.ip()) || self.rate_limited(peer.ip()) {
                continue;
            }
            let lan = self.clone();
            tokio::spawn(async move {
                match timeout(HANDSHAKE_TIMEOUT, lan.incoming(stream, peer)).await {
                    Ok(Ok(conn)) => lan.deliver(conn).await,
                    Ok(Err(err)) => tracing::debug!(%peer, %err, "incoming handshake failed"),
                    Err(_) => tracing::debug!(%peer, "incoming handshake timed out"),
                }
            });
        }
    }

    async fn udp_loop(self) {
        let mut buf = vec![0u8; MAX_IDENTITY_SIZE as usize];
        loop {
            let (len, from) = match self.inner.udp.recv_from(&mut buf).await {
                Ok(received) => received,
                Err(err) => {
                    tracing::warn!(%err, "UDP receive failed");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            };
            let Some(packet) = Packet::from_line(&buf[..len]) else {
                continue;
            };
            let Some(remote) = DeviceInfo::from_packet(&packet) else {
                continue;
            };
            if remote.id == self.inner.me.borrow().id
                || !self.allowed(from.ip())
                || remote.protocol_version < PROTOCOL_VERSION
            {
                continue;
            }
            let Some(port) = packet
                .get_i64("tcpPort")
                .and_then(|p| u16::try_from(p).ok())
                .filter(|p| TCP_PORTS.contains(p) || self.inner.config.loopback)
            else {
                continue;
            };
            if self.rate_limited(from.ip()) {
                continue;
            }
            let lan = self.clone();
            tokio::spawn(async move {
                let addr = SocketAddr::new(from.ip(), port);
                match timeout(HANDSHAKE_TIMEOUT, lan.outgoing(addr, &remote)).await {
                    Ok(Ok(conn)) => lan.deliver(conn).await,
                    Ok(Err(err)) => {
                        tracing::debug!(%addr, %err, "outgoing handshake failed");
                        // They may be unable to accept connections; ask them to connect to us instead.
                        let port = lan.inner.config.announce_port;
                        lan.announce_to(SocketAddr::new(from.ip(), port)).await;
                    }
                    Err(_) => tracing::debug!(%addr, "outgoing handshake timed out"),
                }
            });
        }
    }

    /// They heard our broadcast and connected to us. We are the TLS client.
    async fn incoming(&self, mut stream: TcpStream, peer: SocketAddr) -> Result<Connection> {
        configure(&stream);
        let line = read_line_unbuffered(&mut stream, MAX_IDENTITY_SIZE).await?;
        let packet = Packet::from_line(&line).ok_or(Error::Protocol("bad identity packet"))?;
        let remote = DeviceInfo::from_packet(&packet).ok_or(Error::Protocol("bad identity packet"))?;
        let my_id = self.inner.me.borrow().id.clone();

        if remote.id == my_id {
            return Err(Error::Protocol("connection from ourselves"));
        }
        if remote.protocol_version < PROTOCOL_VERSION {
            return Err(Error::Protocol("protocol version too old"));
        }
        if packet.get_str("targetDeviceId").is_some_and(|t| t != my_id) {
            return Err(Error::Protocol("connection meant for another device"));
        }
        if packet
            .get_i64("targetProtocolVersion")
            .is_some_and(|v| v != PROTOCOL_VERSION)
        {
            return Err(Error::Protocol("connection meant for another protocol version"));
        }

        let connector = TlsConnector::from(self.inner.tls.client.clone());
        let tls = connector
            .connect(ServerName::from(peer.ip()), stream)
            .await?;
        self.secure_identity(TlsStream::Client(tls), &remote, peer).await
    }

    /// We heard their broadcast and connect to them. We are the TLS server.
    async fn outgoing(&self, addr: SocketAddr, remote: &DeviceInfo) -> Result<Connection> {
        let mut stream = TcpStream::connect(addr).await?;
        configure(&stream);

        let mut hello = self.inner.me.borrow().to_packet();
        hello.set("targetDeviceId", remote.id.as_str());
        hello.set("targetProtocolVersion", remote.protocol_version);
        stream.write_all(&hello.to_line()).await?;
        stream.flush().await?;

        let acceptor = TlsAcceptor::from(self.inner.tls.server.clone());
        let tls = acceptor.accept(stream).await?;
        self.secure_identity(TlsStream::Server(tls), remote, addr).await
    }

    /// Exchange identities over the encrypted link and make sure they match what was claimed.
    async fn secure_identity(
        &self,
        stream: Stream,
        claimed: &DeviceInfo,
        peer: SocketAddr,
    ) -> Result<Connection> {
        let cert = stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|c| c.first())
            .cloned()
            .ok_or(Error::Protocol("peer sent no certificate"))?;

        let (reader, mut writer) = tokio::io::split(stream);
        let me = self.inner.me.borrow().clone();
        writer.write_all(&me.to_packet().to_line()).await?;
        writer.flush().await?;

        let mut reader = BufReader::new(reader);
        let line = read_line(&mut reader, MAX_IDENTITY_SIZE)
            .await?
            .ok_or(Error::Protocol("closed before sending identity"))?;
        let info = Packet::from_line(&line)
            .as_ref()
            .and_then(DeviceInfo::from_packet)
            .ok_or(Error::Protocol("bad encrypted identity packet"))?;

        if info.id != claimed.id {
            return Err(Error::Protocol("device id changed during handshake"));
        }
        if info.protocol_version != claimed.protocol_version {
            return Err(Error::Protocol("protocol version changed during handshake"));
        }
        if identity::common_name(&cert).as_deref() != Some(info.id.as_str()) {
            return Err(Error::Protocol("certificate doesn't belong to this device id"));
        }

        Ok(Connection {
            info,
            cert,
            peer,
            reader,
            writer,
        })
    }

    async fn deliver(&self, conn: Connection) {
        tracing::info!(device = %conn.info.name, peer = %conn.peer, "connected");
        let _ = self.inner.connections.send(conn).await;
    }

    fn allowed(&self, ip: IpAddr) -> bool {
        if ip.is_loopback() {
            return self.inner.config.loopback;
        }
        is_local(ip) || self.inner.config.extra_targets.contains(&ip)
    }

    fn rate_limited(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut seen = self.inner.seen.lock().expect("not poisoned");
        seen.retain(|_, t| now.duration_since(*t) < RATE_LIMIT);
        if seen.contains_key(&ip) {
            return true;
        }
        seen.insert(ip, now);
        false
    }
}

/// Read one packet. `Ok(None)` means the connection closed cleanly.
pub async fn read_packet<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Packet>> {
    loop {
        let Some(line) = read_line(reader, MAX_PACKET_SIZE).await? else {
            return Ok(None);
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match Packet::from_line(&line) {
            Some(packet) => return Ok(Some(packet)),
            None => tracing::debug!("skipping malformed packet"),
        }
    }
}

async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let n = (&mut *reader).take(limit).read_until(b'\n', &mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            if n as u64 >= limit { "packet too large" } else { "connection closed mid-packet" },
        ));
    }
    Ok(Some(line))
}

/// Byte by byte, so nothing after the newline is consumed: TLS starts right after it.
async fn read_line_unbuffered<R: AsyncRead + Unpin>(reader: &mut R, limit: u64) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let byte = reader.read_u8().await?;
        line.push(byte);
        if byte == b'\n' {
            return Ok(line);
        }
        if line.len() as u64 >= limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "identity too large"));
        }
    }
}

fn configure(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10));
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let keepalive = keepalive.with_retries(3);
    let _ = SockRef::from(stream).set_tcp_keepalive(&keepalive);
}

fn bind_udp(ip: IpAddr, port: u16) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let addr = SocketAddr::new(ip, port);
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    // Lets us share the port with another KDE Connect implementation on this machine.
    socket.set_reuse_address(true)?;
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    UdpSocket::from_std(socket.into())
}

async fn bind_tcp(ip: IpAddr, ports: RangeInclusive<u16>) -> io::Result<(TcpListener, u16)> {
    let mut last_err = None;
    for port in ports {
        match TcpListener::bind(SocketAddr::new(ip, port)).await {
            Ok(listener) => {
                let port = listener.local_addr()?.port();
                return Ok((listener, port));
            }
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::AddrInUse, "no free port")))
}

fn broadcast_addresses() -> Vec<IpAddr> {
    let mut out = vec![IpAddr::V4(Ipv4Addr::BROADCAST)];
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if let if_addrs::IfAddr::V4(v4) = iface.addr
                && let Some(b) = v4.broadcast.filter(|_| !v4.ip.is_loopback()) {
                    out.push(IpAddr::V4(b));
                }
        }
    }
    out.dedup();
    out
}

/// Private, link-local or CGNAT (Tailscale) addresses.
fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private() || v4.is_link_local() || (v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64)
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_local(IpAddr::V4(v4)),
            None => (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_addresses() {
        assert!(is_local("192.168.1.20".parse().unwrap()));
        assert!(is_local("10.0.0.1".parse().unwrap()));
        assert!(is_local("100.101.102.103".parse().unwrap()));
        assert!(is_local("::ffff:192.168.0.2".parse().unwrap()));
        assert!(!is_local("8.8.8.8".parse().unwrap()));
        assert!(!is_local("100.200.1.1".parse().unwrap()));
    }
}
