//! Segment files (`.bseg`): one device's immutable batch of operations.
//!
//! Layout: 96-byte header, body (canonical CBOR array of ops, optionally zstd), and a BLAKE3
//! trailer over header and body.

use crate::Hlc;
use crate::codec::cbor::{Decoder, Encoder};
use crate::codec::{decode_op, encode_op};
use crate::error::{Error, Result};
use crate::model::{DbId, DeviceId, Op};

/// File magic.
pub const MAGIC: [u8; 4] = *b"BSEG";
/// Segment format version written by this build.
pub const VERSION: u16 = 1;
/// Header length in bytes.
pub const HEADER_LEN: usize = 96;
/// Trailer (BLAKE3) length in bytes.
pub const TRAILER_LEN: usize = 32;
/// Flag: body is zstd-compressed.
pub const FLAG_ZSTD: u16 = 1 << 0;
/// Flag: body is encrypted.
pub const FLAG_ENCRYPTED: u16 = 1 << 1;
const KNOWN_FLAGS: u16 = FLAG_ZSTD | FLAG_ENCRYPTED;

/// Largest uncompressed body a reader accepts (decompression bomb guard).
pub const MAX_PLAIN_LEN: usize = 8 * 1024 * 1024;
/// Largest op count a reader accepts.
pub const MAX_OPS: u32 = 100_000;
/// Body size writers should stay under.
pub const TARGET_PLAIN_LEN: usize = 1024 * 1024;
/// Op count writers should stay under.
pub const TARGET_OPS: usize = 10_000;

const ZSTD_LEVEL: i32 = 3;

/// A decoded segment.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    /// Database the segment belongs to.
    pub db_id: DbId,
    /// Device that wrote it.
    pub device_id: DeviceId,
    /// Sequence number, from 1.
    pub seq: u64,
    /// Operations with strictly increasing HLCs.
    pub ops: Vec<Op>,
}

/// Ids a reader expects, from the manifest and the logical key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expected {
    /// Database id from the manifest.
    pub db_id: DbId,
    /// Device id from the key.
    pub device_id: DeviceId,
    /// Sequence number from the key.
    pub seq: u64,
}

fn check_ops(ops: &[Op]) -> Result<()> {
    if ops.is_empty() {
        return Err(Error::invalid_input("segment needs at least one op"));
    }
    if ops.len() > MAX_OPS as usize {
        return Err(Error::invalid_input("segment has more than 100000 ops"));
    }
    if ops.windows(2).any(|w| w[0].hlc >= w[1].hlc) {
        return Err(Error::invalid_input(
            "segment op HLCs must strictly increase",
        ));
    }
    Ok(())
}

/// Encodes the plain body (CBOR array of ops).
pub fn encode_plain(ops: &[Op]) -> Result<Vec<u8>> {
    let mut e = Encoder::new();
    e.array(ops.len());
    for op in ops {
        encode_op(&mut e, op)?;
    }
    Ok(e.into_bytes())
}

