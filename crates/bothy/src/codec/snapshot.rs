//! Snapshot files (`.bsnap`): the full state up to a version vector, in chunks that can be
//! processed one at a time.

use std::collections::BTreeMap;

use crate::Hlc;
use crate::codec::cbor::{Decoder, Encoder};
use crate::codec::segment::unzstd;
use crate::codec::state::{decode_entry, encode_entry};
use crate::error::{Error, ErrorKind, Result};
use crate::merge::DocState;
use crate::model::{CollectionName, DbId, DeviceId, DocId};

/// File magic.
pub const MAGIC: [u8; 4] = *b"BSNP";
/// Snapshot format version written by this build.
pub const VERSION: u16 = 1;
/// Flag: chunks are zstd-compressed.
pub const FLAG_ZSTD: u16 = 1 << 0;
/// Flag: chunks are encrypted.
pub const FLAG_ENCRYPTED: u16 = 1 << 1;
const KNOWN_FLAGS: u16 = FLAG_ZSTD | FLAG_ENCRYPTED;
const FIXED_HEADER_LEN: usize = 56;
const NONCE_LEN: usize = 24;
const TRAILER_LEN: usize = 32;

/// Plain chunk size writers aim for.
pub const TARGET_CHUNK_LEN: usize = 1024 * 1024;
/// Largest plain chunk a reader accepts. A chunk exceeds the target only when it holds a single
/// entry that is larger than the target.
pub const MAX_CHUNK_LEN: usize = 8 * 1024 * 1024;

const ZSTD_LEVEL: i32 = 3;

/// Version vector: for each device, the highest segment seq the snapshot covers.
pub type VersionVector = BTreeMap<DeviceId, u64>;

/// Snapshot identity and coverage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotHeader {
    /// Database id.
    pub db_id: DbId,
    /// Device that produced the snapshot.
    pub device_id: DeviceId,
    /// HLC at production time; also part of the file's key.
    pub hlc: Hlc,
    /// Segments covered.
    pub vv: VersionVector,
}

fn encode_vv(vv: &VersionVector) -> Vec<u8> {
    let mut e = Encoder::new();
    e.map(vv.len());
    // All keys are 16-byte strings, so canonical order is byte order of the ids.
    for (device, seq) in vv {
        e.bytes(device.as_bytes());
        e.uint(*seq);
    }
    e.into_bytes()
}

fn decode_vv(bytes: &[u8]) -> Result<VersionVector> {
    let mut d = Decoder::new(bytes);
    let n = d.map()?;
    let mut vv = BTreeMap::new();
    let mut prev = None;
    for _ in 0..n {
        let (key, encoded) = d.map_bytes_key(prev)?;
        prev = Some(encoded);
        let id: [u8; 16] = key
            .try_into()
            .map_err(|_| Error::corrupt("snapshot: vv key must be 16 bytes"))?;
        vv.insert(DeviceId::from_bytes(id), d.uint()?);
    }
    d.finish()?;
    Ok(vv)
}

/// Builds a snapshot from entries pushed in `(collection, doc_id)` byte order.
#[derive(Debug)]
pub struct SnapshotWriter {
    header: SnapshotHeader,
    compress: bool,
    chunk_target: usize,
    chunks: Vec<Vec<u8>>,
    pending: Vec<u8>,
    pending_count: usize,
    last: Option<(CollectionName, DocId)>,
}

impl SnapshotWriter {
    /// A writer for an unencrypted snapshot.
    pub fn new(header: SnapshotHeader, compress: bool) -> Self {
        Self {
            header,
            compress,
            chunk_target: TARGET_CHUNK_LEN,
            chunks: Vec::new(),
            pending: Vec::new(),
            pending_count: 0,
            last: None,
        }
    }

    /// Overrides the plain chunk size target (tests and conformance vectors).
    pub fn with_chunk_target(mut self, bytes: usize) -> Self {
        self.chunk_target = bytes.max(1);
        self
    }

