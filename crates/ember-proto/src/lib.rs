//! Ember agent-link protocol: generated protobuf messages ([`v1`]) and the frame codec.
//!
//! Every frame is `u32 len | u16 type | u8 flags | payload`, little-endian, where `len` is the
//! payload length on the wire. Flag bit 0 marks a zstd-compressed payload. There is no checksum:
//! TCP/TLS already guarantee integrity. See `docs/spec/agent-link.md`.

use std::io::{self, Read, Write};

pub use prost::Message;

pub mod triggers;

#[allow(clippy::all, clippy::pedantic)]
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/ember.link.v1.rs"));
}

pub const PROTOCOL_SEMVER: &str = "1.0.0";
pub const HEADER_LEN: usize = 7;
pub const FLAG_ZSTD: u8 = 1;
/// Upper bound for a single frame payload (compressed or not).
pub const MAX_FRAME: u32 = 256 << 20;
/// Payloads larger than this are zstd-compressed.
pub const COMPRESS_THRESHOLD: usize = 4096;
/// Magic first bytes of an offline `.emberlink` recording file.
pub const RECORDING_MAGIC: &[u8; 8] = b"EMBRLINK";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum FrameType {
    Hello = 1,
    Auth = 2,
    Welcome = 3,
    Resume = 4,
    DataBatch = 5,
    Ack = 6,
    Request = 7,
    Response = 8,
    BlobChunk = 9,
    Error = 10,
    Ping = 11,
    Pong = 12,
    RecordingHeader = 13,
}

impl FrameType {
    pub fn from_u16(v: u16) -> Option<Self> {
        use FrameType::*;
        Some(match v {
            1 => Hello,
            2 => Auth,
            3 => Welcome,
            4 => Resume,
            5 => DataBatch,
            6 => Ack,
            7 => Request,
            8 => Response,
            9 => BlobChunk,
            10 => Error,
            11 => Ping,
            12 => Pong,
            13 => RecordingHeader,
            _ => return None,
        })
    }
}

/// A decoded frame with its payload already decompressed.
#[derive(Debug, Clone)]
pub struct Frame {
    pub ty: u16,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn frame_type(&self) -> Option<FrameType> {
        FrameType::from_u16(self.ty)
    }

    pub fn decode<M: Message + Default>(&self) -> io::Result<M> {
        M::decode(self.payload.as_slice()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Encodes `msg` as a complete frame (header + payload).
pub fn encode_frame<M: Message>(ty: FrameType, msg: &M) -> Vec<u8> {
    let raw = msg.encode_to_vec();
    encode_raw(ty as u16, raw)
}

fn encode_raw(ty: u16, raw: Vec<u8>) -> Vec<u8> {
    let (flags, payload) = compress(raw);
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.push(flags);
    out.extend_from_slice(&payload);
    out
}

#[cfg(feature = "zstd")]
fn compress(raw: Vec<u8>) -> (u8, Vec<u8>) {
    if raw.len() > COMPRESS_THRESHOLD
        && let Ok(c) = zstd::bulk::compress(&raw, 3)
        && c.len() < raw.len()
    {
        return (FLAG_ZSTD, c);
    }
    (0, raw)
}

#[cfg(not(feature = "zstd"))]
fn compress(raw: Vec<u8>) -> (u8, Vec<u8>) {
    (0, raw)
}

pub fn write_frame<W: Write, M: Message>(w: &mut W, ty: FrameType, msg: &M) -> io::Result<()> {
    w.write_all(&encode_frame(ty, msg))
}

/// Parses a header into `(payload_len, type, flags)`.
pub fn parse_header(h: &[u8; HEADER_LEN]) -> io::Result<(u32, u16, u8)> {
    let len = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame too large: {len}")));
    }
    Ok((len, u16::from_le_bytes([h[4], h[5]]), h[6]))
}

/// Decompresses a payload according to its header flags.
pub fn decode_payload(flags: u8, payload: Vec<u8>) -> io::Result<Vec<u8>> {
    if flags & FLAG_ZSTD == 0 {
        return Ok(payload);
    }
    #[cfg(feature = "zstd")]
    {
        zstd::stream::decode_all(payload.as_slice())
    }
    #[cfg(not(feature = "zstd"))]
    {
        Err(io::Error::new(io::ErrorKind::Unsupported, "zstd frames not supported in this build"))
    }
}

/// Reads one frame. Returns `Ok(None)` on a clean end of stream before a header.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut h = [0u8; HEADER_LEN];
    let mut got = 0;
    while got < HEADER_LEN {
        match r.read(&mut h[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let (len, ty, flags) = parse_header(&h)?;
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok(Some(Frame { ty, payload: decode_payload(flags, payload)? }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small_and_compressed() {
        let hello = v1::Hello { agent_version: "x".into(), token: "t".into(), ..Default::default() };
        let mut buf = encode_frame(FrameType::Hello, &hello);
        let batch = v1::SampleBatch {
            kind: 0,
            time: (0..10_000).map(|i| 1_700_000_000_000_000_000 + i % 7).collect(),
            ..Default::default()
        };
        let big = encode_frame(FrameType::DataBatch, &batch);
        assert_eq!(big[6] & FLAG_ZSTD, FLAG_ZSTD);
        buf.extend_from_slice(&big);
        let mut r = buf.as_slice();
        let f = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(f.frame_type(), Some(FrameType::Hello));
        assert_eq!(f.decode::<v1::Hello>().unwrap(), hello);
        let f = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(f.decode::<v1::SampleBatch>().unwrap(), batch);
        assert!(read_frame(&mut r).unwrap().is_none());
    }
}
