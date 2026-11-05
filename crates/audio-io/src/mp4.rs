//! Size checks on MP4 atoms before symphonia reads them: it allocates whatever an atom, a sample
//! table or a sample claims, and a failed allocation aborts the process.

use crate::pcm::{be_u32, be_u64};
use crate::{AudioError, Result};

#[derive(Clone, Copy)]
enum Level {
    Top,
    Moov,
    Trak,
    Mdia,
    Minf,
    Stbl,
    Udta,
    Meta,
    Ilst,
    Tag,
}

/// Refuse the MP4 file whose top-level atoms start at `at` if an atom symphonia reads whole, a
/// sample table or a sample claims more bytes than the file holds.
pub(crate) fn check(b: &[u8], at: usize) -> Result<()> {
    atoms(b, at, b.len(), Level::Top).map(|_| ())
}

fn malformed(what: &str) -> AudioError {
    AudioError::Malformed(format!("MP4 {what} is larger than the file"))
}

/// Walk the atoms in `pos..end` that symphonia descends into, as it reads them. False where
/// symphonia stops reading: at a header it refuses, or after an atom that runs past `end`.
fn atoms(b: &[u8], mut pos: usize, end: usize, level: Level) -> Result<bool> {
    // A header that starts inside the container is read whole, even past its end.
    while pos < end {
        let (Some(size), Some(kind)) = (be_u32(b, pos), b.get(pos + 4..pos + 8)) else { return Ok(false) };
        let head = if size == 1 { 16 } else { 8 };
        // Size 0: no data, and the next atom starts at the container's end.
        let len = match size {
            0 => head,
            1 => be_u64(b, pos + 8).unwrap_or(0),
            n => u64::from(n),
        };
        if len < head {
            return Ok(false);
        }
        let body = pos + head as usize;
        let data_end = usize::try_from(len).ok().and_then(|n| pos.checked_add(n));
        let next = if size == 0 { Some(end) } else { data_end };
        let inner = data_end.map_or(b.len(), |n| n.min(b.len()));
        let more = match (level, kind) {
            (Level::Top, b"moov") => atoms(b, body, inner, Level::Moov)?,
            (Level::Top | Level::Udta, b"meta") => atoms(b, body + 4, inner, Level::Meta)?,
            (Level::Moov, b"trak") => atoms(b, body, inner, Level::Trak)?,
            (Level::Moov, b"udta") => atoms(b, body, inner, Level::Udta)?,
            (Level::Trak, b"mdia") => atoms(b, body, inner, Level::Mdia)?,
            (Level::Mdia, b"minf") => atoms(b, body, inner, Level::Minf)?,
            (Level::Minf, b"stbl") => atoms(b, body, inner, Level::Stbl)?,
            (Level::Meta, b"ilst") => atoms(b, body, inner, Level::Ilst)?,
            (Level::Ilst, _) => atoms(b, body, inner, Level::Tag)?,
            (Level::Stbl, b"stts" | b"co64") => table(b, body + 4, 8, inner)?,
            (Level::Stbl, b"stsc") => table(b, body + 4, 12, inner)?,
            (Level::Stbl, b"stco") => table(b, body + 4, 4, inner)?,
            (Level::Stbl, b"stsz") => sample_sizes(b, body, inner)?,
            (Level::Mdia, b"hdlr") | (Level::Tag, b"data" | b"name" | b"mean") if len > (b.len() - pos) as u64 => {
                return Err(malformed(&format!("`{}` atom", String::from_utf8_lossy(kind))));
            }
            _ => true,
        };
        match next {
            Some(n) if more && n <= end => pos = n,
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// A sample table's entries, counted at `count`, must fit in the file: symphonia reserves room for
/// all of them first. False if they run past the table's atom (`end`), where symphonia stops.
fn table(b: &[u8], count: usize, entry: u64, end: usize) -> Result<bool> {
    let Some(n) = be_u32(b, count) else { return Ok(false) };
    let (first, bytes) = (count + 4, u64::from(n) * entry);
    if bytes > b.len().saturating_sub(first) as u64 {
        return Err(malformed("sample table"));
    }
    Ok(first as u64 + bytes <= end as u64)
}

/// Every sample's size must fit in the file: symphonia reads each one into a buffer of that size.
/// Sizes past the atom are never used, as symphonia stops before reading a packet.
fn sample_sizes(b: &[u8], body: usize, end: usize) -> Result<bool> {
    let (Some(size), Some(count)) = (be_u32(b, body + 4), be_u32(b, body + 8)) else { return Ok(false) };
    let file = b.len() as u64;
    if u64::from(size) > file {
        return Err(malformed("sample"));
    }
    if size != 0 {
        return Ok(true);
    }
    let fits = table(b, body + 8, 4, end)?;
    let (sizes, _) = b.get(body + 12..end).unwrap_or_default().as_chunks::<4>();
    if sizes.iter().take(count as usize).any(|s| u64::from(u32::from_be_bytes(*s)) > file) {
        return Err(malformed("sample"));
    }
    Ok(fits)
}

/// The part of an MP4 audio track's media that its edit list presents, in the media's own
/// timescale. Encoders put the AAC priming (usually 1024 frames) and the end padding outside it,
/// and symphonia's MP4 reader parses but does not apply the list, so a decode that ignores it
/// starts late and runs long.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EditRange {
    /// Media units skipped at the start.
    pub skip: u64,
    /// Media units presented after the skip, if the list gives a length.
    pub len: Option<u64>,
    /// The media's units per second (`mdhd`).
    pub timescale: u32,
}

impl EditRange {
    /// Frames at `rate` for `units` of media time.
    fn frames(units: u64, timescale: u32, rate: u32) -> u64 {
        if timescale == 0 {
            return 0;
        }
        (u128::from(units) * u128::from(rate) / u128::from(timescale)).min(u128::from(u64::MAX)) as u64
    }

    /// The frames to drop from the start and the frames to keep after that, for `rate` Hz output.
    pub(crate) fn frames_at(&self, rate: u32) -> (u64, Option<u64>) {
        (Self::frames(self.skip, self.timescale, rate), self.len.map(|l| Self::frames(l, self.timescale, rate)))
    }
}

/// Child atoms of the container whose body is `pos..end`: `(kind, body_start, body_end)`.
fn children(b: &[u8], mut pos: usize, end: usize) -> Vec<([u8; 4], usize, usize)> {
    let end = end.min(b.len());
    let mut out = Vec::new();
    while pos.saturating_add(8) <= end {
        let (Some(size), Some(kind)) = (be_u32(b, pos), crate::pcm::tag(b, pos + 4)) else { break };
        let (head, len) = match size {
            0 => (8, (end - pos) as u64),
            1 => (16, be_u64(b, pos + 8).unwrap_or(0)),
            n => (8, u64::from(n)),
        };
        let Some(next) = usize::try_from(len).ok().and_then(|l| pos.checked_add(l)) else { break };
        if len < head as u64 || next > end {
            break;
        }
        out.push((kind, pos + head, next));
        pos = next;
    }
    out
}

fn child(b: &[u8], parent: (usize, usize), kind: &[u8; 4]) -> Option<(usize, usize)> {
    children(b, parent.0, parent.1).into_iter().find(|c| &c.0 == kind).map(|c| (c.1, c.2))
}

/// `mvhd` / `mdhd` timescale.
fn timescale(b: &[u8], body: usize) -> Option<u32> {
    let off = if *b.get(body)? == 1 { 4 + 8 + 8 } else { 4 + 4 + 4 };
    be_u32(b, body + off)
}

/// The edit range of the first sound track in the MP4 file `b`, when its edit list is one plain
/// (non-empty) edit, which is what AAC encoders write. `None` for any other layout.
pub(crate) fn edit_range(b: &[u8]) -> Option<EditRange> {
    let moov = children(b, 0, b.len()).into_iter().find(|c| &c.0 == b"moov").map(|c| (c.1, c.2))?;
    let movie_scale = timescale(b, child(b, moov, b"mvhd")?.0)?;
    for (kind, start, end) in children(b, moov.0, moov.1) {
        if &kind != b"trak" {
            continue;
        }
        let mdia = child(b, (start, end), b"mdia")?;
        // hdlr: version+flags, pre_defined, handler type.
        if crate::pcm::tag(b, child(b, mdia, b"hdlr")?.0 + 8)? != *b"soun" {
            continue;
        }
        let media_scale = timescale(b, child(b, mdia, b"mdhd")?.0)?;
        let elst = child(b, child(b, (start, end), b"edts")?, b"elst")?;
        let v1 = *b.get(elst.0)? == 1;
        let count = be_u32(b, elst.0 + 4)? as usize;
        let entry = if v1 { 20 } else { 12 };
        let mut real = Vec::new();
        for i in 0..count.min(64) {
            let at = elst.0 + 8 + i * entry;
            let (dur, time) =
                if v1 { (be_u64(b, at)?, be_u64(b, at + 8)? as i64) } else { (u64::from(be_u32(b, at)?), i64::from(be_u32(b, at + 4)? as i32)) };
            if time >= 0 {
                real.push((dur, time as u64));
            } else if !real.is_empty() {
                return None;
            }
        }
        let [(dur, skip)] = real[..] else { return None };
        let len = (dur != 0 && movie_scale != 0)
            .then(|| (u128::from(dur) * u128::from(media_scale) / u128::from(movie_scale)).min(u128::from(u64::MAX)) as u64);
        return Some(EditRange { skip, len, timescale: media_scale });
    }
    None
}

#[cfg(test)]
mod edit_tests {
    use super::*;

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn mp4(movie_scale: u32, media_scale: u32, edits: &[(u32, i32)]) -> Vec<u8> {
        let mvhd = atom(b"mvhd", &[&[0u8; 12][..], &movie_scale.to_be_bytes(), &[0; 4]].concat());
        let hdlr = atom(b"hdlr", &[&[0u8; 8][..], b"soun", &[0; 12]].concat());
        let mdhd = atom(b"mdhd", &[&[0u8; 12][..], &media_scale.to_be_bytes(), &[0; 4]].concat());
        let mut elst = vec![0, 0, 0, 0];
        elst.extend_from_slice(&(edits.len() as u32).to_be_bytes());
        for (d, t) in edits {
            elst.extend_from_slice(&[&d.to_be_bytes()[..], &t.to_be_bytes(), &[0, 1, 0, 0]].concat());
        }
        let trak = atom(b"trak", &[atom(b"edts", &atom(b"elst", &elst)), atom(b"mdia", &[mdhd, hdlr].concat())].concat());
        [atom(b"ftyp", b"M4A \0\0\0\0"), atom(b"moov", &[mvhd, trak].concat())].concat()
    }

    #[test]
    fn single_edit_gives_the_skip_and_length_in_media_units() {
        // 8 kHz media in a 1000-unit movie: 100 ms = 100 movie units = 800 media units.
        let r = edit_range(&mp4(1000, 8000, &[(100, 1024)])).unwrap();
        assert_eq!(r, EditRange { skip: 1024, len: Some(800), timescale: 8000 });
        assert_eq!(r.frames_at(8000), (1024, Some(800)));
        assert_eq!(r.frames_at(16000), (2048, Some(1600)));
    }

    #[test]
    fn an_empty_leading_edit_or_several_edits_are_left_alone() {
        assert_eq!(edit_range(&mp4(1000, 8000, &[(50, -1), (100, 1024)])), Some(EditRange { skip: 1024, len: Some(800), timescale: 8000 }));
        assert_eq!(edit_range(&mp4(1000, 8000, &[(100, 0), (100, 4000)])), None);
        assert_eq!(edit_range(&mp4(1000, 8000, &[(100, 1024), (100, -1)])), None);
    }

    #[test]
    fn garbage_is_none_not_a_panic() {
        assert_eq!(edit_range(&[]), None);
        assert_eq!(edit_range(b"\0\0\0\x10moov\xff\xff\xff\xff"), None);
        let good = mp4(1000, 8000, &[(100, 1024)]);
        for cut in 0..good.len() {
            let _ = edit_range(&good[..cut]);
        }
    }
}
