//! Speech-activity detection for automatic retiming.
//!
//! ffmpeg decodes one audio track to 8 kHz mono PCM, band-limited to the voice
//! range so music and effects count less. An adaptive energy gate then marks
//! each 10 ms frame as speech or not. This is a plain energy detector, not a
//! trained VAD: background music at dialogue level blurs it, which is why the
//! retimer also accepts an already-timed subtitle as the reference.

use std::ops::Range;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;

/// 10 ms of audio at 8 kHz; matches `subs::sync::BIN_MS`.
const FRAME_SAMPLES: usize = 80;
const FRAME_BYTES: usize = FRAME_SAMPLES * 2;

/// Gaps shorter than this inside speech are bridged (breaths, plosives).
const BRIDGE_FRAMES: usize = 30;
/// Bursts shorter than this are clicks, not speech.
const MIN_SPEECH_FRAMES: usize = 10;
/// Speech must sit within this many dB of the loud end of the track.
const DYNAMIC_RANGE_DB: f32 = 25.0;
/// A track flatter than this has no speech/pause contrast to find.
const MIN_CONTRAST_DB: f32 = 6.0;

/// Level of one 10 ms frame of little-endian `i16` PCM, in dB below full scale.
fn frame_db(frame: &[u8]) -> f32 {
    let sum: f64 = frame
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| {
            let s = f64::from(i16::from_le_bytes(*b));
            s * s
        })
        .sum();
    let rms = (sum / (frame.len() / 2) as f64).sqrt() / 32768.0;
    (20.0 * rms.max(1e-5).log10()) as f32
}

/// Speech runs (in 10 ms frames) from per-frame levels.
///
/// The gate sits between the quiet floor (10th percentile) and the loud end
/// (90th), never more than [`DYNAMIC_RANGE_DB`] below the loud end; short gaps
/// are closed first, then short bursts dropped.
pub fn speech_ranges(db: &[f32]) -> Vec<Range<usize>> {
    if db.is_empty() {
        return Vec::new();
    }
    let mut sorted = db.to_vec();
    sorted.sort_by(f32::total_cmp);
    let at = |q: f32| sorted[((sorted.len() - 1) as f32 * q) as usize];
    let (floor, loud) = (at(0.10), at(0.90));
    if loud - floor < MIN_CONTRAST_DB {
        return Vec::new();
    }
    let gate = (floor + 0.45 * (loud - floor)).max(loud - DYNAMIC_RANGE_DB);

    // Runs of frames above the gate.
    let mut runs: Vec<Range<usize>> = Vec::new();
    for (i, &level) in db.iter().enumerate() {
        if level <= gate {
            continue;
        }
        match runs.last_mut() {
            Some(last) if i - last.end < BRIDGE_FRAMES => last.end = i + 1,
            _ => runs.push(i..i + 1),
        }
    }
    runs.retain(|r| r.len() >= MIN_SPEECH_FRAMES);
    runs
}

/// Decode audio stream `audio_track` (0 = first) of `media` and return where
/// speech is, in 10 ms frames.
pub async fn detect(ffmpeg: &Path, media: &Path, audio_track: u32) -> Result<Vec<Range<usize>>> {
    let mut child = tokio::process::Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(media)
        .arg("-map")
        .arg(format!("0:a:{audio_track}"))
        .args(["-ac", "1", "-ar", "8000"])
        .args(["-af", "highpass=f=200,lowpass=f=3400"])
        .args(["-f", "s16le", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot run {}", ffmpeg.display()))?;

    let mut stdout = child.stdout.take().context("ffmpeg stdout unavailable")?;
    let mut stderr = child.stderr.take().context("ffmpeg stderr unavailable")?;
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });

    // Fold the stream into frame levels as it arrives; 25 minutes is ~150k floats.
    let mut db = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = stdout.read(&mut chunk).await.context("reading audio")?;
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..n]);
        let (frames, _) = pending.as_chunks::<FRAME_BYTES>();
        let usable = frames.len() * FRAME_BYTES;
        db.extend(frames.iter().map(|f| frame_db(f)));
        pending.drain(..usable);
    }

    let status = child.wait().await.context("waiting for ffmpeg")?;
    let errors = errors.await.unwrap_or_default();
    if !status.success() {
        let tail: String = errors
            .trim()
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .join(" | ");
        bail!(
            "ffmpeg could not decode audio track {audio_track} of {}: {tail}",
            media.display()
        );
    }
    if db.is_empty() {
        bail!("no audio decoded from {}", media.display());
    }
    Ok(speech_ranges(&db))
}