    /// Adds one document. Entries must arrive in strictly increasing `(collection, doc_id)`.
    pub fn push(
        &mut self,
        collection: &CollectionName,
        id: &DocId,
        state: &DocState,
    ) -> Result<()> {
        let key = (collection.clone(), id.clone());
        if self.last.as_ref().is_some_and(|last| *last >= key) {
            return Err(Error::invalid_input(
                "snapshot entries must be sorted and unique",
            ));
        }
        let mut e = Encoder::new();
        encode_entry(&mut e, collection, id, state)?;
        let entry = e.into_bytes();
        if entry.len() + 10 > MAX_CHUNK_LEN {
            return Err(Error::invalid_input(
                "document is too large for a snapshot chunk",
            ));
        }
        if self.pending_count > 0 && self.pending.len() + entry.len() > self.chunk_target {
            self.flush()?;
        }
        self.pending.extend_from_slice(&entry);
        self.pending_count += 1;
        self.last = Some(key);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let mut e = Encoder::new();
        e.array(self.pending_count);
        e.raw(&self.pending);
        let plain = e.into_bytes();
        let data = if self.compress {
            zstd::bulk::compress(&plain, ZSTD_LEVEL)
                .map_err(|e| Error::new(ErrorKind::Internal, "zstd failed").with_source(e))?
        } else {
            plain
        };
        self.chunks.push(data);
        self.pending.clear();
        self.pending_count = 0;
        Ok(())
    }

    /// Finishes the file.
    pub fn finish(mut self) -> Result<Vec<u8>> {
        if self.pending_count > 0 || self.chunks.is_empty() {
            self.flush()?;
        }
        let vv = encode_vv(&self.header.vv);
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_be_bytes());
        let flags = if self.compress { FLAG_ZSTD } else { 0 };
        out.extend_from_slice(&flags.to_be_bytes());
        out.extend_from_slice(self.header.db_id.as_bytes());
        out.extend_from_slice(self.header.device_id.as_bytes());
        out.extend_from_slice(&self.header.hlc.as_u64().to_be_bytes());
        out.extend_from_slice(&(self.chunks.len() as u32).to_be_bytes());
        out.extend_from_slice(&(vv.len() as u32).to_be_bytes());
        out.extend_from_slice(&vv);
        for chunk in &self.chunks {
            out.extend_from_slice(&((NONCE_LEN + chunk.len()) as u32).to_be_bytes());
            out.extend_from_slice(&[0u8; NONCE_LEN]);
            out.extend_from_slice(chunk);
        }
        let trailer = blake3::hash(&out);
        out.extend_from_slice(trailer.as_bytes());
        Ok(out)
    }
}

/// What a reader expects, from the manifest and the logical key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expected {
    /// Database id from the manifest.
    pub db_id: DbId,
    /// Producing device from the key.
    pub device_id: DeviceId,
    /// HLC from the key.
    pub hlc: Hlc,
}

/// A validated snapshot whose entries can be read chunk by chunk.
#[derive(Debug)]
pub struct SnapshotReader<'a> {
    header: SnapshotHeader,
    compressed: bool,
    chunks: Vec<&'a [u8]>,
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(b[at..at + 2].try_into().expect("in bounds"))
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().expect("in bounds"))
}

impl<'a> SnapshotReader<'a> {
    /// Checks the header, ids, chunk framing and trailer.
    pub fn open(bytes: &'a [u8], expected: &Expected) -> Result<Self> {
        if bytes.len() < FIXED_HEADER_LEN + TRAILER_LEN || bytes[..4] != MAGIC {
            return Err(Error::corrupt("snapshot: bad magic or too short"));
        }
        match be16(bytes, 4) {
            VERSION => {}
            v if v > VERSION => return Err(Error::format_too_new(format!("snapshot version {v}"))),
            v => return Err(Error::corrupt(format!("snapshot: bad version {v}"))),
        }
        let flags = be16(bytes, 6);
        if flags & !KNOWN_FLAGS != 0 {
            return Err(Error::format_too_new(format!(
                "snapshot: unknown flags {flags:#06x}"
            )));
        }
        let db_id = DbId::from_bytes(bytes[8..24].try_into().expect("in bounds"));
        let device_id = DeviceId::from_bytes(bytes[24..40].try_into().expect("in bounds"));
        let hlc = Hlc::from_u64(u64::from_be_bytes(
            bytes[40..48].try_into().expect("in bounds"),
        ));
        if db_id != expected.db_id || device_id != expected.device_id || hlc != expected.hlc {
            return Err(Error::corrupt("snapshot: ids do not match its key"));
        }
        let (signed, trailer) = bytes.split_at(bytes.len() - TRAILER_LEN);
        if blake3::hash(signed).as_bytes() != trailer {
            return Err(Error::corrupt("snapshot: trailer mismatch"));
        }
        if flags & FLAG_ENCRYPTED != 0 {
            return Err(Error::format_too_new(
                "snapshot is encrypted; this build has no encryption support",
            ));
        }

        let chunk_count = be32(bytes, 48) as usize;
        let vv_len = be32(bytes, 52) as usize;
        if chunk_count == 0 {
            return Err(Error::corrupt("snapshot: chunk_count is 0"));
        }
        let vv_end = FIXED_HEADER_LEN
            .checked_add(vv_len)
            .filter(|&e| e <= signed.len())
            .ok_or_else(|| Error::corrupt("snapshot: vv_len out of range"))?;
        let vv = decode_vv(&signed[FIXED_HEADER_LEN..vv_end])?;

        let mut chunks = Vec::new();
        let mut pos = vv_end;
        for _ in 0..chunk_count {
            if pos + 4 > signed.len() {
                return Err(Error::corrupt("snapshot: truncated chunk"));
            }
            let len = be32(signed, pos) as usize;
            pos += 4;
            if len < NONCE_LEN || len - NONCE_LEN > MAX_CHUNK_LEN || pos + len > signed.len() {
                return Err(Error::corrupt("snapshot: bad chunk length"));
            }
            if signed[pos..pos + NONCE_LEN].iter().any(|&b| b != 0) {
                return Err(Error::corrupt(
                    "snapshot: nonce must be zero when not encrypted",
                ));
            }
            chunks.push(&signed[pos + NONCE_LEN..pos + len]);
            pos += len;
        }
        if pos != signed.len() {
            return Err(Error::corrupt("snapshot: bytes after the last chunk"));
        }
        Ok(Self {
            header: SnapshotHeader {
                db_id,
                device_id,
                hlc,
                vv,
            },
            compressed: flags & FLAG_ZSTD != 0,
            chunks,
        })
    }