/// Encodes an unencrypted segment.
pub fn encode(segment: &Segment, compress: bool) -> Result<Vec<u8>> {
    check_ops(&segment.ops)?;
    if segment.seq == 0 {
        return Err(Error::invalid_input("segment seq starts at 1"));
    }
    let plain = encode_plain(&segment.ops)?;
    if plain.len() > MAX_PLAIN_LEN {
        return Err(Error::invalid_input("segment body is larger than 8 MiB"));
    }
    let (flags, body) = if compress {
        let body = zstd::bulk::compress(&plain, ZSTD_LEVEL)
            .map_err(|e| Error::new(crate::ErrorKind::Internal, "zstd failed").with_source(e))?;
        (FLAG_ZSTD, body)
    } else {
        (0, plain)
    };
    let body_len = u32::try_from(body.len()).expect("bounded by MAX_PLAIN_LEN");
    let mut out = Vec::with_capacity(HEADER_LEN + body.len() + TRAILER_LEN);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_be_bytes());
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(segment.db_id.as_bytes());
    out.extend_from_slice(segment.device_id.as_bytes());
    out.extend_from_slice(&segment.seq.to_be_bytes());
    out.extend_from_slice(&segment.ops[0].hlc.as_u64().to_be_bytes());
    out.extend_from_slice(
        &segment.ops[segment.ops.len() - 1]
            .hlc
            .as_u64()
            .to_be_bytes(),
    );
    out.extend_from_slice(&(segment.ops.len() as u32).to_be_bytes());
    out.extend_from_slice(&body_len.to_be_bytes());
    out.extend_from_slice(&[0u8; 24]);
    debug_assert_eq!(out.len(), HEADER_LEN);
    out.extend_from_slice(&body);
    let trailer = blake3::hash(&out);
    out.extend_from_slice(trailer.as_bytes());
    Ok(out)
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(b[at..at + 2].try_into().expect("in bounds"))
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().expect("in bounds"))
}

fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().expect("in bounds"))
}

fn id16(b: &[u8], at: usize) -> [u8; 16] {
    b[at..at + 16].try_into().expect("in bounds")
}

/// Decompresses a single zstd frame, refusing output above `limit`.
pub(crate) fn unzstd(body: &[u8], limit: usize) -> Result<Vec<u8>> {
    let frame = zstd::zstd_safe::find_frame_compressed_size(body)
        .map_err(|_| Error::corrupt("zstd: malformed frame"))?;
    if frame != body.len() {
        return Err(Error::corrupt("zstd: body must be exactly one frame"));
    }
    if let Ok(Some(size)) = zstd::zstd_safe::get_frame_content_size(body)
        && size > limit as u64
    {
        return Err(Error::corrupt("zstd: content is larger than the limit"));
    }
    zstd::bulk::decompress(body, limit).map_err(|_| Error::corrupt("zstd: decompression failed"))
}

