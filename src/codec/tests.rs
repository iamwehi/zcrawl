//! Codec tests against fixed byte vectors. The checksums were computed
//! independently with Python's hashlib, not with this code.

use super::*;

const MAINNET: [u8; 4] = [0x24, 0xe9, 0x27, 0x64];
const TESTNET: [u8; 4] = [0xfa, 0x1a, 0xf9, 0xbf];

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn header(magic: [u8; 4], command: &str, len: u32, checksum: [u8; 4]) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[..4].copy_from_slice(&magic);
    h[4..4 + command.len()].copy_from_slice(command.as_bytes());
    h[16..20].copy_from_slice(&len.to_le_bytes());
    h[20..].copy_from_slice(&checksum);
    h
}

fn frame(magic: [u8; 4], command: &str, payload: &[u8], checksum: [u8; 4]) -> Vec<u8> {
    let mut v = header(magic, command, payload.len() as u32, checksum).to_vec();
    v.extend_from_slice(payload);
    v
}

async fn read(bytes: &[u8], magic: [u8; 4]) -> Result<Message, ReadError> {
    let mut r = bytes;
    read_message(&mut r, magic).await
}

const VERSION_PAYLOAD: &str = "ce980200 0100000000000000 a0431c6900000000
    0100000000000000 00000000000000000000ffff01020304 2029
    0000000000000000 00000000000000000000ffff00000000 0000
    8877665544332211 0d 2f5a656272613a322e352e302f 6d113600 01";
const VERSION_CHECKSUM: [u8; 4] = [0x27, 0x29, 0x86, 0x6b];

fn expected_version() -> VersionMessage {
    VersionMessage {
        version: 170_190,
        services: 1,
        timestamp: 1_763_460_000,
        addr_recv: NetAddr {
            services: 1,
            addr: "1.2.3.4:8233".parse().unwrap(),
        },
        addr_from: NetAddr {
            services: 0,
            addr: "0.0.0.0:0".parse().unwrap(),
        },
        nonce: 0x1122_3344_5566_7788,
        user_agent: b"/Zebra:2.5.0/".to_vec(),
        start_height: 3_543_405,
        relay: true,
    }
}

#[test]
fn checksum_of_empty_payload() {
    assert_eq!(checksum(&[]), [0x5d, 0xf6, 0xe0, 0xe2]);
}

#[test]
fn encode_verack_mainnet() {
    let expected = hex("24e92764 76657261636b000000000000 00000000 5df6e0e2");
    assert_eq!(encode_message(MAINNET, &Message::Verack), expected);
}

#[test]
fn encode_getaddr_testnet() {
    let expected = hex("fa1af9bf 676574616464720000000000 00000000 5df6e0e2");
    assert_eq!(encode_message(TESTNET, &Message::GetAddr), expected);
}

#[test]
fn encode_and_decode_ping_pong() {
    let expected = hex("24e92764 70696e670000000000000000 08000000 3b5a7513 0807060504030201");
    assert_eq!(
        encode_message(MAINNET, &Message::Ping(Some(0x0102_0304_0506_0708))),
        expected
    );
    let pong = encode_message(MAINNET, &Message::Pong(0x0102_0304_0506_0708));
    assert_eq!(&pong[4..16], b"pong\0\0\0\0\0\0\0\0");
    assert_eq!(&pong[16..], &expected[16..]);
    assert_eq!(decode_payload("ping", &[]).unwrap(), Message::Ping(None));
    assert_eq!(
        decode_payload("ping", &[1, 2, 3]).unwrap_err(),
        CodecError::Truncated
    );
}

#[test]
fn version_round_trip() {
    let payload = hex(VERSION_PAYLOAD);
    assert_eq!(payload.len(), 99);
    assert_eq!(checksum(&payload), VERSION_CHECKSUM);
    let msg = decode_payload("version", &payload).unwrap();
    assert_eq!(msg, Message::Version(expected_version()));
    let encoded = encode_message(MAINNET, &msg);
    assert_eq!(
        encoded,
        frame(MAINNET, "version", &payload, VERSION_CHECKSUM)
    );
}

#[test]
fn version_without_relay_flag() {
    let mut payload = hex(VERSION_PAYLOAD);
    payload.pop();
    let Message::Version(v) = decode_payload("version", &payload).unwrap() else {
        panic!()
    };
    assert!(!v.relay);
}

