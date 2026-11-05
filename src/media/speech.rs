//! Speech-onset detection for automatic retiming.
//!
//! ffmpeg decodes one audio track to 8 kHz mono PCM, band-limited to the voice
//! range so music and effects count less. Each 10 ms frame gets a level in dB,
//! and the onset strength of a frame is how much louder the next 80 ms are than
//! the previous 80 ms. That marks where lines of dialogue begin, which is what
//! subtitle starts are timed to. It stays informative on dense, music-backed
//! dialogue where an on/off speech detector calls nearly every moment speech
//! (83% of a real comedy episode), and it is a plain energy measure, not a
//! trained detector: a loud music or effects onset counts like a voice.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;

/// 10 ms of audio at 8 kHz; matches `subs::sync::BIN_MS`.
const FRAME_SAMPLES: usize = 80;
const FRAME_BYTES: usize = FRAME_SAMPLES * 2;

/// Half-width in frames (80 ms) of the windows compared on either side of a frame.
const EDGE_FRAMES: usize = 8;
/// Half-width in frames of the smoothing of the level (20 ms) and of the onset curve (50 ms).
const LEVEL_SMOOTH_FRAMES: usize = 2;
const ONSET_SMOOTH_FRAMES: usize = 5;
/// Level jumps are clipped here so one explosion cannot outweigh a hundred lines.
const MAX_JUMP_DB: f32 = 20.0;

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

/// Moving average over `±half` frames (shorter at the ends).
fn box_smooth(values: &[f32], half: usize) -> Vec<f32> {
    let mut prefix = vec![0f64; values.len() + 1];
    for (i, &v) in values.iter().enumerate() {
        prefix[i + 1] = prefix[i] + f64::from(v);
    }
    (0..values.len())
        .map(|i| {
            let (a, b) = (i.saturating_sub(half), (i + half + 1).min(values.len()));
            ((prefix[b] - prefix[a]) / (b - a) as f64) as f32
        })
        .collect()
}

/// Onset strength per 10 ms frame from per-frame levels: the rise (dB, clipped
/// to [`MAX_JUMP_DB`]) of the level over the 80 ms ahead versus the 80 ms
/// behind, smoothed over 50 ms. Falling levels count as zero.
pub fn onset_strength(db: &[f32]) -> Vec<f32> {
    if db.is_empty() {
        return Vec::new();
    }
    let level = box_smooth(db, LEVEL_SMOOTH_FRAMES);
    let last = level.len() - 1;
    let rise: Vec<f32> = (0..level.len())
        .map(|i| {
            let jump = level[(i + EDGE_FRAMES).min(last)] - level[i.saturating_sub(EDGE_FRAMES)];
            jump.clamp(0.0, MAX_JUMP_DB)
        })
        .collect();
    box_smooth(&rise, ONSET_SMOOTH_FRAMES)
}

/// Decode audio stream `audio_track` (0 = first) of `media` and return its
/// onset strength per 10 ms frame.
pub async fn detect(ffmpeg: &Path, media: &Path, audio_track: u32) -> Result<Vec<f32>> {
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
    Ok(onset_strength(&db))
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
    fn onsets_peak_where_the_level_jumps_up() {
        // Quiet, a line from frame 300 to 500, quiet again, another from 700.
        let mut db = vec![-60.0f32; 1000];
        db[300..500].fill(-20.0);
        db[700..].fill(-25.0);
        let strength = onset_strength(&db);
        let peak = |from: usize, to: usize| {
            (from..to)
                .max_by(|&a, &b| strength[a].total_cmp(&strength[b]))
                .unwrap()
        };
        assert!(peak(250, 350).abs_diff(300) <= 2, "{}", peak(250, 350));
        assert!(peak(650, 750).abs_diff(700) <= 2, "{}", peak(650, 750));
        // The 40 dB jump is clipped, and neither a steady level nor the drop at 500 counts.
        assert!(strength[300] <= MAX_JUMP_DB + 1e-3);
        assert!(strength[100] < 1e-3 && strength[400] < 1e-3);
        assert!(strength[500] < 1e-3);
    }

    #[test]
    fn flat_or_empty_audio_has_no_onsets() {
        assert!(onset_strength(&[]).is_empty());
        assert!(onset_strength(&vec![-30.0; 500]).iter().all(|&s| s < 1e-3));
    }

    /// Real ffmpeg decode of a generated WAV: tones where "speech" is, near
    /// silence elsewhere. Needs ffmpeg — run explicitly: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn e2e_finds_the_start_of_each_tone_burst_in_a_wav() {
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
        let strength = detect(&ffmpeg, &wav, 0).await.unwrap();
        assert!(strength.len().abs_diff(1000) <= 2, "{}", strength.len());
        for &(start, _) in &bursts {
            let at = (start * 100.0) as usize;
            let top = (at - 10..at + 10)
                .max_by(|&a, &b| strength[a].total_cmp(&strength[b]))
                .unwrap();
            // The strongest frame is within 60 ms of the true start, and strong.
            assert!(top.abs_diff(at) <= 6, "burst at {start}s: peak at {top}");
            assert!(strength[top] > 10.0, "burst at {start}s: {}", strength[top]);
        }
        // Silence between the bursts stays quiet.
        assert!(strength[300..380].iter().all(|&s| s < 1.0));
        // A missing audio track is an error that names the track.
        let err = detect(&ffmpeg, &wav, 3).await.unwrap_err().to_string();
        assert!(err.contains("audio track 3"), "{err}");
    }
}
