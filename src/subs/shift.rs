use super::{Event, time};

/// Shift every event by `shift_ms` (positive delays, negative advances).
/// Negative results clamp to zero and inverted ranges collapse.
pub fn shift_events(events: &mut [Event], shift_ms: i32) {
    for e in events {
        e.start_ms = time::clamp_non_negative(e.start_ms + i64::from(shift_ms));
        e.end_ms = time::clamp_non_negative(e.end_ms + i64::from(shift_ms));
        if e.end_ms < e.start_ms {
            e.end_ms = e.start_ms;
        }
    }
}

/// Drop events without visible text. With `use_plaintext` (default) events
/// holding only formatting tags count as empty; otherwise only truly
/// text-less events are dropped. Returns the kept events plus drop count.
pub fn drop_empty(events: Vec<Event>, use_plaintext: bool) -> (Vec<Event>, usize) {
    let before = events.len();
    let kept = events
        .into_iter()
        .filter(|e| {
            let t = if use_plaintext {
                super::plaintext(&e.text)
            } else {
                e.text.clone()
            };
            !t.trim().is_empty()
        })
        .collect::<Vec<_>>();
    let removed = before - kept.len();
    (kept, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(start: i64, end: i64, text: &str) -> Event {
        Event {
            start_ms: start,
            end_ms: end,
            text: text.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn shifts_and_clamps() {
        let mut events = vec![event(2000, 3000, "a"), event(500, 800, "b")];
        shift_events(&mut events, -1500);
        assert_eq!((events[0].start_ms, events[0].end_ms), (500, 1500));
        // Clamped to zero, range collapsed.
        assert_eq!((events[1].start_ms, events[1].end_ms), (0, 0));
    }

    #[test]
    fn drops_empty_plaintext() {
        let events = vec![
            event(0, 1000, "Hello"),
            event(0, 1000, ""),
            event(0, 1000, "{\\i1}{\\i0}"),
            event(0, 1000, "\\N"),
        ];
        let (kept, removed) = drop_empty(events, true);
        assert_eq!(kept.len(), 1);
        assert_eq!(removed, 3);
    }

    #[test]
    fn keeps_tag_only_when_asked() {
        let events = vec![event(0, 1000, "{\\i1}{\\i0}"), event(0, 1000, "")];
        let (kept, removed) = drop_empty(events, false);
        assert_eq!(kept.len(), 1);
        assert_eq!(removed, 1);
    }
}