#[test]
fn version_bad_relay_and_truncation() {
    let mut payload = hex(VERSION_PAYLOAD);
    *payload.last_mut().unwrap() = 2;
    assert!(matches!(
        decode_payload("version", &payload),
        Err(CodecError::Malformed(_))
    ));
    let payload = hex(VERSION_PAYLOAD);
    for cut in [0, 10, 80, 94] {
        assert_eq!(
            decode_payload("version", &payload[..cut]),
            Err(CodecError::Truncated),
            "{cut}"
        );
    }
}

#[test]
fn version_user_agent_length_bounded() {
    // Claims a 0xffff-byte user agent: rejected before any allocation.
    let mut payload = hex(VERSION_PAYLOAD)[..80].to_vec();
    payload.extend_from_slice(&[0xfd, 0xff, 0xff]);
    assert!(matches!(
        decode_payload("version", &payload),
        Err(CodecError::Malformed(_))
    ));
    // Claims a u64::MAX-byte user agent.
    let mut payload = hex(VERSION_PAYLOAD)[..80].to_vec();
    payload.extend_from_slice(&[0xff; 9]);
    assert!(matches!(
        decode_payload("version", &payload),
        Err(CodecError::Malformed(_))
    ));
    // 257 bytes, one over the limit.
    let mut payload = hex(VERSION_PAYLOAD)[..80].to_vec();
    payload.extend_from_slice(&[0xfd, 0x01, 0x01]);
    payload.extend_from_slice(&[b'a'; 257]);
    payload.extend_from_slice(&[0, 0, 0, 0, 0]);
    assert!(matches!(
        decode_payload("version", &payload),
        Err(CodecError::Malformed(_))
    ));
}

#[test]
fn non_canonical_compact_size_rejected() {
    // 0xfd prefix followed by a value below 0xfd.
    let payload = hex("fd0200");
    assert!(matches!(
        decode_payload("addr", &payload),
        Err(CodecError::Malformed(_))
    ));
}

