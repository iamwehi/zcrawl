//! A minimal Zcash p2p message codec.
//!
//! It handles only the messages a crawler needs: the 24-byte header, `version`,
//! `verack`, `ping`, `pong`, `getaddr`, `addr` and `addrv2`. Every other command
//! is read, checksummed and discarded without being parsed or buffered.
//!
//! Every length taken from the network is checked against a fixed bound before
//! anything is allocated, and every decode error is returned as a
//! [`CodecError`]; nothing here panics on peer input.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::params::{
    MAX_ADDR_V2_ADDR_SIZE, MAX_ADDRS_IN_MESSAGE, MAX_PROTOCOL_MESSAGE_LEN, MAX_USER_AGENT_LENGTH,
};

pub const HEADER_LEN: usize = 24;

/// Bytes in one `addr` (v1) entry: time, services, IPv6 address, port.
const ADDR_V1_ENTRY_LEN: usize = 4 + 8 + 16 + 2;
/// Upper bound on one `addrv2` entry: time, services (CompactSize), network id,
/// address length (CompactSize), address, port.
const ADDR_V2_MAX_ENTRY_LEN: usize = 4 + 9 + 1 + 3 + MAX_ADDR_V2_ADDR_SIZE + 2;

const ADDR_V2_IPV4: u8 = 0x01;
const ADDR_V2_IPV6: u8 = 0x02;

/// Why a message could not be decoded. Any of these ends the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    BadMagic([u8; 4]),
    BadCommand,
    PayloadTooLarge {
        command: String,
        length: u32,
        limit: u32,
    },
    BadChecksum,
    Truncated,
    Malformed(&'static str),
}