    /// Identity and coverage.
    pub fn header(&self) -> &SnapshotHeader {
        &self.header
    }

    /// Number of chunks.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Decodes chunk `index`. `after` is the last key of the previous chunk, for the order check.
    fn chunk(
        &self,
        index: usize,
        after: Option<&(CollectionName, DocId)>,
    ) -> Result<Vec<(CollectionName, DocId, DocState)>> {
        let data = self.chunks[index];
        let owned;
        let plain = if self.compressed {
            owned = unzstd(data, MAX_CHUNK_LEN)?;
            owned.as_slice()
        } else {
            data
        };
        let mut d = Decoder::new(plain);
        let n = d.array()?;
        let mut out: Vec<(CollectionName, DocId, DocState)> = Vec::with_capacity(n);
        for _ in 0..n {
            let entry = decode_entry(&mut d)?;
            let prev = out
                .last()
                .map(|(c, i, _)| (c, i))
                .or(after.map(|(c, i)| (c, i)));
            if prev.is_some_and(|p| p >= (&entry.0, &entry.1)) {
                return Err(Error::corrupt("snapshot: entries are not sorted or repeat"));
            }
            out.push(entry);
        }
        d.finish()?;
        Ok(out)
    }

    /// Calls `f` for every entry, one chunk in memory at a time, in key order.
    pub fn for_each_entry(
        &self,
        mut f: impl FnMut(CollectionName, DocId, DocState) -> Result<()>,
    ) -> Result<()> {
        let mut last: Option<(CollectionName, DocId)> = None;
        for i in 0..self.chunks.len() {
            let entries = self.chunk(i, last.as_ref())?;
            if let Some((c, id, _)) = entries.last() {
                last = Some((c.clone(), id.clone()));
            }
            for (c, id, state) in entries {
                f(c, id, state)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::FieldState;
    use crate::model::{FieldName, Stamp, Value};

    fn header() -> SnapshotHeader {
        SnapshotHeader {
            db_id: DbId::from_bytes([1; 16]),
            device_id: DeviceId::from_bytes([2; 16]),
            hlc: Hlc::from_u64(99),
            vv: [
                (DeviceId::from_bytes([3; 16]), 5),
                (DeviceId::from_bytes([2; 16]), 7),
            ]
            .into(),
        }
    }

    fn expected() -> Expected {
        let h = header();
        Expected {
            db_id: h.db_id,
            device_id: h.device_id,
            hlc: h.hlc,
        }
    }

    fn state(i: i64) -> DocState {
        let mut s = DocState::default();
        s.fields.insert(
            FieldName::new("v").unwrap(),
            FieldState {
                stamp: Stamp::new(Hlc::from_u64(10), DeviceId::from_bytes([3; 16])),
                value: Some(Value::Int(i)),
            },
        );
        s
    }

    type Entries = Vec<(CollectionName, DocId, DocState)>;

    fn entries(n: usize) -> Entries {
        (0..n)
            .map(|i| {
                (
                    CollectionName::new("c").unwrap(),
                    DocId::new(&format!("d{i:04}")).unwrap(),
                    state(i as i64),
                )
            })
            .collect()
    }

    fn write(n: usize, compress: bool, chunk_target: usize) -> Vec<u8> {
        let mut w = SnapshotWriter::new(header(), compress).with_chunk_target(chunk_target);
        for (c, id, s) in entries(n) {
            w.push(&c, &id, &s).unwrap();
        }
        w.finish().unwrap()
    }

    fn read_all(bytes: &[u8]) -> Result<(SnapshotHeader, Entries)> {
        let r = SnapshotReader::open(bytes, &expected())?;
        let mut out = Vec::new();
        r.for_each_entry(|c, id, s| {
            out.push((c, id, s));
            Ok(())
        })?;
        Ok((r.header().clone(), out))
    }

    #[test]
    fn round_trip_single_and_multi_chunk() {
        for (compress, target) in [
            (false, TARGET_CHUNK_LEN),
            (true, TARGET_CHUNK_LEN),
            (false, 100),
            (true, 100),
        ] {
            let bytes = write(40, compress, target);
            let (h, got) = read_all(&bytes).unwrap();
            assert_eq!(h, header());
            assert_eq!(got, entries(40));
            let chunks = SnapshotReader::open(&bytes, &expected())
                .unwrap()
                .chunk_count();
            assert_eq!(chunks > 1, target == 100, "{compress} {target}");
        }
    }

    #[test]
    fn empty_snapshot_has_one_empty_chunk() {
        let bytes = write(0, false, TARGET_CHUNK_LEN);
        let r = SnapshotReader::open(&bytes, &expected()).unwrap();
        assert_eq!(r.chunk_count(), 1);
        assert!(read_all(&bytes).unwrap().1.is_empty());
    }

    #[test]
    fn writer_requires_sorted_unique_entries() {
        let mut w = SnapshotWriter::new(header(), false);
        let c = CollectionName::new("c").unwrap();
        w.push(&c, &DocId::new("b").unwrap(), &state(1)).unwrap();
        assert!(w.push(&c, &DocId::new("a").unwrap(), &state(1)).is_err());
        assert!(w.push(&c, &DocId::new("b").unwrap(), &state(1)).is_err());
    }

    #[test]
    fn reader_rejects_damage_and_mismatches() {
        let good = write(10, false, 100);
        let mut b = good.clone();
        let last = b.len() - 1;
        b[last] ^= 1;
        assert_eq!(read_all(&b).unwrap_err().kind(), ErrorKind::Corrupt);
        let mut b = good.clone();
        b[70] ^= 1;
        assert_eq!(read_all(&b).unwrap_err().kind(), ErrorKind::Corrupt);
        let wrong = Expected {
            hlc: Hlc::from_u64(98),
            ..expected()
        };
        assert!(SnapshotReader::open(&good, &wrong).is_err());
        let mut b = good;
        b[5] = 9;
        assert_eq!(read_all(&b).unwrap_err().kind(), ErrorKind::FormatTooNew);
    }

    #[test]
    fn order_is_checked_across_chunks() {
        // Two chunks written separately with keys out of order across the boundary.
        let mut w = SnapshotWriter::new(header(), false).with_chunk_target(1);
        let c = CollectionName::new("c").unwrap();
        w.push(&c, &DocId::new("a").unwrap(), &state(1)).unwrap();
        w.push(&c, &DocId::new("b").unwrap(), &state(2)).unwrap();
        let mut bytes = w.finish().unwrap();
        // Swap the two single-entry chunk records (same length), so "b" comes before "a".
        let vv_end = FIXED_HEADER_LEN + be32(&bytes, 52) as usize;
        let rec = 4 + be32(&bytes, vv_end) as usize;
        let (first, second) = bytes[vv_end..vv_end + 2 * rec].split_at(rec);
        let swapped = [second, first].concat();
        bytes[vv_end..vv_end + 2 * rec].copy_from_slice(&swapped);
        let n = bytes.len() - TRAILER_LEN;
        let h = blake3::hash(&bytes[..n]);
        bytes[n..].copy_from_slice(h.as_bytes());
        let err = read_all(&bytes).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Corrupt);
    }
}
