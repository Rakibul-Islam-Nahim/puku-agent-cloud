//! Blocking frame IO for the guest side of `guest_proto`.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::io::{self, Read, Write};

use puku_cloud_proto::guest_proto::{header, parse_header, GuestReply};

pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    w.write_all(&header(kind, payload.len()))?;
    w.write_all(payload)?;
    w.flush()
}

pub fn write_reply(w: &mut impl Write, reply: &GuestReply) -> io::Result<()> {
    let json = serde_json::to_vec(reply).map_err(io::Error::other)?;
    write_frame(w, puku_cloud_proto::guest_proto::kind::JSON, &json)
}

/// The next frame, or `None` at a clean end of stream.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut h = [0u8; 5];
    match r.read_exact(&mut h) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let (kind, len) = parse_header(h).map_err(io::Error::other)?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((kind, payload)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use puku_cloud_proto::guest_proto::kind;

    #[test]
    fn frames_round_trip_and_end_cleanly() {
        let mut buf = Vec::new();
        write_frame(&mut buf, kind::STDOUT, b"hello").unwrap();
        write_reply(&mut buf, &GuestReply::Exited { code: 3, timed_out: false }).unwrap();
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), Some((kind::STDOUT, b"hello".to_vec())));
        let (k, json) = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(k, kind::JSON);
        assert_eq!(
            serde_json::from_slice::<GuestReply>(&json).unwrap(),
            GuestReply::Exited { code: 3, timed_out: false }
        );
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_an_end() {
        let mut buf = Vec::new();
        write_frame(&mut buf, kind::STDOUT, b"hello").unwrap();
        buf.truncate(7);
        assert!(read_frame(&mut &buf[..]).is_err());
    }
}