impl CodecError {
    /// Short stable code stored in `observations.failure_reason`.
    pub fn reason_code(&self) -> &'static str {
        match self {
            CodecError::BadMagic(_) => "bad_magic",
            CodecError::BadCommand => "bad_command",
            CodecError::PayloadTooLarge { .. } => "oversized_message",
            CodecError::BadChecksum => "bad_checksum",
            CodecError::Truncated | CodecError::Malformed(_) => "malformed_message",
        }
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::BadMagic(m) => write!(f, "wrong network magic {:02x?}", m),
            CodecError::BadCommand => write!(f, "command is not NUL-padded ASCII"),
            CodecError::PayloadTooLarge {
                command,
                length,
                limit,
            } => {
                write!(
                    f,
                    "{command} payload of {length} bytes exceeds limit of {limit}"
                )
            }
            CodecError::BadChecksum => write!(f, "payload checksum mismatch"),
            CodecError::Truncated => write!(f, "payload ends before its fields do"),
            CodecError::Malformed(why) => write!(f, "malformed payload: {why}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Error from reading a message off a stream.
#[derive(Debug)]
pub enum ReadError {
    Io(std::io::Error),
    Codec(CodecError),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Io(e) => write!(f, "{e}"),
            ReadError::Codec(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReadError {}

impl From<std::io::Error> for ReadError {
    fn from(e: std::io::Error) -> Self {
        ReadError::Io(e)
    }
}

impl From<CodecError> for ReadError {
    fn from(e: CodecError) -> Self {
        ReadError::Codec(e)
    }
}

/// An address and the services advertised for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetAddr {
    pub services: u64,
    pub addr: SocketAddr,
}

/// A gossiped address from `addr` or `addrv2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddrEntry {
    /// Peer-claimed last-seen time, Unix seconds.
    pub time: u32,
    pub services: u64,
    pub addr: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMessage {
    pub version: u32,
    pub services: u64,
    pub timestamp: i64,
    pub addr_recv: NetAddr,
    pub addr_from: NetAddr,
    pub nonce: u64,
    /// Exactly the bytes the peer sent; not necessarily UTF-8.
    pub user_agent: Vec<u8>,
    pub start_height: i32,
    pub relay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Version(VersionMessage),
    Verack,
    /// BIP 31 ping. `None` for the pre-BIP 31 form with no nonce.
    Ping(Option<u64>),
    Pong(u64),
    GetAddr,
    Addr(Vec<AddrEntry>),
    /// Only the IPv4 and IPv6 entries; Tor, I2P and CJDNS entries are dropped.
    AddrV2(Vec<AddrEntry>),
    /// A valid message with a command we do not parse.
    Other(String),
}

impl Message {
    pub fn command(&self) -> &str {
        match self {
            Message::Version(_) => "version",
            Message::Verack => "verack",
            Message::Ping(_) => "ping",
            Message::Pong(_) => "pong",
            Message::GetAddr => "getaddr",
            Message::Addr(_) => "addr",
            Message::AddrV2(_) => "addrv2",
            Message::Other(c) => c,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub command: String,
    pub length: u32,
    pub checksum: [u8; 4],
}

/// Largest payload accepted for `command`. Commands we parse get a tight
/// bound; everything else gets the protocol maximum and is streamed past.
pub fn payload_limit(command: &str) -> u32 {
    let limit = match command {
        "version" => 1024,
        "verack" | "getaddr" => 0,
        "ping" | "pong" => 8,
        "addr" => 3 + MAX_ADDRS_IN_MESSAGE * ADDR_V1_ENTRY_LEN,
        "addrv2" => 3 + MAX_ADDRS_IN_MESSAGE * ADDR_V2_MAX_ENTRY_LEN,
        _ => return MAX_PROTOCOL_MESSAGE_LEN,
    };
    // All of the above are far below u32::MAX.
    u32::try_from(limit).unwrap_or(MAX_PROTOCOL_MESSAGE_LEN)
}

/// True for the commands [`decode_payload`] parses.
pub fn is_parsed_command(command: &str) -> bool {
    matches!(
        command,
        "version" | "verack" | "ping" | "pong" | "getaddr" | "addr" | "addrv2"
    )
}

/// First four bytes of SHA-256(SHA-256(payload)).
pub fn checksum(payload: &[u8]) -> [u8; 4] {
    let second = Sha256::digest(Sha256::digest(payload));
    [second[0], second[1], second[2], second[3]]
}

/// Decode and validate a message header.
pub fn decode_header(bytes: &[u8; HEADER_LEN], magic: [u8; 4]) -> Result<Header, CodecError> {
    let got_magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if got_magic != magic {
        return Err(CodecError::BadMagic(got_magic));
    }
    let command = decode_command(&bytes[4..16])?;
    let length = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let checksum = [bytes[20], bytes[21], bytes[22], bytes[23]];
    let limit = payload_limit(&command);
    if length > limit {
        return Err(CodecError::PayloadTooLarge {
            command,
            length,
            limit,
        });
    }
    Ok(Header {
        command,
        length,
        checksum,
    })
}

fn decode_command(raw: &[u8]) -> Result<String, CodecError> {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let (name, padding) = raw.split_at(end);
    if name.is_empty()
        || !name.iter().all(|b| b.is_ascii_graphic())
        || !padding.iter().all(|&b| b == 0)
    {
        return Err(CodecError::BadCommand);
    }
    // Checked above to be ASCII.
    Ok(name.iter().map(|&b| char::from(b)).collect())
}

/// Decode the payload of a command [`is_parsed_command`] accepts. Any other
/// command decodes to [`Message::Other`].
pub fn decode_payload(command: &str, payload: &[u8]) -> Result<Message, CodecError> {
    let mut r = Reader::new(payload);
    let msg = match command {
        "version" => Message::Version(decode_version(&mut r)?),
        "verack" => Message::Verack,
        "getaddr" => Message::GetAddr,
        "ping" => {
            if payload.is_empty() {
                Message::Ping(None)
            } else {
                Message::Ping(Some(r.u64_le()?))
            }
        }
        "pong" => Message::Pong(r.u64_le()?),
        "addr" => Message::Addr(decode_addr_v1(&mut r)?),
        "addrv2" => Message::AddrV2(decode_addr_v2(&mut r)?),
        other => Message::Other(other.to_owned()),
    };
    Ok(msg)
}

fn decode_version(r: &mut Reader<'_>) -> Result<VersionMessage, CodecError> {
    let version = r.u32_le()?;
    let services = r.u64_le()?;
    let timestamp = r.i64_le()?;
    let addr_recv = decode_addr_in_version(r)?;
    let addr_from = decode_addr_in_version(r)?;
    let nonce = r.u64_le()?;
    let ua_len = r.compact_size()?;
    if ua_len > MAX_USER_AGENT_LENGTH as u64 {
        return Err(CodecError::Malformed("user agent longer than 256 bytes"));
    }
    // ua_len <= 256, so the cast is lossless.
    let user_agent = r.bytes(ua_len as usize)?.to_vec();
    let start_height = r.i32_le()?;
    // `relay` is optional in the Bitcoin protocol; Zebra always sends it.
    let relay = match r.remaining() {
        0 => false,
        _ => match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(CodecError::Malformed("relay flag is not 0 or 1")),
        },
    };
    // Bytes after `relay` are ignored, as later protocol versions may add fields.
    Ok(VersionMessage {
        version,
        services,
        timestamp,
        addr_recv,
        addr_from,
        nonce,
        user_agent,
        start_height,
        relay,
    })
}

fn decode_addr_in_version(r: &mut Reader<'_>) -> Result<NetAddr, CodecError> {
    let services = r.u64_le()?;
    let ip = r.ipv6()?;
    let port = r.u16_be()?;
    Ok(NetAddr {
        services,
        addr: SocketAddr::new(ip, port),
    })
}

fn decode_addr_v1(r: &mut Reader<'_>) -> Result<Vec<AddrEntry>, CodecError> {
    let count = r.compact_size()?;
    if count > MAX_ADDRS_IN_MESSAGE as u64 {
        return Err(CodecError::Malformed(
            "addr message has more than 1000 entries",
        ));
    }
    // count <= 1000, so the cast is lossless.
    let count = count as usize;
    if r.remaining() < count * ADDR_V1_ENTRY_LEN {
        return Err(CodecError::Truncated);
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let time = r.u32_le()?;
        let services = r.u64_le()?;
        let ip = r.ipv6()?;
        let port = r.u16_be()?;
        out.push(AddrEntry {
            time,
            services,
            addr: SocketAddr::new(ip, port),
        });
    }
    Ok(out)
}

fn decode_addr_v2(r: &mut Reader<'_>) -> Result<Vec<AddrEntry>, CodecError> {
    let count = r.compact_size()?;
    if count > MAX_ADDRS_IN_MESSAGE as u64 {
        return Err(CodecError::Malformed(
            "addrv2 message has more than 1000 entries",
        ));
    }
    // Smallest possible entry: time 4, services 1, network 1, length 1, port 2.
    if (r.remaining() as u64) < count * 9 {
        return Err(CodecError::Truncated);
    }
    let mut out = Vec::new();
    for _ in 0..count {
        let time = r.u32_le()?;
        let services = r.compact_size()?;
        let network = r.u8()?;
        let len = r.compact_size()?;
        if len > MAX_ADDR_V2_ADDR_SIZE as u64 {
            return Err(CodecError::Malformed(
                "addrv2 address longer than 512 bytes",
            ));
        }
        // len <= 512, so the cast is lossless.
        let raw = r.bytes(len as usize)?;
        let port = r.u16_be()?;
        let ip = match network {
            ADDR_V2_IPV4 => {
                let b: [u8; 4] = raw
                    .try_into()
                    .map_err(|_| CodecError::Malformed("addrv2 IPv4 address is not 4 bytes"))?;
                IpAddr::V4(Ipv4Addr::from(b))
            }
            ADDR_V2_IPV6 => {
                let b: [u8; 16] = raw
                    .try_into()
                    .map_err(|_| CodecError::Malformed("addrv2 IPv6 address is not 16 bytes"))?;
                IpAddr::V6(Ipv6Addr::from(b))
            }
            // Tor, I2P, CJDNS and unknown networks are out of scope.
            _ => continue,
        };
        out.push(AddrEntry {
            time,
            services,
            addr: SocketAddr::new(ip, port),
        });
    }
    Ok(out)
}

/// Encode a complete message, header included.
pub fn encode_message(magic: [u8; 4], msg: &Message) -> Vec<u8> {
    let payload = encode_payload(msg);
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&magic);
    let mut command = [0u8; 12];
    for (dst, src) in command.iter_mut().zip(msg.command().bytes()) {
        *dst = src;
    }
    out.extend_from_slice(&command);
    // Our own payloads are always small; saturate rather than panic.
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&checksum(&payload));
    out.extend_from_slice(&payload);
    out
}

fn encode_payload(msg: &Message) -> Vec<u8> {
    let mut p = Vec::new();
    match msg {
        Message::Version(v) => {
            p.extend_from_slice(&v.version.to_le_bytes());
            p.extend_from_slice(&v.services.to_le_bytes());
            p.extend_from_slice(&v.timestamp.to_le_bytes());
            for a in [&v.addr_recv, &v.addr_from] {
                p.extend_from_slice(&a.services.to_le_bytes());
                put_ip_port(&mut p, a.addr);
            }
            p.extend_from_slice(&v.nonce.to_le_bytes());
            put_compact_size(&mut p, v.user_agent.len() as u64);
            p.extend_from_slice(&v.user_agent);
            p.extend_from_slice(&v.start_height.to_le_bytes());
            p.push(u8::from(v.relay));
        }
        Message::Verack | Message::GetAddr | Message::Other(_) | Message::Ping(None) => {}
        Message::Ping(Some(n)) | Message::Pong(n) => p.extend_from_slice(&n.to_le_bytes()),
        Message::Addr(entries) => {
            put_compact_size(&mut p, entries.len() as u64);
            for e in entries {
                p.extend_from_slice(&e.time.to_le_bytes());
                p.extend_from_slice(&e.services.to_le_bytes());
                put_ip_port(&mut p, e.addr);
            }
        }
        Message::AddrV2(entries) => {
            put_compact_size(&mut p, entries.len() as u64);
            for e in entries {
                p.extend_from_slice(&e.time.to_le_bytes());
                put_compact_size(&mut p, e.services);
                match e.addr.ip() {
                    IpAddr::V4(ip) => {
                        p.push(ADDR_V2_IPV4);
                        put_compact_size(&mut p, 4);
                        p.extend_from_slice(&ip.octets());
                    }
                    IpAddr::V6(ip) => {
                        p.push(ADDR_V2_IPV6);
                        put_compact_size(&mut p, 16);
                        p.extend_from_slice(&ip.octets());
                    }
                }
                p.extend_from_slice(&e.addr.port().to_be_bytes());
            }
        }
    }
    p
}

fn put_ip_port(p: &mut Vec<u8>, addr: SocketAddr) {
    let v6 = match addr.ip() {
        IpAddr::V4(ip) => ip.to_ipv6_mapped(),
        IpAddr::V6(ip) => ip,
    };
    p.extend_from_slice(&v6.octets());
    p.extend_from_slice(&addr.port().to_be_bytes());
}

fn put_compact_size(p: &mut Vec<u8>, n: u64) {
    match n {
        0..=0xfc => p.push(n as u8),
        0xfd..=0xffff => {
            p.push(0xfd);
            p.extend_from_slice(&(n as u16).to_le_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            p.push(0xfe);
            p.extend_from_slice(&(n as u32).to_le_bytes());
        }
        _ => {
            p.push(0xff);
            p.extend_from_slice(&n.to_le_bytes());
        }
    }
}

/// Read one message from `stream`.
///
/// Payloads of parsed commands are read into a buffer no larger than
/// [`payload_limit`]. Payloads of other commands are streamed through a fixed
/// 8 KiB buffer while their checksum is computed, so they never allocate.
pub async fn read_message<R: AsyncRead + Unpin>(
    stream: &mut R,
    magic: [u8; 4],
) -> Result<Message, ReadError> {
    let mut header_bytes = [0u8; HEADER_LEN];
    stream.read_exact(&mut header_bytes).await?;
    let header = decode_header(&header_bytes, magic)?;
    // length <= MAX_PROTOCOL_MESSAGE_LEN (2 MiB), checked in decode_header.
    let length = header.length as usize;

    if is_parsed_command(&header.command) {
        let mut payload = vec![0u8; length];
        stream.read_exact(&mut payload).await?;
        if checksum(&payload) != header.checksum {
            return Err(CodecError::BadChecksum.into());
        }
        return Ok(decode_payload(&header.command, &payload)?);
    }

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    let mut left = length;
    while left > 0 {
        let n = left.min(buf.len());
        stream.read_exact(&mut buf[..n]).await?;
        hasher.update(&buf[..n]);
        left -= n;
    }
    let second = Sha256::digest(hasher.finalize());
    if second[..4] != header.checksum {
        return Err(CodecError::BadChecksum.into());
    }
    Ok(Message::Other(header.command))
}

/// Bounds-checked little cursor over a payload.
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf }
    }

