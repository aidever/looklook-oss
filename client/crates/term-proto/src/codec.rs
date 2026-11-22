//! Wire framing and single-connection streaming zstd compression / decompression.
//!
//! Each connection uses one persistent streaming zstd context for `ServerMsg` frames, flushing
//! immediately after every message so viewers render every frame with minimum latency. Native
//! targets use `zstd` for encoding and decoding; `wasm32-unknown-unknown` decodes via pure-Rust
//! `ruzstd` (no C dependencies, encoding only runs on native side).

use std::collections::VecDeque;
use std::fmt;
#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::messages::{ClientMsg, ServerMsg};

/// Maximum allowed uncompressed wire message size (16 MiB).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// 128 KiB sliding zstd compression window (matching `gateway/term_zstd.rs`).
pub const ZSTD_WINDOW_LOG: u32 = 17;
pub const ZSTD_COMPRESSION_LEVEL: i32 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecError {
    Serialize(String),
    Deserialize(String),
    Compress(String),
    Decompress(String),
    FrameTooLarge(usize),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialize(e) => write!(f, "serialize error: {e}"),
            Self::Deserialize(e) => write!(f, "deserialize error: {e}"),
            Self::Compress(e) => write!(f, "zstd compress error: {e}"),
            Self::Decompress(e) => write!(f, "zstd decompress error: {e}"),
            Self::FrameTooLarge(sz) => write!(f, "wire frame exceeds limit ({sz} bytes)"),
        }
    }
}

impl std::error::Error for CodecError {}

/// bincode options for `looklook-term.v1` packets: variable-length integers. A cell is mostly
/// small numbers (ASCII char, style id, width, spacer tag, empty `extra` length): fixed-width
/// encoding spent ~21 bytes per cell, varint ~5, which matters for full-screen redraws
/// (vim paging, `less`) even after zstd.
fn wire() -> impl bincode::Options {
    use bincode::Options as _;
    bincode::DefaultOptions::new().with_varint_encoding().with_little_endian()
}

/// Serialize any protocol message into a `[u32 LE len][bincode payload]` packet.
pub fn encode_raw_packet<T: Serialize>(msg: &T) -> Result<Vec<u8>, CodecError> {
    use bincode::Options as _;
    let payload = wire().serialize(msg).map_err(|e| CodecError::Serialize(e.to_string()))?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge(payload.len()));
    }
    let len = payload.len() as u32;
    let mut packet = Vec::with_capacity(4 + payload.len());
    packet.extend_from_slice(&len.to_le_bytes());
    packet.extend_from_slice(&payload);
    Ok(packet)
}

/// Incremental length-prefixed packet deframer reading from decompressed byte streams.
#[derive(Debug, Default)]
pub struct PacketFramer {
    buf: Vec<u8>,
}

