use crate::record::Record;

/// Fixed fields after the 4-byte length prefix, per spec §"Record layout":
/// offset(8) timestamp_ns(8) schema_id(4) key_len(2) value_len(4) = 26 bytes.
/// Total header incl. length prefix = 30 B; CRC32C trailer = 4 B.
const HEADER_AFTER_LEN: usize = 8 + 8 + 4 + 2 + 4;

#[derive(Debug, PartialEq)]
pub enum WalDecodeError {
    /// Not enough bytes for a full framed record yet (torn tail / EOF).
    Incomplete,
    /// CRC32C over offset..value did not match the trailer.
    CrcMismatch,
    /// length field is implausible (key/value exceed declared frame).
    Malformed,
}

/// Appends one framed record to `out`. CRC32C (Castagnoli) covers
/// `offset` through `value` (everything except the length prefix and CRC).
pub fn encode_record(r: &Record, out: &mut Vec<u8>) {
    let key_len: u16 = r.key.len() as u16;
    let value_len: u32 = r.value.len() as u32;
    let body_len: usize = HEADER_AFTER_LEN + r.key.len() + r.value.len();
    // length = bytes from `offset` through `value`, excluding CRC.
    out.extend_from_slice(&(body_len as u32).to_le_bytes());

    let start: usize = out.len();
    out.extend_from_slice(&r.offset.to_le_bytes());
    out.extend_from_slice(&r.timestamp_ns.to_le_bytes());
    out.extend_from_slice(&r.schema_id.to_le_bytes());
    out.extend_from_slice(&key_len.to_le_bytes());
    out.extend_from_slice(&value_len.to_le_bytes());
    out.extend_from_slice(&r.key);
    out.extend_from_slice(&r.value);

    let crc: u32 = crc32c::crc32c(&out[start..]);
    out.extend_from_slice(&crc.to_le_bytes());
}

/// Decodes one record from the front of `buf`. Returns the record and the
/// number of bytes consumed (length prefix + body + CRC trailer).
pub fn decode_record(buf: &[u8]) -> Result<(Record, usize), WalDecodeError> {
    if buf.len() < 4 {
        return Err(WalDecodeError::Incomplete);
    }
    let body_len: usize = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
    let total: usize = 4 + body_len + 4;
    if buf.len() < total {
        return Err(WalDecodeError::Incomplete);
    }
    if body_len < HEADER_AFTER_LEN {
        return Err(WalDecodeError::Malformed);
    }
    let body: &[u8] = &buf[4..4 + body_len];
    let crc_stored: u32 = u32::from_le_bytes(buf[4 + body_len..total].try_into().unwrap());
    if crc32c::crc32c(body) != crc_stored {
        return Err(WalDecodeError::CrcMismatch);
    }

    let offset: i64 = i64::from_le_bytes(body[0..8].try_into().unwrap());
    let timestamp_ns: i64 = i64::from_le_bytes(body[8..16].try_into().unwrap());
    let schema_id: u32 = u32::from_le_bytes(body[16..20].try_into().unwrap());
    let key_len: usize = u16::from_le_bytes(body[20..22].try_into().unwrap()) as usize;
    let value_len: usize = u32::from_le_bytes(body[22..26].try_into().unwrap()) as usize;
    if HEADER_AFTER_LEN + key_len + value_len != body_len {
        return Err(WalDecodeError::Malformed);
    }
    let key: Vec<u8> = body[26..26 + key_len].to_vec();
    let value: Vec<u8> = body[26 + key_len..26 + key_len + value_len].to_vec();

    Ok((
        Record {
            offset,
            timestamp_ns,
            schema_id,
            key,
            value,
        },
        total,
    ))
}

/// One-pass recovery scan: decode records until the first failure. Returns the
/// recovered records and the number of leading bytes that are valid (the
/// truncation point — spec §"Why the CRC is at the end").
pub fn scan_wal(buf: &[u8]) -> (Vec<Record>, usize) {
    let mut records: Vec<Record> = Vec::new();
    let mut pos: usize = 0;
    while pos < buf.len() {
        match decode_record(&buf[pos..]) {
            Ok((r, consumed)) => {
                records.push(r);
                pos += consumed;
            }
            Err(_) => break,
        }
    }
    (records, pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Record;

    fn rec() -> Record {
        Record {
            offset: 42,
            timestamp_ns: 1_700_000_000_000_000_000,
            schema_id: 7,
            key: vec![1, 2],
            value: vec![3, 4, 5],
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let r = rec();
        let mut buf = Vec::new();
        encode_record(&r, &mut buf);
        let (decoded, consumed) = decode_record(&buf).unwrap();
        assert_eq!(decoded, r);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn truncated_tail_fails_decode() {
        let r = rec();
        let mut buf = Vec::new();
        encode_record(&r, &mut buf);
        buf.truncate(buf.len() - 1);
        assert!(matches!(
            decode_record(&buf),
            Err(WalDecodeError::Incomplete)
        ));
    }

    #[test]
    fn corrupt_body_fails_crc() {
        let r = rec();
        let mut buf = Vec::new();
        encode_record(&r, &mut buf);
        let n = buf.len();
        buf[n - 5] ^= 0xFF; // flip a byte inside value, before CRC trailer
        assert!(matches!(
            decode_record(&buf),
            Err(WalDecodeError::CrcMismatch)
        ));
    }

    #[test]
    fn scan_stops_at_first_invalid() {
        let mut buf = Vec::new();
        encode_record(&rec(), &mut buf);
        let good_len = buf.len();
        encode_record(&rec(), &mut buf);
        let n = buf.len();
        buf[n - 3] ^= 0xFF; // corrupt second record
        let (records, valid_bytes) = scan_wal(&buf);
        assert_eq!(records.len(), 1);
        assert_eq!(valid_bytes, good_len);
    }
}
