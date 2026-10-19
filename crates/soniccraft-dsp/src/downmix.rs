//! Mixing sources with more than two channels down to stereo (ITU-R BS.775).
//!
//! DESIGN.md §4.4 only defines mono and stereo sources, so the decoders mix wider sources down to
//! stereo with the ITU-R BS.775 3/2 coefficients:
//!
//! ```text
//! Lo = L + 0.7071·C + 0.7071·Ls        Ro = R + 0.7071·C + 0.7071·Rs        (LFE dropped)
//! ```
//!
//! The other WAVE speaker positions follow the same idea: front left/right-of-centre and wide channels
//! count as front channels (gain 1), side, back, top and bottom left/right channels as surrounds
//! (0.7071 on their side), centre positions other than the front centre (back, top, bottom) are split
//! equally (0.5 into each side), and LFE channels and channels without a speaker position are
//! dropped. The sum is not normalized (the master limiter takes care of peaks). A mono mix of a wide
//! source is the mono mix of this stereo downmix, `(Lo + Ro)/2`.
//!
//! The layout comes from the file when it has one (WAV channel mask, the codec's channel map).
//! Otherwise the WAVE speaker order is assumed: channel `k` sits at the `k`-th position of the
//! WAVE_FORMAT_EXTENSIBLE channel mask (`FL FR FC LFE BL BR FLC FRC BC SL SR TC ...`), so six channels
//! are 5.1 `L R C LFE Ls Rs`.

use crate::AudioBuffer;

/// `1/√2` (−3 dB).
const S: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Speaker positions by bit index: the 18 WAVE_FORMAT_EXTENSIBLE positions (bits 0–17), then the
/// extra positions symphonia defines (bits 18–25). `(name, gain into Lo, gain into Ro)`.
const POSITIONS: [(&str, f32, f32); 26] = [
    ("FL", 1.0, 0.0),
    ("FR", 0.0, 1.0),
    ("FC", S, S),
    ("LFE", 0.0, 0.0),
    ("BL", S, 0.0),
    ("BR", 0.0, S),
    ("FLC", 1.0, 0.0),
    ("FRC", 0.0, 1.0),
    ("BC", 0.5, 0.5),
    ("SL", S, 0.0),
    ("SR", 0.0, S),
    ("TC", 0.5, 0.5),
    ("TFL", S, 0.0),
    ("TFC", 0.5, 0.5),
    ("TFR", 0.0, S),
    ("TBL", S, 0.0),
    ("TBC", 0.5, 0.5),
    ("TBR", 0.0, S),
    ("LFE2", 0.0, 0.0),
    ("TSL", S, 0.0),
    ("TSR", 0.0, S),
    ("BFC", 0.5, 0.5),
    ("BFL", S, 0.0),
    ("BFR", 0.0, S),
    ("FLW", 1.0, 0.0),
    ("FRW", 0.0, 1.0),
];

/// Number of positions in a WAVE channel mask.
const WAVE_POSITIONS: usize = 18;

/// What a downmix did, for diagnostics (`channels_downmixed`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downmix {
    /// Channel count of the source.
    pub source_channels: usize,
    /// Speaker position of every source channel in file order, space-separated (`FL FR FC LFE BL BR`
    /// for 5.1; `?` for a channel without a position).
    pub layout: String,
    /// Whether the layout comes from the file (`false`: assumed from the WAVE speaker order).
    pub layout_from_file: bool,
    /// Positions of the source channels left out of the stereo mix (LFE, `?`), in file order.
    pub dropped: Vec<String>,
}

/// Where a decoder found (or did not find) the speaker layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Layout {
    /// No layout information: assume the WAVE speaker order.
    Unknown,
    /// A WAVE_FORMAT_EXTENSIBLE-style bit mask: the set bits, in increasing order, are the positions of
    /// the channels in file order (channels beyond the set bits have no position).
    Mask(u64),
    /// One position (bit index) per channel, `None` for a channel without one.
    Positions(Vec<Option<u32>>),
}

impl Layout {
    /// The position (bit index) of each of `channels` channels, and whether it came from the file.
    fn resolve(&self, channels: usize) -> (Vec<Option<u32>>, bool) {
        match self {
            Layout::Mask(mask) if *mask != 0 => {
                let mut bits = (0..64u32).filter(|b| mask & (1u64 << b) != 0);
                ((0..channels).map(|_| bits.next()).collect(), true)
            }
            Layout::Positions(positions) if positions.len() == channels => {
                (positions.clone(), true)
            }
            _ => (
                (0..channels)
                    .map(|c| (c < WAVE_POSITIONS).then_some(c as u32))
                    .collect(),
                false,
            ),
        }
    }
}

fn position(bit: Option<u32>) -> (&'static str, f32, f32) {
    bit.and_then(|b| POSITIONS.get(b as usize))
        .copied()
        .unwrap_or(("?", 0.0, 0.0))
}

/// Mix a buffer with more than two channels down to stereo, assuming the WAVE speaker order (see the
/// module docs). Returns `None` for mono and stereo buffers, which need no downmix.
pub fn to_stereo(buffer: &AudioBuffer) -> Option<(AudioBuffer, Downmix)> {
    to_stereo_with(buffer, &Layout::Unknown)
}

