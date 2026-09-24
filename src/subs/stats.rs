use super::{Event, plaintext};
use serde::Serialize;

/// One line over the CPS limit (1-based event number).
#[derive(Debug, Clone, Serialize)]
pub struct LineIssue {
    pub index: usize,
    pub start_ms: i64,
    pub end_ms: i64,
    pub cps: f64,
}

/// QC numbers for one subtitle file.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FileStats {
    pub events: usize,
    pub min_dur_ms: i64,
    pub max_dur_ms: i64,
    pub avg_cps: f64,
    pub max_cps: f64,
    pub too_fast: Vec<LineIssue>,
    /// Overlapping pairs as 1-based (earlier, later) event numbers.
    pub overlaps: Vec<(usize, usize)>,
}

/// Analyze timing QC: durations, reading speed (chars/sec on visible text),
/// too-fast lines and overlaps. Zero-duration lines score finite but huge CPS
/// (duration floored at 1ms) so they always get flagged, JSON stays valid.
pub fn analyze(events: &[Event], cps_limit: f64) -> FileStats {
    let mut stats = FileStats {
        events: events.len(),
        ..Default::default()
    };
    if events.is_empty() {
        return stats;
    }
    let mut min_d = i64::MAX;
    let mut max_d = 0i64;
    let mut sum_cps = 0.0;
    for (i, e) in events.iter().enumerate() {
        let dur = (e.end_ms - e.start_ms).max(0);
        min_d = min_d.min(dur);
        max_d = max_d.max(dur);
        let chars = plaintext(&e.text).chars().count() as f64;
        let cps = chars * 1000.0 / (dur.max(1) as f64);
        sum_cps += cps;
        stats.max_cps = stats.max_cps.max(cps);
        if cps > cps_limit {
            stats.too_fast.push(LineIssue {
                index: i + 1,
                start_ms: e.start_ms,
                end_ms: e.end_ms,
                cps,
            });
        }
        if i + 1 < events.len() && events[i + 1].start_ms < e.end_ms {
            stats.overlaps.push((i + 1, i + 2));
        }
    }
    stats.min_dur_ms = min_d;
    stats.max_dur_ms = max_d;
    stats.avg_cps = sum_cps / events.len() as f64;
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(start: i64, end: i64, text: &str) -> Event {
        Event {
            start_ms: start,
            end_ms: end,
            text: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn flags_fast_and_overlapping_lines() {
        // 20 chars in 1s = 20 CPS (ok at limit 20); 30 chars in 1s = 30 (flag).
        let events = vec![
            ev(0, 1000, "12345678901234567890"),
            ev(900, 1900, "123456789012345678901234567890"),
            ev(2000, 4000, "ok"),
        ];
        let s = analyze(&events, 20.0);
        assert_eq!(s.events, 3);
        assert_eq!(s.min_dur_ms, 1000);
        assert_eq!(s.max_dur_ms, 2000);
        assert_eq!(s.too_fast.len(), 1);
        assert_eq!(s.too_fast[0].index, 2);
        assert_eq!(s.overlaps, [(1, 2)]);
    }

    #[test]
    fn handles_empty_input() {
        let s = analyze(&[], 20.0);
        assert_eq!(s.events, 0);
        assert!(s.too_fast.is_empty());
    }
}
