//! Automatic retiming: find the single time offset that lines a subtitle up
//! with the moments speech starts.
//!
//! Dialogue is timed by where lines *begin*, so that is what gets matched:
//! every cue start of the subtitle is looked up in a per-10 ms "something starts
//! here" signal (energy onsets of an audio track, or the cue starts of an
//! already-timed subtitle) and the offset with the highest total wins. The
//! level alone (is anybody talking?) says too little on dense, music-backed
//! dialogue, where almost every moment counts as speech; onsets stay sharp. On
//! a real 24-minute comedy episode the onset score stood 24 standard
//! deviations over chance where the level score reached 5.

use std::collections::HashMap;

use anyhow::{Result, bail};

use super::{Event, plaintext};

/// Resolution of the signals.
pub const BIN_MS: i64 = 10;

/// Events longer than this are watermarks or whole-episode signs, not speech.
const MAX_CUE_MS: i64 = 20_000;

/// Half-width in bins (100 ms) of the tolerance around each cue start of a
/// reference subtitle; people time the same line a few frames apart.
const START_TOLERANCE_BINS: usize = 10;

/// An event with an Effect and no more text than this is a karaoke syllable.
const GENERATED_MAX_CHARS: usize = 3;

/// Offsets scored for the background statistics whatever window is searched
/// (±120 s), so a narrow `--max-shift` cannot make a true peak look weak.
const BACKGROUND_BINS: i64 = 12_000;

/// Offsets this close to the winner are its own shoulder, not background (3 s).
const PEAK_GUARD_BINS: i64 = 300;

/// Offsets up to this size count as already in sync. Authors start a cue a
/// little before the speech, which the measurement reports as a small offset
/// of its own, and the onsets are only good to about 100 ms anyway.
pub const IN_SYNC_MS: i64 = 150;

/// "Something starts here" strength per 10 ms bin, zero mean so that cue
/// starts landing outside the signal score neutral.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Onsets {
    strength: Vec<f32>,
}

impl Onsets {
    /// From per-bin onset strengths, e.g. energy onsets of an audio track.
    pub fn from_strength(mut strength: Vec<f32>) -> Self {
        if !strength.is_empty() {
            let mean = strength.iter().map(|&s| f64::from(s)).sum::<f64>() / strength.len() as f64;
            for s in &mut strength {
                *s -= mean as f32;
            }
        }
        Self { strength }
    }

    /// From the cue starts of an already-timed subtitle: each start becomes a
    /// triangle ±100 ms wide, so the exact offset is the unique top.
    pub fn from_starts(starts: &[usize]) -> Self {
        let tol = START_TOLERANCE_BINS;
        let len = starts.iter().max().map_or(0, |m| m + tol + 1);
        let mut raw = vec![0f32; len];
        let norm = ((tol + 1) * (tol + 1)) as f32;
        for &s in starts {
            let from = s.saturating_sub(tol);
            for (i, slot) in raw
                .iter_mut()
                .enumerate()
                .skip(from)
                .take(s + tol + 1 - from)
            {
                *slot += (tol + 1 - i.abs_diff(s)) as f32 / norm;
            }
        }
        Self::from_strength(raw)
    }

    fn at(&self, i: i64) -> f64 {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.strength.get(i))
            .map_or(0.0, |&s| f64::from(s))
    }

    /// Nothing to align against: empty, or the same everywhere.
    fn is_flat(&self) -> bool {
        self.strength.iter().all(|s| s.abs() < 1e-9)
    }
}

