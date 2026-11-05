//! Automatic retiming: find the single time offset that best lines a subtitle
//! up with a reference "something is being said" signal, either speech detected
//! in an audio track or the cues of an already-timed subtitle.
//!
//! Both sides are reduced to on/off bits at 10 ms resolution. Every candidate
//! offset is scored as `matches - mismatches` over the overlap (the idea behind
//! ffsubsync); bit-packed popcounts keep a ±2 minute search over a 25-minute
//! episode well under a second.

use std::cmp::Reverse;
use std::ops::Range;

use anyhow::{Result, bail};

use super::{Event, plaintext};

/// Resolution of the on/off signals.
pub const BIN_MS: i64 = 10;

/// Events longer than this are watermarks or whole-episode signs, not speech.
const MAX_CUE_MS: i64 = 20_000;

/// Fixed-length bit set; bits past `len` stay zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bits {
    words: Vec<u64>,
    len: usize,
}

/// Bits `lo..hi` of a word (`lo < hi <= 64`).
fn mask(lo: usize, hi: usize) -> u64 {
    let width = hi - lo;
    let ones = if width == 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    ones << lo
}

impl Bits {
    /// Bits set over every range; the length is the furthest range end.
    pub fn from_ranges(ranges: impl IntoIterator<Item = Range<usize>>) -> Self {
        let ranges: Vec<Range<usize>> = ranges.into_iter().collect();
        let len = ranges.iter().map(|r| r.end).max().unwrap_or(0);
        let mut bits = Self {
            words: vec![0; len.div_ceil(64)],
            len,
        };
        for r in &ranges {
            bits.set_range(r.start, r.end);
        }
        bits
    }

    fn set_range(&mut self, start: usize, end: usize) {
        let end = end.min(self.len);
        if start >= end {
            return;
        }
        let (first, last) = (start / 64, (end - 1) / 64);
        for w in first..=last {
            let lo = if w == first { start % 64 } else { 0 };
            let hi = if w == last { (end - 1) % 64 + 1 } else { 64 };
            self.words[w] |= mask(lo, hi);
        }
    }

    pub fn ones(&self) -> u64 {
        self.words.iter().map(|w| u64::from(w.count_ones())).sum()
    }

    fn word(&self, i: i64) -> u64 {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.words.get(i))
            .copied()
            .unwrap_or(0)
    }

    /// 64 bits starting at signed bit position `pos`; outside the set reads as 0.
    fn window(&self, pos: i64) -> u64 {
        let (w, off) = (pos.div_euclid(64), pos.rem_euclid(64) as u32);
        if off == 0 {
            self.word(w)
        } else {
            (self.word(w) >> off) | (self.word(w + 1) << (64 - off))
        }
    }
}

#[cfg(test)]
impl Bits {
    fn len(&self) -> usize {
        self.len
    }

    fn get(&self, i: usize) -> bool {
        self.words
            .get(i / 64)
            .is_some_and(|w| (w >> (i % 64)) & 1 == 1)
    }
}

/// Bin ranges of the cues that carry spoken text: empty, drawing-only and
/// whole-episode events are left out.
pub fn cue_ranges(events: &[Event]) -> Vec<Range<usize>> {
    events
        .iter()
        .filter(|e| e.end_ms > e.start_ms && e.end_ms - e.start_ms <= MAX_CUE_MS)
        .filter(|e| !is_drawing(&e.text) && !plaintext(&e.text).is_empty())
        .map(|e| {
            let start = e.start_ms.max(0) / BIN_MS;
            let end = (e.end_ms.max(0) + BIN_MS - 1) / BIN_MS;
            start as usize..end as usize
        })
        .collect()
}

/// ASS vector drawing (`{\p1}m 0 0 l ...`); `\pos(` is not one.
fn is_drawing(text: &str) -> bool {
    text.match_indices("\\p").any(|(i, _)| {
        text[i + 2..]
            .chars()
            .next()
            .is_some_and(|c| ('1'..='9').contains(&c))
    })
}

