/// Subtitle time utils: parse/format ASS, SRT and VTT timestamps.
///
/// Contract:
/// - all internal times are milliseconds (i64, may be negative before clamping)
/// - `clamp_non_negative()` keeps shifted times inside the valid range
pub fn clamp_non_negative(ms: i64) -> i64 {
    ms.max(0)
}

fn parse_hms(h: &str, m: &str, s: &str) -> Option<i64> {
    let h: i64 = h.trim().parse().ok()?;
    let m: i64 = m.trim().parse().ok()?;
    let s: i64 = s.trim().parse().ok()?;
    if h < 0 || !(0..60).contains(&m) || !(0..60).contains(&s) {
        return None;
    }
    Some(h * 3600 + m * 60 + s)
}

/// Parse `H:MM:SS.cc` (ASS centiseconds, 1-3 fraction digits) into milliseconds.
pub fn parse_ass_time(s: &str) -> Option<i64> {
    let (hms, frac) = s.trim().split_once('.')?;
    let mut parts = hms.split(':');
    let base = parse_hms(parts.next()?, parts.next()?, parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    let frac = frac.trim();
    if frac.is_empty() || frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let f: i64 = frac.parse().ok()?;
    Some(base * 1000 + f * 10_i64.pow(3 - frac.len() as u32))
}

/// Format milliseconds as ASS `H:MM:SS.cc`.
pub fn format_ass_time(ms: i64) -> String {
    let ms = clamp_non_negative(ms);
    let s = ms / 1000;
    format!(
        "{}:{:02}:{:02}.{:02}",
        s / 3600,
        (s / 60) % 60,
        s % 60,
        (ms % 1000) / 10
    )
}

/// Parse `HH:MM:SS,mmm` (SRT, `.` accepted as well) into milliseconds.
pub fn parse_srt_time(s: &str) -> Option<i64> {
    let s = s.trim().replace('.', ",");
    let (hms, ms) = s.split_once(',')?;
    let mut parts = hms.split(':');
    let base = parse_hms(parts.next()?, parts.next()?, parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    let ms = ms.trim();
    if ms.len() != 3 || !ms.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(base * 1000 + ms.parse::<i64>().ok()?)
}

/// Format milliseconds as SRT `HH:MM:SS,mmm`.
pub fn format_srt_time(ms: i64) -> String {
    let ms = clamp_non_negative(ms);
    let s = ms / 1000;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        s / 3600,
        (s / 60) % 60,
        s % 60,
        ms % 1000
    )
}

/// Parse `[HH:]MM:SS.mmm` (VTT) into milliseconds.
pub fn parse_vtt_time(s: &str) -> Option<i64> {
    let (hms, ms) = s.trim().split_once('.')?;
    let parts: Vec<&str> = hms.split(':').collect();
    let base = match parts.len() {
        3 => parse_hms(parts[0], parts[1], parts[2])?,
        2 => parse_hms("0", parts[0], parts[1])?,
        _ => return None,
    };
    let ms = ms.trim();
    if ms.len() != 3 || !ms.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(base * 1000 + ms.parse::<i64>().ok()?)
}

/// Format milliseconds as VTT `HH:MM:SS.mmm`.
pub fn format_vtt_time(ms: i64) -> String {
    let ms = clamp_non_negative(ms);
    let s = ms / 1000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        s / 3600,
        (s / 60) % 60,
        s % 60,
        ms % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_negative_to_zero() {
        assert_eq!(clamp_non_negative(-1500), 0);
        assert_eq!(clamp_non_negative(42), 42);
    }

    #[test]
    fn ass_roundtrip() {
        assert_eq!(parse_ass_time("0:00:01.00"), Some(1000));
        assert_eq!(parse_ass_time("1:02:03.45"), Some(3723450));
        assert_eq!(format_ass_time(3723450), "1:02:03.45");
        assert_eq!(parse_ass_time("bogus"), None);
        assert_eq!(parse_ass_time("0:00:61.00"), None);
    }

    #[test]
    fn srt_roundtrip() {
        assert_eq!(parse_srt_time("00:00:01,500"), Some(1500));
        assert_eq!(format_srt_time(1500), "00:00:01,500");
        assert_eq!(parse_srt_time("00:00:01.500"), Some(1500));
        assert_eq!(parse_srt_time("00:00:01,5"), None);
    }

    #[test]
    fn vtt_roundtrip() {
        assert_eq!(parse_vtt_time("00:01:02.500"), Some(62500));
        assert_eq!(parse_vtt_time("01:02.500"), Some(62500));
        assert_eq!(format_vtt_time(62500), "00:01:02.500");
    }
}