impl PacketFramer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn next_packet<T: DeserializeOwned>(&mut self) -> Result<Option<T>, CodecError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if len > MAX_FRAME_BYTES {
            return Err(CodecError::FrameTooLarge(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        use bincode::Options as _;
        let decoded = wire()
            .deserialize::<T>(&self.buf[4..4 + len])
            .map_err(|e| CodecError::Deserialize(e.to_string()))?;
        self.buf.drain(..4 + len);
        Ok(Some(decoded))
    }

    pub fn drain_all<T: DeserializeOwned>(&mut self) -> Result<Vec<T>, CodecError> {
        let mut out = Vec::new();
        while let Some(msg) = self.next_packet()? {
            out.push(msg);
        }
        Ok(out)
    }
}

/// Helper for Client -> Server un-zstd packets (both native and wasm32).
pub fn encode_client_msg(msg: &ClientMsg) -> Result<Vec<u8>, CodecError> {
    encode_raw_packet(msg)
}

pub fn decode_client_msg(bytes: &[u8]) -> Result<ClientMsg, CodecError> {
    let mut framer = PacketFramer::new();
    framer.push(bytes);
    framer
        .next_packet()?
        .ok_or_else(|| CodecError::Deserialize("incomplete client frame".into()))
}

// ---------------------------------------------------------------------------
// Native streaming zstd encoder/decoder (single context per connection, flushed per message)
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
pub struct ZstdStreamEncoder {
    encoder: zstd::stream::write::Encoder<'static, Vec<u8>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl fmt::Debug for ZstdStreamEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZstdStreamEncoder").finish_non_exhaustive()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl ZstdStreamEncoder {
    pub fn new() -> Result<Self, CodecError> {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), ZSTD_COMPRESSION_LEVEL)
            .map_err(|e| CodecError::Compress(e.to_string()))?;
        let _ = encoder.window_log(ZSTD_WINDOW_LOG);
        Ok(Self { encoder })
    }

    /// Encode and immediately flush a message on the connection's zstd stream, returning the
    /// compressed bytes produced by this message.
    pub fn encode_server_msg(&mut self, msg: &ServerMsg) -> Result<Vec<u8>, CodecError> {
        let packet = encode_raw_packet(msg)?;
        self.encoder
            .write_all(&packet)
            .map_err(|e| CodecError::Compress(e.to_string()))?;
        self.encoder
            .flush()
            .map_err(|e| CodecError::Compress(e.to_string()))?;
        Ok(std::mem::take(self.encoder.get_mut()))
    }

    /// Finish the zstd stream (writes the epilogue empty last-block marker), returning trailing bytes.
    pub fn finish(self) -> Result<Vec<u8>, CodecError> {
        self.encoder
            .finish()
            .map_err(|e| CodecError::Compress(e.to_string()))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub struct ZstdStreamDecoder {
    decoder: zstd::stream::write::Decoder<'static, Vec<u8>>,
    framer: PacketFramer,
}

#[cfg(not(target_arch = "wasm32"))]
impl fmt::Debug for ZstdStreamDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZstdStreamDecoder").finish_non_exhaustive()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for ZstdStreamDecoder {
    fn default() -> Self {
        Self::new().expect("zstd decoder init")
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl ZstdStreamDecoder {
    pub fn new() -> Result<Self, CodecError> {
        let decoder = zstd::stream::write::Decoder::new(Vec::new())
            .map_err(|e| CodecError::Decompress(e.to_string()))?;
        Ok(Self {
            decoder,
            framer: PacketFramer::new(),
        })
    }

    /// Feed a compressed chunk received from the connection and return all newly decoded `ServerMsg`s.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<ServerMsg>, CodecError> {
        self.decoder
            .write_all(chunk)
            .map_err(|e| CodecError::Decompress(e.to_string()))?;
        self.decoder
            .flush()
            .map_err(|e| CodecError::Decompress(e.to_string()))?;
        let plain = std::mem::take(self.decoder.get_mut());
        if !plain.is_empty() {
            self.framer.push(&plain);
        }
        self.framer.drain_all()
    }
}

// ---------------------------------------------------------------------------
// Pure-Rust ruzstd decoder (available everywhere including wasm32-unknown-unknown)
// ---------------------------------------------------------------------------

/// Pure-Rust zstd decoder backed by `ruzstd` for `wasm32-unknown-unknown` compatibility.
/// Note: `ruzstd`'s internal `DecodeBuffer` retains up to `window_size` bytes on incomplete
/// frames until more data arrives or the frame closes (`finish`).
pub struct RuzstdStreamDecoder {
    frame: ruzstd::decoding::FrameDecoder,
    in_buf: VecDeque<u8>,
    initialized: bool,
    framer: PacketFramer,
}

impl fmt::Debug for RuzstdStreamDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuzstdStreamDecoder")
            .field("initialized", &self.initialized)
            .field("buffered_in", &self.in_buf.len())
            .finish()
    }
}

impl Default for RuzstdStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl RuzstdStreamDecoder {
    pub fn new() -> Self {
        Self {
            frame: ruzstd::decoding::FrameDecoder::new(),
            in_buf: VecDeque::new(),
            initialized: false,
            framer: PacketFramer::new(),
        }
    }

    /// Feed compressed bytes into the pure-Rust decoder and extract any complete `ServerMsg`s.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<ServerMsg>, CodecError> {
        self.in_buf.extend(chunk.iter().copied());
        if self.in_buf.is_empty() {
            return Ok(Vec::new());
        }

        let total_len = self.in_buf.len();
        let mut cursor: &[u8] = self.in_buf.make_contiguous();

        if !self.initialized {
            match self.frame.init(&mut cursor) {
                Ok(()) => self.initialized = true,
                Err(_) => return Ok(Vec::new()),
            }
        }

        let mut tmp_out = vec![0u8; 64 * 1024];
        loop {
            let (read_bytes, written_bytes) = self
                .frame
                .decode_from_to(cursor, &mut tmp_out)
                .map_err(|e| CodecError::Decompress(format!("{e:?}")))?;
            if written_bytes > 0 {
                self.framer.push(&tmp_out[..written_bytes]);
            }
            if read_bytes > 0 {
                cursor = &cursor[read_bytes..];
            }
            if read_bytes == 0 && written_bytes == 0 {
                break;
            }
            if self.frame.is_finished() {
                while self.frame.can_collect() > 0 {
                    use std::io::Read as _;
                    let n = self
                        .frame
                        .read(&mut tmp_out)
                        .map_err(|e| CodecError::Decompress(e.to_string()))?;
                    if n == 0 {
                        break;
                    }
                    self.framer.push(&tmp_out[..n]);
                }
                self.initialized = false;
                if cursor.is_empty() {
                    break;
                }
                if self.frame.init(&mut cursor).is_ok() {
                    self.initialized = true;
                } else {
                    break;
                }
            }
        }

        let consumed = total_len - cursor.len();
        self.in_buf.drain(..consumed);
        self.framer.drain_all()
    }
}