/// `(sum, overlap length)` of `count(speech word, subs word, mask)` when `subs`
/// is moved later by `lag` bins; only bins present on both sides take part.
fn fold_overlap(
    speech: &Bits,
    subs: &Bits,
    lag: i64,
    count: impl Fn(u64, u64, u64) -> u32,
) -> (i64, i64) {
    let lo = lag.max(0);
    let hi = (speech.len as i64).min(subs.len as i64 + lag);
    if hi <= lo {
        return (0, 0);
    }
    let mut sum = 0i64;
    for w in lo / 64..=(hi - 1) / 64 {
        let base = w * 64;
        let m = mask((lo - base).max(0) as usize, (hi - base).min(64) as usize);
        sum += i64::from(count(speech.word(w), subs.window(base - lag), m));
    }
    (sum, hi - lo)
}

/// `matches - mismatches` for one lag; bins outside the overlap score 0.
fn score(speech: &Bits, subs: &Bits, lag: i64) -> i64 {
    let (matches, overlap) = fold_overlap(speech, subs, lag, |a, b, m| (!(a ^ b) & m).count_ones());
    2 * matches - overlap
}

/// Result of an offset search.
#[derive(Debug, Clone, PartialEq)]
pub struct Alignment {
    /// Positive delays the subtitle, negative advances it.
    pub shift_ms: i64,
    /// Peak height in standard deviations above the typical offset's score.
    pub peak_sigma: f64,
    /// 0..1 certainty derived from `peak_sigma` (see [`confidence`]).
    pub confidence: f64,
    /// Share of the subtitle's cue time that falls on speech at the best offset.
    pub coverage: f64,
}

/// Map a peak's height in standard deviations to 0..1: 4σ and below is 0 (unrelated
/// dialogue-like signals peak at about 4.3σ by chance, measured over 30 pairs),
/// 12σ and above is 1 (an exact match).
pub fn confidence(peak_sigma: f64) -> f64 {
    ((peak_sigma - 4.0) / 8.0).clamp(0.0, 1.0)
}

