//! The stored form of one snapshot layer, and the pieces it is uploaded in.
//!
//! A layer is zstd, then XChaCha20-Poly1305 in the STREAM construction:
//! every 1 MiB chunk is authenticated on its own, and the last chunk carries
//! a flag the others do not, so a stream cut short -- even exactly on a chunk
//! boundary -- does not decrypt rather than restoring half a home.
//!
//! ```text
//! "PKS1" | chunk size, u32 BE | nonce prefix (19 bytes) | chunk...
//! chunk = up to `chunk size` bytes of plaintext, sealed: +16 bytes of tag
//! ```
//!
//! The associated data names the snapshot and the layer, so one layer's
//! object cannot be passed off as another's.

use std::io::{self, Read, Write};

use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::stream::{DecryptorBE32, EncryptorBE32};
use chacha20poly1305::aead::{KeyInit, OsRng, Payload};
use chacha20poly1305::XChaCha20Poly1305;
use sha2::{Digest, Sha256};

pub const MAGIC: &[u8; 4] = b"PKS1";
/// Plaintext per sealed chunk.
pub const CHUNK: usize = 1 << 20;
const TAG: usize = 16;
/// XChaCha20's 24-byte nonce, less the 5 bytes STREAM uses for its counter
/// and last-chunk flag.
const PREFIX: usize = 19;
const HEADER: usize = MAGIC.len() + 4 + PREFIX;
/// Refused on read: no writer produces chunks this big.
const MAX_CHUNK: usize = 64 << 20;

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Seals everything written to it into `inner`, in the format above.
pub struct SealWriter<W: Write> {
    inner: W,
    enc: Option<EncryptorBE32<XChaCha20Poly1305>>,
    aad: Vec<u8>,
    buf: Vec<u8>,
    chunk: usize,
}

impl<W: Write> SealWriter<W> {
    pub fn new(inner: W, key: &[u8; 32], aad: &[u8]) -> io::Result<Self> {
        Self::with_chunk(inner, key, aad, CHUNK)
    }

    fn with_chunk(mut inner: W, key: &[u8; 32], aad: &[u8], chunk: usize) -> io::Result<Self> {
        let mut prefix = [0u8; PREFIX];
        OsRng.fill_bytes(&mut prefix);
        inner.write_all(MAGIC)?;
        inner.write_all(&(chunk as u32).to_be_bytes())?;
        inner.write_all(&prefix)?;
        let aead = XChaCha20Poly1305::new(key.as_slice().into());
        Ok(SealWriter {
            inner,
            enc: Some(EncryptorBE32::from_aead(aead, prefix.as_slice().into())),
            aad: aad.to_vec(),
            buf: Vec::with_capacity(chunk + chunk / 2),
            chunk,
        })
    }