/// Decodes and fully validates a segment. On any failure no op is returned.
pub fn decode(bytes: &[u8], expected: &Expected) -> Result<Segment> {
    // 1. Size, magic, version, flags.
    if bytes.len() < HEADER_LEN + TRAILER_LEN || bytes[..4] != MAGIC {
        return Err(Error::corrupt("segment: bad magic or too short"));
    }
    match be16(bytes, 4) {
        VERSION => {}
        v if v > VERSION => {
            return Err(Error::format_too_new(format!("segment version {v}")));
        }
        v => return Err(Error::corrupt(format!("segment: bad version {v}"))),
    }
    let flags = be16(bytes, 6);
    if flags & !KNOWN_FLAGS != 0 {
        return Err(Error::format_too_new(format!(
            "segment: unknown flags {flags:#06x}"
        )));
    }

    // 2. Ids against the manifest and the key.
    let db_id = DbId::from_bytes(id16(bytes, 8));
    let device_id = DeviceId::from_bytes(id16(bytes, 24));
    let seq = be64(bytes, 40);
    if db_id != expected.db_id || device_id != expected.device_id || seq != expected.seq {
        return Err(Error::corrupt("segment: ids do not match its key"));
    }
    if seq == 0 {
        return Err(Error::corrupt("segment: seq 0"));
    }

    // 3. Length and trailer.
    let body_len = be32(bytes, 68) as usize;
    if HEADER_LEN
        .checked_add(body_len)
        .and_then(|n| n.checked_add(TRAILER_LEN))
        != Some(bytes.len())
    {
        return Err(Error::corrupt("segment: length does not match body_len"));
    }
    let (signed, trailer) = bytes.split_at(HEADER_LEN + body_len);
    if blake3::hash(signed).as_bytes() != trailer {
        return Err(Error::corrupt("segment: trailer mismatch"));
    }

    // 4. Encryption.
    let nonce = &bytes[72..96];
    if flags & FLAG_ENCRYPTED != 0 {
        return Err(Error::format_too_new(
            "segment is encrypted; this build has no encryption support",
        ));
    }
    if nonce.iter().any(|&b| b != 0) {
        return Err(Error::corrupt(
            "segment: nonce must be zero when not encrypted",
        ));
    }

    // 5. Body.
    let op_count = be32(bytes, 64);
    if op_count == 0 || op_count > MAX_OPS {
        return Err(Error::corrupt("segment: op_count out of range"));
    }
    let body = &signed[HEADER_LEN..];
    let plain_owned;
    let plain = if flags & FLAG_ZSTD != 0 {
        plain_owned = unzstd(body, MAX_PLAIN_LEN)?;
        plain_owned.as_slice()
    } else {
        if body.len() > MAX_PLAIN_LEN {
            return Err(Error::corrupt("segment: body is larger than 8 MiB"));
        }
        body
    };
    let mut d = Decoder::new(plain);
    let n = d.array()?;
    if n != op_count as usize {
        return Err(Error::corrupt("segment: op_count does not match the body"));
    }
    let mut ops = Vec::with_capacity(n);
    for _ in 0..n {
        ops.push(decode_op(&mut d)?);
    }
    d.finish()?;
    if ops.windows(2).any(|w| w[0].hlc >= w[1].hlc) {
        return Err(Error::corrupt("segment: op HLCs do not strictly increase"));
    }
    if ops[0].hlc != Hlc::from_u64(be64(bytes, 48))
        || ops[n - 1].hlc != Hlc::from_u64(be64(bytes, 56))
    {
        return Err(Error::corrupt(
            "segment: first_hlc or last_hlc does not match",
        ));
    }
    Ok(Segment {
        db_id,
        device_id,
        seq,
        ops,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use crate::model::{CollectionName, DocId, FieldName, OpKind, Value};

    fn sample(n: usize) -> Segment {
        let ops = (0..n)
            .map(|i| Op {
                hlc: Hlc::from_parts(1_700_000_000_000, i as u16).unwrap(),
                collection: CollectionName::new("notes").unwrap(),
                doc_id: DocId::new(&format!("n{i}")).unwrap(),
                kind: OpKind::SetFields(
                    [(FieldName::new("title").unwrap(), Value::from("hello world"))].into(),
                ),
            })
            .collect();
        Segment {
            db_id: DbId::from_bytes([1; 16]),
            device_id: DeviceId::from_bytes([2; 16]),
            seq: 7,
            ops,
        }
    }

    fn expected(s: &Segment) -> Expected {
        Expected {
            db_id: s.db_id,
            device_id: s.device_id,
            seq: s.seq,
        }
    }

    fn reseal(mut bytes: Vec<u8>) -> Vec<u8> {
        let n = bytes.len() - TRAILER_LEN;
        let h = blake3::hash(&bytes[..n]);
        bytes[n..].copy_from_slice(h.as_bytes());
        bytes
    }

    #[test]
    fn round_trip_plain_and_zstd() {
        let s = sample(50);
        for compress in [false, true] {
            let bytes = encode(&s, compress).unwrap();
            assert_eq!(decode(&bytes, &expected(&s)).unwrap(), s);
        }
        assert!(encode(&s, true).unwrap().len() < encode(&s, false).unwrap().len());
    }

    #[test]
    fn header_fields() {
        let s = sample(2);
        let b = encode(&s, false).unwrap();
        assert_eq!(&b[..4], b"BSEG");
        assert_eq!(be16(&b, 4), 1);
        assert_eq!(be64(&b, 40), 7);
        assert_eq!(be64(&b, 48), s.ops[0].hlc.as_u64());
        assert_eq!(be64(&b, 56), s.ops[1].hlc.as_u64());
        assert_eq!(be32(&b, 64), 2);
        assert_eq!(be32(&b, 68) as usize, b.len() - HEADER_LEN - TRAILER_LEN);
    }

    #[test]
    fn writer_rejects_bad_input() {
        let mut s = sample(2);
        s.ops.swap(0, 1);
        assert!(encode(&s, false).is_err());
        assert!(encode(&sample(0), false).is_err());
        let mut s = sample(1);
        s.seq = 0;
        assert!(encode(&s, false).is_err());
    }

    #[test]
    fn reader_checks_every_step() {
        let s = sample(3);
        let good = encode(&s, false).unwrap();
        let exp = expected(&s);
        let kind = |b: &[u8], e: &Expected| decode(b, e).unwrap_err().kind();

        assert_eq!(kind(&good[..100], &exp), ErrorKind::Corrupt);
        let mut b = good.clone();
        b[0] = b'X';
        assert_eq!(kind(&b, &exp), ErrorKind::Corrupt);
        let mut b = good.clone();
        b[5] = 2;
        assert_eq!(kind(&b, &exp), ErrorKind::FormatTooNew);
        let mut b = good.clone();
        b[7] = 0x04;
        assert_eq!(kind(&b, &exp), ErrorKind::FormatTooNew);
        let mut b = good.clone();
        b[7] = 0x02;
        assert_eq!(kind(&reseal(b), &exp), ErrorKind::FormatTooNew);

        for wrong in [
            Expected { seq: 8, ..exp },
            Expected {
                device_id: DeviceId::from_bytes([9; 16]),
                ..exp
            },
            Expected {
                db_id: DbId::from_bytes([9; 16]),
                ..exp
            },
        ] {
            assert_eq!(kind(&good, &wrong), ErrorKind::Corrupt);
        }

        let mut b = good.clone();
        let last = b.len() - 1;
        b[last] ^= 1;
        assert_eq!(kind(&b, &exp), ErrorKind::Corrupt, "trailer");
        let mut b = good.clone();
        b[HEADER_LEN + 3] ^= 1;
        assert_eq!(kind(&b, &exp), ErrorKind::Corrupt, "body bit flip");
        let mut b = good.clone();
        b[80] = 1;
        assert_eq!(kind(&reseal(b), &exp), ErrorKind::Corrupt, "nonce");
        let mut b = good.clone();
        b[67] = 4;
        assert_eq!(kind(&reseal(b), &exp), ErrorKind::Corrupt, "op_count");
        let mut b = good.clone();
        b[63] ^= 1;
        assert_eq!(kind(&reseal(b), &exp), ErrorKind::Corrupt, "last_hlc");
        let mut b = good.clone();
        b.insert(HEADER_LEN, 0);
        assert_eq!(kind(&reseal(b), &exp), ErrorKind::Corrupt, "body_len");
    }

    #[test]
    fn zstd_bomb_and_multi_frame_are_rejected() {
        let s = sample(1);
        let exp = expected(&s);
        let build = |body: Vec<u8>| {
            let mut b = encode(&s, false).unwrap();
            b.truncate(HEADER_LEN);
            b[6..8].copy_from_slice(&FLAG_ZSTD.to_be_bytes());
            b[68..72].copy_from_slice(&(body.len() as u32).to_be_bytes());
            b.extend_from_slice(&body);
            b.extend_from_slice(&[0; TRAILER_LEN]);
            reseal(b)
        };
        let bomb = zstd::bulk::compress(&vec![0u8; MAX_PLAIN_LEN + 1], 3).unwrap();
        assert_eq!(
            decode(&build(bomb), &exp).unwrap_err().kind(),
            ErrorKind::Corrupt
        );

        let one = zstd::bulk::compress(&encode_plain(&s.ops).unwrap(), 3).unwrap();
        assert!(decode(&build(one.clone()), &exp).is_ok());
        let two = [one.clone(), one].concat();
        assert_eq!(
            decode(&build(two), &exp).unwrap_err().kind(),
            ErrorKind::Corrupt
        );
    }
}