    fn remaining(&self) -> usize {
        self.buf.len()
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        if self.buf.len() < n {
            return Err(CodecError::Truncated);
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let b = self.bytes(N)?;
        b.try_into().map_err(|_| CodecError::Truncated)
    }

    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16_be(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32_le(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn i32_le(&mut self) -> Result<i32, CodecError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    fn u64_le(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn i64_le(&mut self) -> Result<i64, CodecError> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    /// A 16-byte address; IPv4-mapped addresses come back as IPv4.
    fn ipv6(&mut self) -> Result<IpAddr, CodecError> {
        let v6 = Ipv6Addr::from(self.array::<16>()?);
        Ok(match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        })
    }

    /// Bitcoin CompactSize. Non-canonical encodings are rejected.
    fn compact_size(&mut self) -> Result<u64, CodecError> {
        let first = self.u8()?;
        let (n, min) = match first {
            0xfd => (u64::from(u16::from_le_bytes(self.array()?)), 0xfd),
            0xfe => (u64::from(u32::from_le_bytes(self.array()?)), 0x1_0000),
            0xff => (u64::from_le_bytes(self.array()?), 0x1_0000_0000),
            n => return Ok(u64::from(n)),
        };
        if n < min {
            return Err(CodecError::Malformed("non-canonical CompactSize"));
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests;