    /// Seal the last chunk and hand back the writer underneath.
    pub fn finish(mut self) -> io::Result<W> {
        let enc = self.enc.take().ok_or_else(|| invalid("already finished"))?;
        let sealed = enc
            .encrypt_last(Payload { msg: &self.buf, aad: &self.aad })
            .map_err(|_| invalid("sealing the last chunk failed"))?;
        self.inner.write_all(&sealed)?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for SealWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        // Strictly more than a chunk: until more data follows, the chunk held
        // back might be the last one, and the last is sealed differently.
        while self.buf.len() > self.chunk {
            let enc = self.enc.as_mut().ok_or_else(|| invalid("already finished"))?;
            let sealed = enc
                .encrypt_next(Payload { msg: &self.buf[..self.chunk], aad: &self.aad })
                .map_err(|_| invalid("sealing a chunk failed"))?;
            self.inner.write_all(&sealed)?;
            self.buf.drain(..self.chunk);
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Opens a sealed stream as it is read. Tampering, truncation and the wrong
/// key are all `InvalidData` errors -- never output that is merely short.
pub struct OpenReader<R: Read> {
    inner: R,
    dec: Option<DecryptorBE32<XChaCha20Poly1305>>,
    aad: Vec<u8>,
    block: usize,
    /// The next sealed chunk, read ahead: only the absence of another after
    /// it says a chunk is the last.
    ahead: Vec<u8>,
    out: Vec<u8>,
    pos: usize,
}

impl<R: Read> OpenReader<R> {
    pub fn new(mut inner: R, key: &[u8; 32], aad: &[u8]) -> io::Result<Self> {
        let mut header = [0u8; HEADER];
        inner.read_exact(&mut header).map_err(|_| invalid("not a snapshot layer: no header"))?;
        if &header[..4] != MAGIC {
            return Err(invalid("not a snapshot layer"));
        }
        let chunk = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if chunk == 0 || chunk > MAX_CHUNK {
            return Err(invalid("not a snapshot layer: implausible chunk size"));
        }
        let aead = XChaCha20Poly1305::new(key.as_slice().into());
        let mut r = OpenReader {
            inner,
            dec: Some(DecryptorBE32::from_aead(aead, header[8..].into())),
            aad: aad.to_vec(),
            block: chunk + TAG,
            ahead: Vec::new(),
            out: Vec::new(),
            pos: 0,
        };
        r.ahead = r.read_block()?;
        Ok(r)
    }

    /// Up to one sealed chunk; shorter only at the end of the stream.
    fn read_block(&mut self) -> io::Result<Vec<u8>> {
        let mut b = vec![0u8; self.block];
        let mut n = 0;
        while n < b.len() {
            match self.inner.read(&mut b[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        b.truncate(n);
        Ok(b)
    }

    fn refill(&mut self) -> io::Result<()> {
        let cur = std::mem::take(&mut self.ahead);
        let last = if cur.len() < self.block {
            true
        } else {
            self.ahead = self.read_block()?;
            self.ahead.is_empty()
        };
        if cur.len() < TAG {
            return Err(invalid("the snapshot layer is truncated"));
        }
        let payload = Payload { msg: &cur, aad: &self.aad };
        let opened = if last {
            self.dec.take().map(|d| d.decrypt_last(payload))
        } else {
            self.dec.as_mut().map(|d| d.decrypt_next(payload))
        };
        self.out = opened
            .ok_or_else(|| invalid("already finished"))?
            .map_err(|_| invalid("the snapshot layer does not open: corrupt, truncated, or the wrong key"))?;
        self.pos = 0;
        Ok(())
    }
}

impl<R: Read> Read for OpenReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.out.len() {
            if self.dec.is_none() {
                return Ok(0);
            }
            self.refill()?;
        }
        let n = buf.len().min(self.out.len() - self.pos);
        buf[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// What a layer came to once every part was handed over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkSummary {
    pub parts: u32,
    pub bytes: u64,
    /// Of the whole stored stream, hex.
    pub sha256: String,
}

/// Cuts a stream into the parts of a multipart upload and hands each to
/// `send` as it fills. Every part but the last is exactly `part_bytes` long:
/// R2 refuses an upload whose parts differ.
pub struct PartSink<F> {
    part_bytes: usize,
    buf: Vec<u8>,
    next: u32,
    send: F,
    hasher: Sha256,
    bytes: u64,
}

impl<F: FnMut(u32, Vec<u8>) -> io::Result<()>> PartSink<F> {
    pub fn new(part_bytes: usize, send: F) -> Self {
        PartSink { part_bytes: part_bytes.max(1), buf: Vec::new(), next: 1, send, hasher: Sha256::new(), bytes: 0 }
    }

    fn emit(&mut self, part: Vec<u8>) -> io::Result<()> {
        (self.send)(self.next, part)?;
        self.next += 1;
        Ok(())
    }

    /// Send what is left as the last part. An upload needs at least one
    /// part, so an empty stream still sends an empty one.
    pub fn finish(mut self) -> io::Result<SinkSummary> {
        if !self.buf.is_empty() || self.next == 1 {
            let last = std::mem::take(&mut self.buf);
            self.emit(last)?;
        }
        Ok(SinkSummary { parts: self.next - 1, bytes: self.bytes, sha256: hex::encode(self.hasher.finalize()) })
    }
}

impl<F: FnMut(u32, Vec<u8>) -> io::Result<()>> Write for PartSink<F> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.hasher.update(data);
        self.bytes += data.len() as u64;
        self.buf.extend_from_slice(data);
        while self.buf.len() >= self.part_bytes {
            let rest = self.buf.split_off(self.part_bytes);
            let part = std::mem::replace(&mut self.buf, rest);
            self.emit(part)?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];

    fn seal(data: &[u8], chunk: usize) -> Vec<u8> {
        let mut w = SealWriter::with_chunk(Vec::new(), &KEY, b"aad", chunk).unwrap();
        // Odd-sized writes, so chunk boundaries fall mid-write.
        for piece in data.chunks(333) {
            w.write_all(piece).unwrap();
        }
        w.finish().unwrap()
    }

    fn open(sealed: &[u8], key: &[u8; 32], aad: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        OpenReader::new(sealed, key, aad)?.read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn round_trips_on_and_across_chunk_boundaries() {
        for len in [0usize, 1, 1023, 1024, 1025, 2048, 5000] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 % 256) as u8).collect();
            assert_eq!(open(&seal(&data, 1024), &KEY, b"aad").unwrap(), data, "{len} bytes");
        }
    }

    #[test]
    fn a_flipped_bit_is_caught() {
        let mut sealed = seal(&[1u8; 3000], 1024);
        let i = sealed.len() / 2;
        sealed[i] ^= 1;
        assert!(open(&sealed, &KEY, b"aad").is_err());
    }

    /// The last-chunk flag is what makes this fail: every chunk up to the cut
    /// is individually valid.
    #[test]
    fn a_stream_cut_on_a_chunk_boundary_is_caught() {
        let sealed = seal(&[1u8; 3000], 1024);
        let cut = HEADER + 2 * (1024 + TAG);
        assert!(sealed.len() > cut);
        assert!(open(&sealed[..cut], &KEY, b"aad").is_err());
    }

    #[test]
    fn another_key_or_another_layer_does_not_open_it() {
        let sealed = seal(b"hello", 1024);
        assert!(open(&sealed, &[8; 32], b"aad").is_err());
        assert!(open(&sealed, &KEY, b"another layer").is_err());
    }

    #[test]
    fn garbage_is_not_a_layer() {
        assert!(open(b"nope", &KEY, b"aad").is_err());
        assert!(open(b"PKS1\0\0\0\0aaaaaaaaaaaaaaaaaaa", &KEY, b"aad").is_err(), "a zero chunk size");
    }

    #[test]
    fn parts_are_all_equal_but_the_last() {
        let mut got = Vec::new();
        let mut sink = PartSink::new(10, |n, part: Vec<u8>| {
            got.push((n, part.len()));
            Ok(())
        });
        sink.write_all(&[0u8; 35]).unwrap();
        let summary = sink.finish().unwrap();
        assert_eq!(got, vec![(1, 10), (2, 10), (3, 10), (4, 5)]);
        assert_eq!((summary.parts, summary.bytes), (4, 35));
        assert_eq!(summary.sha256, hex::encode(Sha256::digest([0u8; 35])));
    }

    #[test]
    fn an_empty_stream_is_still_one_part() {
        let mut got = Vec::new();
        let sink = PartSink::new(10, |n, part: Vec<u8>| {
            got.push((n, part.len()));
            Ok(())
        });
        assert_eq!(sink.finish().unwrap().parts, 1);
        assert_eq!(got, vec![(1, 0)]);
    }
}