/// Bins where the spoken cues of a subtitle start.
///
/// Left out: empty events, ASS drawings, events over 20 s, karaoke syllables
/// (an Effect plus at most three characters of text: a song can be hundreds of
/// per-syllable lines; real dialogue that merely carries an Effect stays) and
/// styles with far less cue time than the main one
/// (signs, opening and ending lyrics, notes). Time, not count: signs tracked
/// frame by frame are hundreds of 40 ms events and can outnumber the dialogue
/// without adding up to much. SRT and WebVTT have one style, so every cue counts.
pub fn cue_starts(events: &[Event]) -> Vec<usize> {
    let spoken: Vec<&Event> = events
        .iter()
        .filter(|e| e.end_ms > e.start_ms && e.end_ms - e.start_ms <= MAX_CUE_MS)
        .filter(|e| !is_drawing(&e.text) && is_spoken(e))
        .collect();
    let mut per_style: HashMap<&str, i64> = HashMap::new();
    for e in &spoken {
        *per_style.entry(e.style.as_str()).or_default() += e.end_ms - e.start_ms;
    }
    let busiest = per_style.values().copied().max().unwrap_or(0);
    spoken
        .into_iter()
        .filter(|e| per_style[e.style.as_str()] * 5 >= busiest)
        .map(|e| (e.start_ms.max(0) / BIN_MS) as usize)
        .collect()
}