#[test]
fn addr_two_entries() {
    let payload = hex("02
         a0431c69 0100000000000000 00000000000000000000ffff05060708 2029
         a1431c69 0904000000000000 2a0104f8000000000000000000000001 4739");
    assert_eq!(checksum(&payload), [0x21, 0xb3, 0xfe, 0x7e]);
    let msg = decode_payload("addr", &payload).unwrap();
    let expected = Message::Addr(vec![
        AddrEntry {
            time: 1_763_460_000,
            services: 1,
            addr: "5.6.7.8:8233".parse().unwrap(),
        },
        AddrEntry {
            time: 1_763_460_001,
            services: 1033,
            addr: "[2a01:4f8::1]:18233".parse().unwrap(),
        },
    ]);
    assert_eq!(msg, expected);
    assert_eq!(encode_message(MAINNET, &expected)[24..], payload[..]);
}

#[test]
fn addr_count_bounds() {
    // 1001 entries: over the limit.
    assert!(matches!(
        decode_payload("addr", &hex("fde903")),
        Err(CodecError::Malformed(_))
    ));
    // 1000 entries claimed, none present: rejected before allocating.
    assert_eq!(
        decode_payload("addr", &hex("fde803")),
        Err(CodecError::Truncated)
    );
}

#[test]
fn addrv2_keeps_ip_drops_tor() {
    let payload = hex("03
         a0431c69 01 01 04 09090909 2029
         a0431c69 01 04 20 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f 2029
         a0431c69 fd0904 02 10 2a0104f8000000000000000000000001 2029");
    assert_eq!(checksum(&payload), [0xa3, 0x0a, 0xc6, 0xbc]);
    let msg = decode_payload("addrv2", &payload).unwrap();
    assert_eq!(
        msg,
        Message::AddrV2(vec![
            AddrEntry {
                time: 1_763_460_000,
                services: 1,
                addr: "9.9.9.9:8233".parse().unwrap()
            },
            AddrEntry {
                time: 1_763_460_000,
                services: 1033,
                addr: "[2a01:4f8::1]:8233".parse().unwrap()
            },
        ])
    );
}

#[test]
fn addrv2_bounds() {
    // IPv4 entry with a 5-byte address.
    let bad_len = hex("01 a0431c69 01 01 05 0909090909 2029");
    assert!(matches!(
        decode_payload("addrv2", &bad_len),
        Err(CodecError::Malformed(_))
    ));
    // Address longer than 512 bytes.
    let too_long = hex("01 a0431c69 01 04 fd0102");
    assert!(matches!(
        decode_payload("addrv2", &too_long),
        Err(CodecError::Malformed(_))
    ));
    // 1000 entries claimed in a 3-byte payload.
    assert_eq!(
        decode_payload("addrv2", &hex("fde803")),
        Err(CodecError::Truncated)
    );
}

#[test]
fn header_rejects_bad_command() {
    let mut h = header(MAINNET, "verack", 0, checksum(&[]));
    h[12] = b'x'; // a byte after the NUL padding started
    assert_eq!(decode_header(&h, MAINNET), Err(CodecError::BadCommand));
    let h = header(MAINNET, "", 0, checksum(&[]));
    assert_eq!(decode_header(&h, MAINNET), Err(CodecError::BadCommand));
    let mut h = header(MAINNET, "ver ack", 0, checksum(&[]));
    h[7] = b' ';
    assert_eq!(decode_header(&h, MAINNET), Err(CodecError::BadCommand));
}

#[tokio::test]
async fn read_wrong_magic() {
    let bytes = encode_message(TESTNET, &Message::Verack);
    let err = read(&bytes, MAINNET).await.unwrap_err();
    assert!(matches!(err, ReadError::Codec(CodecError::BadMagic(m)) if m == TESTNET));
}

#[tokio::test]
async fn read_oversized_length_field() {
    // A version header claiming 4 GiB, with no payload behind it. Rejected from
    // the header alone; nothing is allocated or awaited.
    let h = header(MAINNET, "version", u32::MAX, [0; 4]);
    let err = read(&h, MAINNET).await.unwrap_err();
    assert!(matches!(
        err,
        ReadError::Codec(CodecError::PayloadTooLarge {
            length: u32::MAX,
            limit: 1024,
            ..
        })
    ));
    // An unparsed command one byte over the 2 MiB protocol limit.
    let h = header(MAINNET, "block", 2 * 1024 * 1024 + 1, [0; 4]);
    let err = read(&h, MAINNET).await.unwrap_err();
    assert!(matches!(
        err,
        ReadError::Codec(CodecError::PayloadTooLarge { .. })
    ));
    // verack must be empty.
    let h = header(MAINNET, "verack", 1, [0; 4]);
    assert!(matches!(
        read(&h, MAINNET).await.unwrap_err(),
        ReadError::Codec(CodecError::PayloadTooLarge { .. })
    ));
}

#[tokio::test]
async fn read_wrong_checksum() {
    let payload = hex("0807060504030201");
    let bytes = frame(MAINNET, "ping", &payload, [0x3b, 0x5a, 0x75, 0x14]);
    let err = read(&bytes, MAINNET).await.unwrap_err();
    assert!(matches!(err, ReadError::Codec(CodecError::BadChecksum)));
    // Also for a command that is streamed rather than parsed.
    let bytes = frame(MAINNET, "inv", &[0x00], [0, 0, 0, 0]);
    let err = read(&bytes, MAINNET).await.unwrap_err();
    assert!(matches!(err, ReadError::Codec(CodecError::BadChecksum)));
}

#[tokio::test]
async fn read_valid_messages() {
    let payload = hex(VERSION_PAYLOAD);
    let bytes = frame(MAINNET, "version", &payload, VERSION_CHECKSUM);
    assert_eq!(
        read(&bytes, MAINNET).await.unwrap(),
        Message::Version(expected_version())
    );
    // An unparsed command larger than the 8 KiB streaming buffer.
    let big = vec![7u8; 20_000];
    let bytes = frame(MAINNET, "inv", &big, checksum(&big));
    assert_eq!(
        read(&bytes, MAINNET).await.unwrap(),
        Message::Other("inv".into())
    );
}

#[tokio::test]
async fn read_truncated_stream() {
    let bytes = encode_message(MAINNET, &Message::Ping(Some(1)));
    let err = read(&bytes[..bytes.len() - 1], MAINNET).await.unwrap_err();
    assert!(matches!(err, ReadError::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof));
}

#[test]
fn decoder_survives_arbitrary_bytes() {
    // A cheap deterministic fuzz: no input may panic.
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for len in 0..400 {
        let bytes: Vec<u8> = (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        for cmd in ["version", "addr", "addrv2", "ping", "pong"] {
            let _ = decode_payload(cmd, &bytes);
        }
        if let Ok(h) = <[u8; HEADER_LEN]>::try_from(&bytes[..HEADER_LEN.min(bytes.len())]) {
            let _ = decode_header(&h, MAINNET);
        }
    }
}
