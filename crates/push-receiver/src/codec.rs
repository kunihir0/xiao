#![allow(dead_code)]

use bytes::{Buf, BufMut, BytesMut};
use std::io;
use tokio_util::codec::{Decoder, Encoder};

use crate::error::{Error, Result};

pub const MCS_VERSION: u8 = 41;
pub const LEGACY_MCS_VERSION: u8 = 38;
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameTag {
    HeartbeatPing = 0,
    HeartbeatAck = 1,
    LoginRequest = 2,
    LoginResponse = 3,
    Close = 4,
    Iq = 7,
    DataMessage = 8,
    StreamError = 10,
}

impl TryFrom<u8> for FrameTag {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::HeartbeatPing),
            1 => Ok(Self::HeartbeatAck),
            2 => Ok(Self::LoginRequest),
            3 => Ok(Self::LoginResponse),
            4 => Ok(Self::Close),
            7 => Ok(Self::Iq),
            8 => Ok(Self::DataMessage),
            10 => Ok(Self::StreamError),
            other => Err(Error::UnsupportedTag(other)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McsFrame {
    pub(crate) tag: FrameTag,
    pub(crate) payload: Vec<u8>,
}

impl McsFrame {
    #[must_use]
    pub(crate) const fn new(tag: FrameTag, payload: Vec<u8>) -> Self {
        Self { tag, payload }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodecDirection {
    Client,
    Server,
}

#[derive(Debug)]
pub(crate) struct McsCodec {
    direction: CodecDirection,
    decode_first: bool,
    encode_first: bool,
    in_frame: bool,
    unknown_frames: u8,
}

impl Default for McsCodec {
    fn default() -> Self {
        Self::client()
    }
}

impl McsCodec {
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self {
            direction: CodecDirection::Server,
            decode_first: true,
            encode_first: false,
            in_frame: false,
            unknown_frames: 0,
        }
    }

    #[must_use]
    pub(crate) const fn client() -> Self {
        Self {
            direction: CodecDirection::Client,
            decode_first: false,
            encode_first: true,
            in_frame: false,
            unknown_frames: 0,
        }
    }

    pub(crate) const fn server() -> Self {
        Self {
            direction: CodecDirection::Server,
            decode_first: false,
            encode_first: false,
            in_frame: false,
            unknown_frames: 0,
        }
    }

    pub(crate) fn decode_frame(&mut self, src: &mut BytesMut) -> Result<Option<McsFrame>> {
        loop {
            if self.decode_first {
                let Some(version) = src.first().copied() else {
                    return Ok(None);
                };
                if version != MCS_VERSION && version != LEGACY_MCS_VERSION {
                    return Err(Error::UnsupportedVersion(version));
                }
                src.advance(1);
                self.decode_first = false;
                self.in_frame = true;
            }
            let Some(tag_byte) = src.first().copied() else {
                return Ok(None);
            };
            let Some((size, consumed)) = decode_varint(src.get(1..).unwrap_or_default())? else {
                self.in_frame = true;
                return Ok(None);
            };
            if size > MAX_FRAME_SIZE {
                return Err(Error::FrameTooLarge {
                    actual: size,
                    max: MAX_FRAME_SIZE,
                });
            }
            let header = 1usize.checked_add(consumed).ok_or(Error::InvalidVarint)?;
            let total = header.checked_add(size).ok_or(Error::FrameTooLarge {
                actual: usize::MAX,
                max: MAX_FRAME_SIZE,
            })?;
            if src.len() < total {
                self.in_frame = true;
                return Ok(None);
            }
            src.advance(header);
            let payload = src.split_to(size).to_vec();
            self.in_frame = false;
            let tag = match FrameTag::try_from(tag_byte) {
                Ok(tag) => tag,
                Err(Error::UnsupportedTag(_)) => {
                    self.unknown_frames = self.unknown_frames.saturating_add(1);
                    if self.unknown_frames > 64 {
                        return Err(Error::UnsupportedTag(tag_byte));
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            self.unknown_frames = 0;
            return Ok(Some(McsFrame { tag, payload }));
        }
    }

    pub(crate) fn encode_frame(&mut self, frame: &McsFrame, dst: &mut BytesMut) -> Result<()> {
        if frame.payload.len() > MAX_FRAME_SIZE {
            return Err(Error::FrameTooLarge {
                actual: frame.payload.len(),
                max: MAX_FRAME_SIZE,
            });
        }
        dst.reserve(frame.payload.len() + 7);
        if self.encode_first {
            if self.direction != CodecDirection::Client || frame.tag != FrameTag::LoginRequest {
                return Err(Error::InvalidState(
                    "first client frame must be LoginRequest",
                ));
            }
            dst.put_u8(MCS_VERSION);
            self.encode_first = false;
        }
        dst.put_u8(frame.tag as u8);
        encode_varint(frame.payload.len(), dst);
        dst.extend_from_slice(&frame.payload);
        Ok(())
    }
}

impl Decoder for McsCodec {
    type Item = McsFrame;
    type Error = io::Error;
    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<Self::Item>> {
        self.decode_frame(src)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
    fn decode_eof(&mut self, src: &mut BytesMut) -> io::Result<Option<Self::Item>> {
        if !src.is_empty() || self.in_frame {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                Error::TruncatedFrame.to_string(),
            ));
        }
        Ok(None)
    }
}

impl Encoder<McsFrame> for McsCodec {
    type Error = io::Error;
    fn encode(&mut self, item: McsFrame, dst: &mut BytesMut) -> io::Result<()> {
        self.encode_frame(&item, dst)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

fn decode_varint(src: &[u8]) -> Result<Option<(usize, usize)>> {
    let mut value = 0usize;
    for (index, byte) in src.iter().copied().enumerate().take(5) {
        let shift = index * 7;
        if shift >= usize::BITS as usize {
            return Err(Error::InvalidVarint);
        }
        let part = usize::from(byte & 0x7f);
        if index == 4 && (byte & 0x80 != 0 || part > 0x0f) {
            return Err(Error::InvalidVarint);
        }
        value |= part << shift;
        if byte & 0x80 == 0 {
            if index > 0 && part == 0 && value < (1usize << (index * 7)) {
                return Err(Error::InvalidVarint);
            }
            return Ok(Some((value, index + 1)));
        }
    }
    if src.len() < 5 {
        Ok(None)
    } else {
        Err(Error::InvalidVarint)
    }
}

fn encode_varint(mut value: usize, dst: &mut BytesMut) {
    while value >= 0x80 {
        dst.put_u8((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    dst.put_u8(value as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_overlong_and_overflow_varints() {
        let mut codec = McsCodec::new();
        let mut src = BytesMut::from(&[41, 8, 0x80, 0x00][..]);
        assert!(matches!(
            codec.decode_frame(&mut src),
            Err(Error::InvalidVarint)
        ));
        let mut codec = McsCodec::new();
        let mut src = BytesMut::from(&[41, 8, 0xff, 0xff, 0xff, 0xff, 0x10][..]);
        assert!(matches!(
            codec.decode_frame(&mut src),
            Err(Error::InvalidVarint)
        ));
    }

    #[test]
    fn decodes_reference_data_frame() {
        let mut codec = McsCodec::new();
        let mut src = BytesMut::from(&[41, 8, 3, 1, 2, 3][..]);
        let decoded = codec.decode_frame(&mut src);
        assert!(decoded.is_ok());
        assert_eq!(
            decoded.ok().flatten(),
            Some(McsFrame::new(FrameTag::DataMessage, vec![1, 2, 3]))
        );
    }

    #[test]
    fn consumes_independent_client_and_server_framing_fixtures() {
        let parse = |value: &str| {
            value
                .split_whitespace()
                .filter_map(|pair| u8::from_str_radix(pair, 16).ok())
                .collect::<Vec<_>>()
        };
        let mut client = McsCodec::new();
        let client_fixture = parse(include_str!("../fixtures/client_login_frame.hex"));
        let mut client_bytes = BytesMut::from(client_fixture.as_slice());
        assert_eq!(
            client
                .decode_frame(&mut client_bytes)
                .ok()
                .flatten()
                .map(|frame| frame.tag),
            Some(FrameTag::LoginRequest)
        );
        let mut server = McsCodec::server();
        let server_fixture = parse(include_str!("../fixtures/server_login_response_frame.hex"));
        let mut server_bytes = BytesMut::from(server_fixture.as_slice());
        assert_eq!(
            server
                .decode_frame(&mut server_bytes)
                .ok()
                .flatten()
                .map(|frame| frame.tag),
            Some(FrameTag::LoginResponse)
        );
    }
}
