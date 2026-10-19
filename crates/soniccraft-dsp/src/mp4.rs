//! Minimal ISO-BMFF (MP4/M4A) box reader for the one thing symphonia 0.6.1 ignores: the edit list
//! (`elst`) that tells players to skip the AAC encoder priming (usually 1024 or 2112 samples) and
//! where the real audio ends. Without it, AAC decodes ~21 ms late and too long.
//!
//! Every access is bounds-checked: malformed files yield `None`, never a panic.

/// The playable range of the first sound track, from its edit list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioEdit {
    /// Media time where playback starts (priming to skip), in media timescale units.
    pub skip: u64,
    /// Playable duration in media timescale units (`None` = to the end of the media).
    pub keep: Option<u64>,
    /// The media (`mdhd`) timescale, normally equal to the sample rate.
    pub media_timescale: u32,
}

impl AudioEdit {
    /// `(skip, keep)` converted to frames at `sample_rate` (rounded to nearest).
    pub(crate) fn frames(&self, sample_rate: u32) -> (u64, Option<u64>) {
        let ts = u128::from(self.media_timescale.max(1));
        let sr = u128::from(sample_rate);
        let conv = |v: u64| ((u128::from(v) * sr + ts / 2) / ts) as u64;
        (conv(self.skip), self.keep.map(conv))
    }
}

fn be_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn be_u64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(
        b.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// Iterate over the boxes in `b`: `(type, payload)`.
fn boxes(mut b: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        if b.len() < 8 {
            return None;
        }
        let size32 = be_u32(b, 0)?;
        let kind: [u8; 4] = b.get(4..8)?.try_into().ok()?;
        let (header, size) = match size32 {
            1 => (16usize, usize::try_from(be_u64(b, 8)?).ok()?),
            0 => (8, b.len()),
            s => (8, s as usize),
        };
        if size < header || size > b.len() {
            return None;
        }
        let payload = &b[header..size];
        b = &b[size..];
        Some((kind, payload))
    })
}

fn find<'a>(b: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(b).find(|(k, _)| k == kind).map(|(_, p)| p)
}

/// Timescale of an `mvhd`/`mdhd` full box (version 0: payload[12..16], version 1: payload[20..24]).
fn timescale(full_box: &[u8]) -> Option<u32> {
    let at = if *full_box.first()? == 1 { 20 } else { 12 };
    be_u32(full_box, at).filter(|&t| t > 0)
}

/// The edit of the first sound track (`hdlr` type `soun`) that has an edit list, if any.
pub(crate) fn audio_edit(file: &[u8]) -> Option<AudioEdit> {
    let moov = find(file, b"moov")?;
    let movie_timescale = timescale(find(moov, b"mvhd")?)?;
    for (kind, trak) in boxes(moov) {
        if kind != *b"trak" {
            continue;
        }
        let Some(mdia) = find(trak, b"mdia") else {
            continue;
        };
        if find(mdia, b"hdlr").and_then(|h| h.get(8..12)) != Some(b"soun".as_slice()) {
            continue;
        }
        let media_timescale = timescale(find(mdia, b"mdhd")?)?;
        let elst = find(find(trak, b"edts")?, b"elst")?;
        let version = *elst.first()?;
        let count = be_u32(elst, 4)?;
        let entry_len = if version == 1 { 20 } else { 12 };
        let mut off = 8usize;
        for _ in 0..count.min(1024) {
            let (segment_duration, media_time) = if version == 1 {
                (be_u64(elst, off)?, be_u64(elst, off + 8)? as i64)
            } else {
                (
                    u64::from(be_u32(elst, off)?),
                    i64::from(be_u32(elst, off + 4)? as i32),
                )
            };
            off += entry_len;
            if media_time >= 0 {
                // segment_duration is in the movie timescale; 0 means "the rest of the media".
                let keep = (segment_duration > 0).then(|| {
                    (u128::from(segment_duration) * u128::from(media_timescale)
                        / u128::from(movie_timescale)) as u64
                });
                return Some(AudioEdit {
                    skip: media_time as u64,
                    keep,
                    media_timescale,
                });
            }
        }
        return None;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    fn full_v0_timescale(ts: u32) -> Vec<u8> {
        let mut p = vec![0u8; 12];
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&[0u8; 8]);
        p
    }

    fn sample_file(elst_entries: &[(u32, i32)]) -> Vec<u8> {
        let mut elst = vec![0u8, 0, 0, 0];
        elst.extend_from_slice(&(elst_entries.len() as u32).to_be_bytes());
        for &(d, t) in elst_entries {
            elst.extend_from_slice(&d.to_be_bytes());
            elst.extend_from_slice(&t.to_be_bytes());
            elst.extend_from_slice(&[0, 1, 0, 0]);
        }
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(b"soun");
        hdlr.extend_from_slice(&[0u8; 12]);
        let mdia = [bx(b"mdhd", &full_v0_timescale(48_000)), bx(b"hdlr", &hdlr)].concat();
        let trak = [bx(b"edts", &bx(b"elst", &elst)), bx(b"mdia", &mdia)].concat();
        let moov = [bx(b"mvhd", &full_v0_timescale(1000)), bx(b"trak", &trak)].concat();
        [
            bx(b"ftyp", b"M4A \0\0\0\0"),
            bx(b"moov", &moov),
            bx(b"mdat", &[0u8; 16]),
        ]
        .concat()
    }

    #[test]
    fn reads_priming_and_duration() {
        // 4647 ms in the movie timescale (1000), skip 1024 media samples.
        let file = sample_file(&[(4647, 1024)]);
        let edit = audio_edit(&file).unwrap();
        assert_eq!(
            edit,
            AudioEdit {
                skip: 1024,
                keep: Some(223_056),
                media_timescale: 48_000
            }
        );
        assert_eq!(edit.frames(48_000), (1024, Some(223_056)));
        assert_eq!(edit.frames(24_000), (512, Some(111_528)));
    }

    #[test]
    fn skips_empty_edits() {
        let file = sample_file(&[(100, -1), (0, 2112)]);
        assert_eq!(
            audio_edit(&file).unwrap(),
            AudioEdit {
                skip: 2112,
                keep: None,
                media_timescale: 48_000
            }
        );
    }

    #[test]
    fn malformed_input_never_panics() {
        let file = sample_file(&[(4647, 1024)]);
        for cut in 0..file.len() {
            let _ = audio_edit(&file[..cut]);
        }
        assert_eq!(audio_edit(b"not an mp4 at all"), None);
        // A box claiming a 64-bit size larger than the buffer.
        let mut bogus = vec![0, 0, 0, 1];
        bogus.extend_from_slice(b"moov");
        bogus.extend_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(audio_edit(&bogus), None);
    }
}
