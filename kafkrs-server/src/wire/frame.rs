use bytes::{Bytes, BytesMut};
use kafkrs_models::wire::v1::Command;
use prost::Message;

pub const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024;

/// One decoded frame: the protobuf Command plus the raw payload section.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub command: Command,
    pub payload: Bytes,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FrameError {
    #[error("frame too large: {0} bytes exceeds the {MAX_FRAME_SIZE} byte limit")]
    TooLarge(usize),
    #[error("frame malformed: {0}")]
    Malformed(&'static str),
    #[error("protobuf decode failed")]
    ProstDecode,
}

/// Encode a Frame to the on-wire bytes (total_size + command_size + command +
/// payload). Returns an error if the resulting frame would exceed MAX_FRAME_SIZE.
pub fn encode_frame(frame: &Frame) -> Result<Bytes, FrameError> {
    let command_bytes_len = frame.command.encoded_len();
    let payload_len = frame.payload.len();
    // total_size excludes itself but includes the 4-byte command_size field.
    let total_size = 4usize
        .checked_add(command_bytes_len)
        .and_then(|n| n.checked_add(payload_len))
        .ok_or(FrameError::TooLarge(usize::MAX))?;
    // Whole-frame check: outer 4 bytes (total_size field) + total_size.
    let whole = total_size.checked_add(4).ok_or(FrameError::TooLarge(usize::MAX))?;
    if whole > MAX_FRAME_SIZE {
        return Err(FrameError::TooLarge(whole));
    }
    let mut buf = BytesMut::with_capacity(whole);
    buf.extend_from_slice(&(total_size as u32).to_be_bytes());
    buf.extend_from_slice(&(command_bytes_len as u32).to_be_bytes());
    frame
        .command
        .encode(&mut buf)
        .map_err(|_| FrameError::ProstDecode)?;
    buf.extend_from_slice(&frame.payload);
    Ok(buf.freeze())
}

/// Decode a frame body (everything AFTER the outer total_size prefix that
/// LengthDelimitedCodec strips). The input bytes are: command_size (4 B) +
/// command (command_size B) + payload (rest).
pub fn decode_frame_body(body: &[u8]) -> Result<Frame, FrameError> {
    if body.len() < 4 {
        return Err(FrameError::Malformed("body shorter than command_size prefix"));
    }
    let command_size = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
    if body.len() < 4 + command_size {
        return Err(FrameError::Malformed("body shorter than declared command_size"));
    }
    let command_bytes = &body[4..4 + command_size];
    let command = Command::decode(command_bytes).map_err(|_| FrameError::ProstDecode)?;
    let payload = Bytes::copy_from_slice(&body[4 + command_size..]);
    Ok(Frame { command, payload })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kafkrs_models::wire::v1::{command::Body, PingRequest};

    fn ping_command(correlation_id: u64) -> Command {
        Command {
            correlation_id,
            body: Some(Body::Ping(PingRequest {})),
        }
    }

    #[test]
    fn encode_empty_payload_command_has_correct_size_prefixes() {
        let frame = Frame {
            command: ping_command(7),
            payload: Bytes::new(),
        };
        let bytes = encode_frame(&frame).expect("encode");

        // First 4 bytes: total_size (big-endian u32), which excludes itself.
        let total_size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        assert_eq!(bytes.len(), 4 + total_size);

        // Next 4 bytes: command_size (big-endian u32).
        let command_size = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;

        // For an empty payload, total_size == 4 (command_size) + command_size.
        assert_eq!(total_size, 4 + command_size);
    }

    #[test]
    fn encode_decode_roundtrip_no_payload() {
        let frame = Frame {
            command: ping_command(42),
            payload: Bytes::new(),
        };
        let bytes = encode_frame(&frame).expect("encode");
        // Skip the outer 4-byte total_size; that's what LengthDelimitedCodec strips.
        let decoded = decode_frame_body(&bytes[4..]).expect("decode");
        assert_eq!(decoded.command.correlation_id, 42);
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn encode_decode_roundtrip_with_payload() {
        let payload = Bytes::from_static(b"hello-world-payload-bytes");
        let frame = Frame {
            command: ping_command(99),
            payload: payload.clone(),
        };
        let bytes = encode_frame(&frame).expect("encode");
        let decoded = decode_frame_body(&bytes[4..]).expect("decode");
        assert_eq!(decoded.command.correlation_id, 99);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn encode_rejects_too_large() {
        // Build a Command whose serialized size alone exceeds the cap.
        let huge = Command {
            correlation_id: 0,
            body: Some(Body::Connect(kafkrs_models::wire::v1::ConnectRequest {
                protocol_version: 1,
                client_id: "x".repeat(MAX_FRAME_SIZE + 1024),
                auth_data: vec![],
            })),
        };
        let frame = Frame {
            command: huge,
            payload: Bytes::new(),
        };
        let err = encode_frame(&frame).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_)));
    }

    #[test]
    fn decode_malformed_too_short() {
        let err = decode_frame_body(&[0, 0, 0]).unwrap_err();
        assert!(matches!(err, FrameError::Malformed(_)));
    }

    #[test]
    fn decode_command_size_exceeds_body() {
        // command_size says 100, but body has only 4 more bytes.
        let mut buf = vec![];
        buf.extend_from_slice(&100u32.to_be_bytes());
        buf.extend_from_slice(b"abcd");
        let err = decode_frame_body(&buf).unwrap_err();
        assert!(matches!(err, FrameError::Malformed(_)));
    }
}