/// 16-bit mono PCM WAV file bytes, for tests that need real audio.
#[cfg(test)]
pub(crate) fn wav_bytes(rate: u32, samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 ms frame levels: `speech` ranges at -20 dB over a -60 dB floor.
    fn levels(len: usize, speech: &[Range<usize>]) -> Vec<f32> {
        let mut db = vec![-60.0; len];
        for r in speech {
            db[r.clone()].fill(-20.0);
        }
        db
    }

    #[test]
    fn frame_level_matches_amplitude() {
        let loud: Vec<u8> = (0..FRAME_SAMPLES)
            .flat_map(|_| 16384i16.to_le_bytes())
            .collect();
        assert!((frame_db(&loud) - -6.02).abs() < 0.05);
        let silent = vec![0u8; FRAME_BYTES];
        assert!(frame_db(&silent) <= -99.0);
    }

    #[test]
    fn finds_bursts_and_bridges_short_gaps() {
        // 1.0-3.0 s, a 0.2 s breath, 3.2-4.0 s, then a 40 ms click, then 8.0-9.0 s.
        let db = levels(1200, &[100..300, 320..400, 600..604, 800..900]);
        assert_eq!(speech_ranges(&db), vec![100..400, 800..900]);
    }

    #[test]
    fn flat_or_empty_audio_has_no_speech() {
        assert!(speech_ranges(&[]).is_empty());
        assert!(speech_ranges(&vec![-30.0; 5000]).is_empty());
        let hum: Vec<f32> = (0..5000).map(|i| -30.0 + (i % 3) as f32).collect();
        assert!(speech_ranges(&hum).is_empty());
    }

    #[test]
    fn quiet_music_below_the_dialogue_range_is_ignored() {
        // Dialogue at -20 dB; music bed at -50 dB (30 dB down) must not count.
        let mut db = levels(2000, &[200..400, 900..1100, 1500..1700]);
        for v in db.iter_mut().filter(|v| **v < -55.0) {
            *v = -50.0;
        }
        assert_eq!(speech_ranges(&db), vec![200..400, 900..1100, 1500..1700]);
    }

    /// Real ffmpeg decode of a generated WAV: tones where "speech" is, near
    /// silence elsewhere. Needs ffmpeg — run explicitly: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn e2e_detects_tone_bursts_from_a_wav() {
        let rate = 16_000usize;
        let bursts = [(1.0f64, 2.5f64), (4.0, 5.0), (7.0, 9.0)];
        let samples: Vec<i16> = (0..rate * 10)
            .map(|i| {
                let t = i as f64 / rate as f64;
                let on = bursts.iter().any(|&(a, b)| t >= a && t < b);
                let amp = if on { 12000.0 } else { 30.0 };
                let wave = (t * 440.0 * std::f64::consts::TAU).sin()
                    + 0.5 * (t * 1100.0 * std::f64::consts::TAU).sin();
                (amp * wave) as i16
            })
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("bursts.wav");
        std::fs::write(&wav, wav_bytes(rate as u32, &samples)).unwrap();

        let (ffmpeg, _) = crate::common::tools::ffmpeg_pair().expect("ffmpeg missing");
        let found = detect(&ffmpeg, &wav, 0).await.unwrap();
        assert_eq!(found.len(), bursts.len(), "{found:?}");
        for (r, &(a, b)) in found.iter().zip(&bursts) {
            // Within 60 ms of the true edges (filter and resampler ringing).
            assert!((r.start as f64 - a * 100.0).abs() <= 6.0, "{r:?} vs {a}");
            assert!((r.end as f64 - b * 100.0).abs() <= 6.0, "{r:?} vs {b}");
        }
        // A missing audio track is an error that names the track.
        let err = detect(&ffmpeg, &wav, 3).await.unwrap_err().to_string();
        assert!(err.contains("audio track 3"), "{err}");
    }
}