/// Has visible text, and is not a karaoke syllable.
fn is_spoken(e: &Event) -> bool {
    let text = plaintext(&e.text);
    !text.is_empty() && (e.effect.is_empty() || text.chars().count() > GENERATED_MAX_CHARS)
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

/// Summed onset strength under the cue starts when the subtitle is moved
/// later by `lag` bins.
fn score(reference: &Onsets, starts: &[usize], lag: i64) -> f64 {
    starts.iter().map(|&s| reference.at(s as i64 + lag)).sum()
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
    /// Cue starts that took part.
    pub cues: usize,
}

impl Alignment {
    /// The subtitle already lines up, within [`IN_SYNC_MS`].
    pub fn in_sync(&self) -> bool {
        self.shift_ms.abs() <= IN_SYNC_MS
    }
}

/// Map a peak's height in standard deviations to 0..1: 6σ and below is 0
/// (random cue sets against a real episode's audio peaked at 5.5σ at most),
/// 20σ and above is 1 (a real episode against its own subtitle scored 20-27σ).
pub fn confidence(peak_sigma: f64) -> f64 {
    ((peak_sigma - 6.0) / 14.0).clamp(0.0, 1.0)
}

/// Best offset within `±max_shift_ms` that moves the cue `starts` onto `reference`.
pub fn align(reference: &Onsets, starts: &[usize], max_shift_ms: i64) -> Result<Alignment> {
    if reference.is_flat() {
        bail!("the reference has no speech onsets or cues to align against");
    }
    if starts.is_empty() {
        bail!("the subtitle has no timed text to align");
    }
    let max_lag = (max_shift_ms / BIN_MS).max(1);
    let scan = max_lag.max(BACKGROUND_BINS);
    let scores: Vec<f64> = (-scan..=scan)
        .map(|lag| score(reference, starts, lag))
        .collect();

    // Highest score within the searched window; ties go to the offset closest to zero.
    let distance = |i: usize| (i as i64 - scan).abs();
    let (best_idx, best) = scores
        .iter()
        .enumerate()
        .filter(|&(i, _)| distance(i) <= max_lag)
        .max_by(|a, b| {
            a.1.total_cmp(b.1)
                .then_with(|| distance(b.0).cmp(&distance(a.0)))
        })
        .map(|(i, &s)| (i, s))
        .expect("at least one lag is scored");

    // How far the peak stands above the scores of unrelated offsets: every
    // scored offset except the peak's own shoulder.
    let background: Vec<f64> = scores
        .iter()
        .enumerate()
        .filter(|&(i, _)| (i as i64 - best_idx as i64).abs() > PEAK_GUARD_BINS)
        .map(|(_, &s)| s)
        .collect();
    let n = background.len() as f64;
    let mean = background.iter().sum::<f64>() / n.max(1.0);
    let var = background.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / n.max(1.0);
    let peak_sigma = if var > 0.0 {
        (best - mean) / var.sqrt()
    } else {
        0.0
    };

    let lag = best_idx as i64 - scan;
    tracing::debug!(
        cues = starts.len(),
        best_ms = lag * BIN_MS,
        best_score = best,
        background_mean = mean,
        background_sigma = var.sqrt(),
        peak_sigma,
        "offset search"
    );
    Ok(Alignment {
        shift_ms: lag * BIN_MS,
        peak_sigma,
        confidence: confidence(peak_sigma),
        cues: starts.len(),
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

    /// Cue starts of dialogue-like speech: a line every 1.5-9 s over `minutes`.
    fn dialogue(seed: u64, minutes: usize) -> Vec<usize> {
        let mut rng = Rng(seed);
        let end = minutes * 60 * 100;
        let mut t = 500 + rng.below(500) as usize;
        let mut out = Vec::new();
        while t < end {
            out.push(t);
            t += 150 + rng.below(750) as usize;
        }
        out
    }

    /// Move starts by `bins`; whatever lands before zero is cut off, as in a real shift.
    fn shifted(starts: &[usize], bins: i64) -> Vec<usize> {
        starts
            .iter()
            .filter(|&&s| s as i64 + bins >= 0)
            .map(|&s| (s as i64 + bins) as usize)
            .collect()
    }

    #[test]
    fn finds_the_offset_in_both_directions() {
        let speech = dialogue(7, 8);
        let reference = Onsets::from_starts(&speech);
        for bins in [-1500i64, -37, 0, 1, 842, 5100] {
            // Subtitle that is `bins` too early must be delayed by `bins`.
            let found = align(&reference, &shifted(&speech, -bins), 120_000).unwrap();
            assert_eq!(found.shift_ms, bins * BIN_MS, "bins={bins}");
            assert!(found.confidence > 0.9, "{found:?}");
        }
    }

    #[test]
    fn survives_missing_spurious_and_jittered_cues() {
        let speech = dialogue(11, 10);
        let reference = Onsets::from_starts(&speech);
        let mut rng = Rng(99);
        // A tenth of the lines missing, signs where nobody speaks, every start
        // jittered by up to ±0.3 s.
        let mut starts: Vec<usize> = shifted(&speech, -2000)
            .into_iter()
            .filter_map(|s| {
                if rng.below(10) == 0 {
                    return None;
                }
                Some((s as i64 + rng.below(61) as i64 - 30).max(0) as usize)
            })
            .collect();
        for _ in 0..40 {
            starts.push(rng.below(55_000) as usize);
        }
        let found = align(&reference, &starts, 120_000).unwrap();
        // Jitter this wide only pins the offset down to about the width of it.
        assert!((found.shift_ms - 20_000).abs() <= 200, "{found:?}");
        assert!(found.confidence > 0.5, "{found:?}");
    }

    #[test]
    fn unrelated_signals_are_not_trusted() {
        for seed in 1..=8u64 {
            let reference = Onsets::from_starts(&dialogue(seed, 10));
            let starts = dialogue(seed * 1000 + 17, 10);
            let found = align(&reference, &starts, 120_000).unwrap();
            assert!(found.confidence < 0.1, "seed {seed}: {found:?}");
        }
    }

    #[test]
    fn a_narrow_search_window_does_not_weaken_the_peak() {
        let speech = dialogue(21, 10);
        let reference = Onsets::from_starts(&speech);
        let subs = shifted(&speech, -250);
        let wide = align(&reference, &subs, 120_000).unwrap();
        let narrow = align(&reference, &subs, 5_000).unwrap();
        assert_eq!(narrow.shift_ms, 2500);
        assert_eq!(wide.shift_ms, narrow.shift_ms);
        // The statistics come from the same fixed background either way.
        assert!((wide.peak_sigma - narrow.peak_sigma).abs() < 1e-9);
        assert!(narrow.confidence > 0.9, "{narrow:?}");
    }

    #[test]
    fn strength_signals_work_like_cue_signals() {
        // Onset strength with a sharp bump 250 bins after each cue start.
        let speech = dialogue(5, 6);
        let mut strength = vec![0f32; 6 * 60 * 100 + 1000];
        for &s in &speech {
            strength[s + 250] = 10.0;
        }
        let reference = Onsets::from_strength(strength);
        let found = align(&reference, &speech, 120_000).unwrap();
        assert_eq!(found.shift_ms, 2500);
        assert!(found.confidence > 0.9, "{found:?}");
    }

    #[test]
    fn small_offsets_count_as_in_sync() {
        let at = |shift_ms| Alignment {
            shift_ms,
            peak_sigma: 20.0,
            confidence: 1.0,
            cues: 100,
        };
        assert!(at(0).in_sync() && at(100).in_sync() && at(-150).in_sync());
        assert!(!at(160).in_sync() && !at(-2500).in_sync());
    }

    #[test]
    fn empty_or_flat_sides_are_errors() {
        let some = Onsets::from_starts(&[100, 400, 900]);
        assert!(align(&Onsets::default(), &[100], 1000).is_err());
        assert!(align(&Onsets::from_strength(vec![3.0; 500]), &[100], 1000).is_err());
        assert!(align(&some, &[], 1000).is_err());
    }

    fn ev(start: i64, end: i64, style: &str, effect: &str, text: &str) -> Event {
        Event {
            start_ms: start,
            end_ms: end,
            style: style.to_string(),
            effect: effect.to_string(),
            text: text.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn cue_starts_skip_what_is_not_dialogue() {
        let events = vec![
            ev(1000, 2500, "", "", "Hello"),
            ev(3000, 3000, "", "", "zero length"),
            ev(4000, 5000, "", "", "{\\an8}"),
            ev(6000, 7000, "", "", "{\\p1}m 0 0 l 10 0 10 10{\\p0}"),
            ev(8000, 9000, "", "", "{\\pos(10,10)}Positioned"),
            ev(0, 600_000, "", "", "Subbed by someone"),
            ev(10_005, 10_012, "", "", "Short"),
            ev(11_000, 12_000, "", "fx", "k"),
            ev(13_000, 14_000, "", "fx", "A real line"),
        ];
        // The karaoke syllable goes; dialogue that merely carries an Effect stays.
        assert_eq!(cue_starts(&events), vec![100, 800, 1000, 1300]);
    }

    #[test]
    fn cue_starts_keep_the_dialogue_styles_only() {
        let mut events = Vec::new();
        // 20 dialogue lines, 6 in a second dialogue style, 2 song lyrics, 1 sign.
        for i in 0..20 {
            events.push(ev(i * 4000, i * 4000 + 2000, "Default", "", "Line"));
        }
        for i in 0..6 {
            events.push(ev(
                90_000 + i * 4000,
                92_000 + i * 4000,
                "Italics",
                "",
                "Inner",
            ));
        }
        events.push(ev(200_000, 203_000, "OP", "", "Lyrics one"));
        events.push(ev(204_000, 207_000, "OP", "", "Lyrics two"));
        events.push(ev(300_000, 302_000, "Sign", "", "Shop"));
        let starts = cue_starts(&events);
        assert_eq!(starts.len(), 26);
        assert!(!starts.contains(&20_000) && !starts.contains(&30_000));
        // A single-style script (SRT) keeps everything.
        let srt: Vec<Event> = (0..3)
            .map(|i| ev(i * 1000, i * 1000 + 500, "", "", "x"))
            .collect();
        assert_eq!(cue_starts(&srt).len(), 3);
    }

    #[test]
    fn frame_tracked_signs_do_not_outweigh_the_dialogue() {
        // 100 lines of dialogue (200 s) against 600 one-frame sign events
        // (24 s): the signs are six times as many but a tenth of the time.
        let mut events: Vec<Event> = (0..100)
            .map(|i| ev(i * 5000, i * 5000 + 2000, "Dialogue", "", "Line"))
            .collect();
        events.extend((0..600).map(|i| ev(i * 800, i * 800 + 40, "Sign", "", "Shop")));
        let starts = cue_starts(&events);
        assert_eq!(starts.len(), 100);
        assert!(starts.iter().all(|s| s % 500 == 0), "only dialogue starts");
    }
}