/// [`to_stereo`] with the layout a decoder found.
pub(crate) fn to_stereo_with(
    buffer: &AudioBuffer,
    layout: &Layout,
) -> Option<(AudioBuffer, Downmix)> {
    let channels = buffer.num_channels();
    if channels <= 2 {
        return None;
    }
    let frames = buffer.frames();
    let (bits, layout_from_file) = layout.resolve(channels);
    let mut left = vec![0.0f32; frames];
    let mut right = vec![0.0f32; frames];
    let mut names = Vec::with_capacity(channels);
    let mut dropped = Vec::new();
    for (ch, &bit) in buffer.channels.iter().zip(&bits) {
        let (name, gl, gr) = position(bit);
        names.push(name);
        if gl == 0.0 && gr == 0.0 {
            dropped.push(name.to_string());
            continue;
        }
        // Channels are added in file order, so the result is deterministic.
        for (side, gain) in [(&mut left, gl), (&mut right, gr)] {
            if gain != 0.0 {
                for (o, &x) in side.iter_mut().zip(ch) {
                    *o += gain * x;
                }
            }
        }
    }
    let info = Downmix {
        source_channels: channels,
        layout: names.join(" "),
        layout_from_file,
        dropped,
    };
    Some((
        AudioBuffer::new(buffer.sample_rate, vec![left, right]),
        info,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn bs775_five_one_in_wave_order() {
        // L R C LFE Ls Rs, each channel a distinct value.
        let buf = AudioBuffer::new(
            48_000,
            vec![
                vec![1.0],
                vec![2.0],
                vec![4.0],
                vec![100.0],
                vec![8.0],
                vec![16.0],
            ],
        );
        let (out, info) = to_stereo(&buf).unwrap();
        assert!(close(out.channels[0][0], 1.0 + S * 4.0 + S * 8.0));
        assert!(close(out.channels[1][0], 2.0 + S * 4.0 + S * 16.0));
        assert_eq!(info.layout, "FL FR FC LFE BL BR");
        assert!(!info.layout_from_file);
        assert_eq!(info.dropped, vec!["LFE"]);
        assert_eq!(info.source_channels, 6);
        // A centre-only 5.1 source is not silent.
        let centre = AudioBuffer::new(
            8000,
            vec![
                vec![0.0],
                vec![0.0],
                vec![0.5],
                vec![0.0],
                vec![0.0],
                vec![0.0],
            ],
        );
        let (out, _) = to_stereo(&centre).unwrap();
        assert!(close(out.channels[0][0], 0.5 * S) && close(out.channels[1][0], 0.5 * S));
        assert!(close(out.to_mono().channels[0][0], 0.5 * S));
    }

    #[test]
    fn mono_and_stereo_are_untouched() {
        assert!(to_stereo(&AudioBuffer::mono(8000, vec![0.1])).is_none());
        assert!(to_stereo(&AudioBuffer::silent(8000, 2, 4)).is_none());
    }

    #[test]
    fn layouts_from_the_file() {
        // AAC-style order C L R Ls Rs LFE given per channel.
        let buf = AudioBuffer::new(
            8000,
            vec![
                vec![4.0],
                vec![1.0],
                vec![2.0],
                vec![8.0],
                vec![16.0],
                vec![100.0],
            ],
        );
        let layout = Layout::Positions(vec![Some(2), Some(0), Some(1), Some(9), Some(10), Some(3)]);
        let (out, info) = to_stereo_with(&buf, &layout).unwrap();
        assert!(close(out.channels[0][0], 4.0 * S + 1.0 + 8.0 * S));
        assert!(close(out.channels[1][0], 4.0 * S + 2.0 + 16.0 * S));
        assert_eq!(info.layout, "FC FL FR SL SR LFE");
        assert!(info.layout_from_file);
        // A WAV mask with fewer positions than channels leaves the rest unassigned (dropped).
        let three = AudioBuffer::new(8000, vec![vec![1.0], vec![2.0], vec![3.0]]);
        let (out, info) = to_stereo_with(&three, &Layout::Mask(0b11)).unwrap();
        assert_eq!(out.channels, vec![vec![1.0], vec![2.0]]);
        assert_eq!(info.layout, "FL FR ?");
        assert_eq!(info.dropped, vec!["?"]);
        // Mask 0 means "no assignment": the WAVE order is assumed.
        let (_, info) = to_stereo_with(&three, &Layout::Mask(0)).unwrap();
        assert_eq!(
            (info.layout.as_str(), info.layout_from_file),
            ("FL FR FC", false)
        );
        // Positions of the wrong length are ignored too.
        let (_, info) = to_stereo_with(&three, &Layout::Positions(vec![Some(0)])).unwrap();
        assert!(!info.layout_from_file);
    }

    #[test]
    fn many_channels_never_panic() {
        let buf = AudioBuffer::silent(8000, 40, 3);
        let (out, info) = to_stereo(&buf).unwrap();
        assert_eq!((out.num_channels(), out.frames()), (2, 3));
        assert_eq!(info.layout.split(' ').count(), 40);
        assert_eq!(
            info.dropped.len(),
            1 + 22,
            "LFE and the 22 channels past the WAVE positions"
        );
        let (_, info) = to_stereo_with(&buf, &Layout::Mask(u64::MAX)).unwrap();
        assert!(info.layout.ends_with('?'));
    }
}