/// Best offset within `±max_shift_ms` that moves `subs` onto `reference`.
pub fn align(reference: &Bits, subs: &Bits, max_shift_ms: i64) -> Result<Alignment> {
    if reference.ones() == 0 {
        bail!("the reference has no speech or cues to align against");
    }
    if subs.ones() == 0 {
        bail!("the subtitle has no timed text to align");
    }
    let max_lag = (max_shift_ms / BIN_MS).max(1);
    let scores: Vec<i64> = (-max_lag..=max_lag)
        .map(|lag| score(reference, subs, lag))
        .collect();

    // Highest score; ties go to the offset closest to zero.
    let (best_idx, best) = scores
        .iter()
        .enumerate()
        .max_by_key(|&(i, &s)| (s, Reverse((i as i64 - max_lag).abs())))
        .map(|(i, &s)| (i, s))
        .expect("at least one lag is scored");
    let lag = best_idx as i64 - max_lag;

    let n = scores.len() as f64;
    let mean = scores.iter().map(|&s| s as f64).sum::<f64>() / n;
    let var = scores
        .iter()
        .map(|&s| (s as f64 - mean).powi(2))
        .sum::<f64>()
        / n;
    let peak_sigma = if var > 0.0 {
        (best as f64 - mean) / var.sqrt()
    } else {
        0.0
    };

    let (on_speech, _) = fold_overlap(reference, subs, lag, |a, b, m| (a & b & m).count_ones());
    Ok(Alignment {
        shift_ms: lag * BIN_MS,
        peak_sigma,
        confidence: confidence(peak_sigma),
        coverage: on_speech as f64 / subs.ones() as f64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subs::Event;

    /// Small deterministic generator so tests never depend on a crate or the clock.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Dialogue-like cues: 1-4 s long, 0.5-6 s apart, over `minutes`.
    fn dialogue(seed: u64, minutes: usize) -> Vec<Range<usize>> {
        let mut rng = Rng(seed);
        let end = minutes * 60 * 100;
        let mut t = 500 + rng.below(500) as usize;
        let mut out = Vec::new();
        while t < end {
            let len = 100 + rng.below(300) as usize;
            out.push(t..t + len);
            t += len + 50 + rng.below(550) as usize;
        }
        out
    }

    /// Move ranges by `bins`; whatever lands before zero is cut off, as in a real shift.
    fn shifted(ranges: &[Range<usize>], bins: i64) -> Vec<Range<usize>> {
        ranges
            .iter()
            .filter(|r| r.end as i64 + bins > 0)
            .map(|r| {
                let start = (r.start as i64 + bins).max(0) as usize;
                start..(r.end as i64 + bins) as usize
            })
            .collect()
    }

    #[test]
    fn bits_cross_word_boundaries() {
        let bits = Bits::from_ranges([60..70, 128..129]);
        assert_eq!(bits.len(), 129);
        assert_eq!(bits.ones(), 11);
        assert!(bits.get(60) && bits.get(69) && bits.get(128));
        assert!(!bits.get(59) && !bits.get(70) && !bits.get(127) && !bits.get(500));
        // Windows that start inside the run, before it, and straddle a word edge.
        assert_eq!(bits.window(62), 0xFF);
        assert_eq!(bits.window(56), 0x3FF << 4);
        assert_eq!(bits.window(1), 0b1_1111 << 59);
        assert_eq!(bits.window(-4), 0);
        assert_eq!(bits.window(1000), 0);
        assert_eq!(bits.window(-1000), 0);
    }

    #[test]
    fn full_word_ranges_do_not_overflow() {
        let bits = Bits::from_ranges(std::iter::once(0..192));
        assert_eq!(bits.ones(), 192);
        assert_eq!(bits.window(64), u64::MAX);
    }

    #[test]
    fn finds_the_offset_in_both_directions() {
        let speech = dialogue(7, 8);
        let reference = Bits::from_ranges(speech.clone());
        for bins in [-1500i64, -37, 0, 1, 842, 5100] {
            // Subtitle that is `bins` too early must be delayed by `bins`.
            let subs = Bits::from_ranges(shifted(&speech, -bins));
            let found = align(&reference, &subs, 120_000).unwrap();
            assert_eq!(found.shift_ms, bins * BIN_MS, "bins={bins}");
            assert!(found.confidence > 0.9, "{found:?}");
            assert!(found.coverage > 0.99, "{found:?}");
        }
    }

    #[test]
    fn survives_missing_and_spurious_cues() {
        let speech = dialogue(11, 10);
        let reference = Bits::from_ranges(speech.clone());
        let mut rng = Rng(99);
        // Subtitle: a tenth of the lines missing, some signs where nobody speaks,
        // every line jittered by up to ±0.3 s on both edges.
        let mut cues: Vec<Range<usize>> = shifted(&speech, -2000)
            .into_iter()
            .filter_map(|r| {
                if rng.below(10) == 0 {
                    return None;
                }
                let a = (r.start as i64 + rng.below(61) as i64 - 30).max(0) as usize;
                let b = (r.end as i64 + rng.below(61) as i64 - 30) as usize;
                Some(a..b.max(a + 1))
            })
            .collect();
        for _ in 0..40 {
            let at = rng.below(55_000) as usize;
            cues.push(at..at + 150);
        }
        let found = align(&reference, &Bits::from_ranges(cues), 120_000).unwrap();
        assert!((found.shift_ms - 20_000).abs() <= 100, "{found:?}");
        assert!(found.confidence > 0.5, "{found:?}");
    }

    #[test]
    fn unrelated_signals_are_not_trusted() {
        for seed in 1..=6u64 {
            let reference = Bits::from_ranges(dialogue(seed, 10));
            let subs = Bits::from_ranges(dialogue(seed * 1000 + 17, 10));
            let found = align(&reference, &subs, 120_000).unwrap();
            assert!(found.confidence < 0.1, "seed {seed}: {found:?}");
        }
    }

    #[test]
    fn empty_sides_are_errors() {
        let some = Bits::from_ranges(std::iter::once(0..100));
        let none = Bits::from_ranges([]);
        assert!(align(&none, &some, 1000).is_err());
        assert!(align(&some, &none, 1000).is_err());
    }

    #[test]
    fn cue_ranges_skip_non_speech() {
        let ev = |start, end, text: &str| Event {
            start_ms: start,
            end_ms: end,
            text: text.to_string(),
            ..Default::default()
        };
        let events = vec![
            ev(1000, 2500, "Hello"),
            ev(3000, 3000, "zero length"),
            ev(4000, 5000, "{\\an8}"),
            ev(6000, 7000, "{\\p1}m 0 0 l 10 0 10 10{\\p0}"),
            ev(8000, 9000, "{\\pos(10,10)}Positioned"),
            ev(0, 600_000, "Subbed by someone"),
            ev(10_005, 10_012, "Short"),
        ];
        assert_eq!(cue_ranges(&events), vec![100..250, 800..900, 1000..1002]);
    }
}
