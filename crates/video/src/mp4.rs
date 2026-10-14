//! Reads an MP4's duration from its movie header (`moov/mvhd`), without ffmpeg.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The duration in seconds from the file's `mvhd` box, if it has one.
pub(crate) fn duration_seconds(path: &Path) -> Option<f32> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let (moov_start, moov_end) = find_box(&mut file, 0, len, b"moov")?;
    let (mvhd_start, mvhd_end) = find_box(&mut file, moov_start, moov_end, b"mvhd")?;
    let mut body = vec![0u8; usize::try_from((mvhd_end - mvhd_start).min(128)).ok()?];
    file.seek(SeekFrom::Start(mvhd_start)).ok()?;
    file.read_exact(&mut body).ok()?;
    mvhd_duration(&body)
}

/// Finds the first box of `kind` among the boxes in `[start, end)`; returns its payload
/// range.
fn find_box<R: Read + Seek>(r: &mut R, start: u64, end: u64, kind: &[u8; 4]) -> Option<(u64, u64)> {
    let mut at = start;
    while at + 8 <= end {
        r.seek(SeekFrom::Start(at)).ok()?;
        let mut header = [0u8; 8];
        r.read_exact(&mut header).ok()?;
        let size32 = u32::from_be_bytes(header[0..4].try_into().ok()?);
        let (size, header_len) = match size32 {
            0 => (end - at, 8),
            1 => {
                let mut large = [0u8; 8];
                r.read_exact(&mut large).ok()?;
                (u64::from_be_bytes(large), 16)
            }
            n => (u64::from(n), 8),
        };
        if size < header_len || at + size > end {
            return None;
        }
        if &header[4..8] == kind {
            return Some((at + header_len, at + size));
        }
        at += size;
    }
    None
}

/// Duration from an `mvhd` payload (version, flags, times, timescale, duration).
fn mvhd_duration(body: &[u8]) -> Option<f32> {
    let version = *body.first()?;
    let (timescale, duration) = if version == 1 {
        let timescale = u32::from_be_bytes(body.get(20..24)?.try_into().ok()?);
        let duration = u64::from_be_bytes(body.get(24..32)?.try_into().ok()?);
        (timescale, duration)
    } else {
        let timescale = u32::from_be_bytes(body.get(12..16)?.try_into().ok()?);
        let duration = u32::from_be_bytes(body.get(16..20)?.try_into().ok()?);
        (timescale, u64::from(duration))
    };
    if timescale == 0 || duration == u64::from(u32::MAX) || duration == u64::MAX {
        return None;
    }
    Some((duration as f64 / f64::from(timescale)) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    fn mvhd_v0(timescale: u32, duration: u32) -> Vec<u8> {
        let mut p = vec![0u8; 4 + 8]; // version, flags, creation, modification
        p.extend_from_slice(&timescale.to_be_bytes());
        p.extend_from_slice(&duration.to_be_bytes());
        p.extend_from_slice(&[0u8; 80]);
        bx(b"mvhd", &p)
    }

    #[test]
    fn reads_duration_after_mdat() {
        let dir = std::env::temp_dir().join(format!("margin-video-mp4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.mp4");
        let mut file = bx(b"ftyp", b"isom\0\0\0\0isom");
        file.extend(bx(b"mdat", &[7u8; 1000]));
        let mut moov_payload = mvhd_v0(1000, 5280);
        moov_payload.extend(bx(b"trak", &[0u8; 16]));
        file.extend(bx(b"moov", &moov_payload));
        std::fs::write(&path, &file).unwrap();
        assert_eq!(duration_seconds(&path), Some(5.28));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn version_1_header() {
        let mut p = vec![1u8, 0, 0, 0];
        p.extend_from_slice(&[0u8; 16]);
        p.extend_from_slice(&600u32.to_be_bytes());
        p.extend_from_slice(&1800u64.to_be_bytes());
        assert_eq!(mvhd_duration(&p), Some(3.0));
    }
}
