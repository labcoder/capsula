use super::{MAX_CHARS, Segmenter};

/// Feeds the deltas, then finishes, and returns each segment's range and text.
fn segments(deltas: &[&str]) -> Vec<(std::ops::Range<usize>, String)> {
    let mut segmenter = Segmenter::default();
    let mut out = Vec::new();
    for delta in deltas {
        segmenter.push(delta, &mut out);
    }
    segmenter.finish(&mut out);
    out.into_iter().map(|cut| (cut.range, cut.text)).collect()
}

fn texts(deltas: &[&str]) -> Vec<String> {
    segments(deltas).into_iter().map(|(_, text)| text).collect()
}

#[test]
fn a_sentence_end_needs_whitespace_after_it() {
    assert_eq!(
        segments(&["Hi there. How are you? Fine!"]),
        [
            (0..9, "Hi there.".to_owned()),
            (9..22, " How are you?".to_owned()),
            (22..28, " Fine!".to_owned()),
        ]
    );
}

#[test]
fn a_mark_at_the_end_of_a_delta_waits_for_the_next_character() {
    assert_eq!(texts(&["Pi is 3.", "14 or so."]), ["Pi is 3.14 or so."]);
    assert_eq!(texts(&["Wait..", ". Then go."]), ["Wait...", " Then go."]);
    assert_eq!(texts(&["Why?", " Because."]), ["Why?", " Because."]);
    let mut segmenter = Segmenter::default();
    let mut out = Vec::new();
    segmenter.push("Done.", &mut out);
    assert_eq!(out.len(), 0);
    segmenter.push(" ", &mut out);
    assert_eq!(out.len(), 1);
}

#[test]
fn a_clause_ends_a_segment_only_past_120_characters() {
    let at_120 = format!("{}, rest", "a".repeat(119));
    assert_eq!(texts(&[&at_120]), [at_120.as_str()]);
    let at_121 = format!("{}, rest", "a".repeat(120));
    assert_eq!(
        texts(&[&at_121]),
        [format!("{},", "a".repeat(120)), " rest".to_owned()]
    );
    let number = format!("{}3,000 more", "a".repeat(120));
    assert_eq!(texts(&[&number]), [number.as_str()]);
}

#[test]
fn a_long_token_is_cut_at_240_characters() {
    let exact = "b".repeat(240);
    assert_eq!(segments(&[&exact]), [(0..240, exact.clone())]);
    for len in [241, 245] {
        let over = "b".repeat(len);
        assert_eq!(
            segments(&[&over]),
            [(0..240, exact.clone()), (240..len, "b".repeat(len - 240))]
        );
    }
}

#[test]
fn a_hard_cut_falls_after_the_last_whitespace() {
    let words = format!("{} {}", "c".repeat(200), "d".repeat(60));
    assert_eq!(
        texts(&[&words]),
        [format!("{} ", "c".repeat(200)), "d".repeat(60)]
    );
}

#[test]
fn cuts_stay_on_character_boundaries() {
    let wide = "é".repeat(MAX_CHARS + 1);
    let got = segments(&[&wide]);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].0, 0..MAX_CHARS * 2);
    assert_eq!(got[1].0, MAX_CHARS * 2..MAX_CHARS * 2 + 2);
    assert_eq!(got[1].1, "é");
}

#[test]
fn the_tail_is_flushed_once_and_whitespace_alone_is_never_spoken() {
    let mut segmenter = Segmenter::default();
    let mut out = Vec::new();
    segmenter.push("First. and a tail", &mut out);
    segmenter.finish(&mut out);
    segmenter.finish(&mut out);
    segmenter.push(" late. more ", &mut out);
    segmenter.finish(&mut out);
    let got: Vec<_> = out.into_iter().map(|cut| (cut.range, cut.text)).collect();
    assert_eq!(
        got,
        [
            (0..6, "First.".to_owned()),
            (6..17, " and a tail".to_owned())
        ]
    );
    assert_eq!(texts(&["Hi.  ", "", "\n"]), ["Hi."]);
    assert_eq!(texts(&["One.", "", " Two."]), ["One.", " Two."]);
    assert!(texts(&[""]).is_empty());
}

#[test]
fn a_flush_cuts_mid_sentence_and_ranges_stay_contiguous() {
    let mut segmenter = Segmenter::default();
    let mut out = Vec::new();
    segmenter.push("Look at", &mut out);
    segmenter.flush(&mut out);
    segmenter.push(" this. Then", &mut out);
    segmenter.finish(&mut out);
    let got: Vec<_> = out.into_iter().map(|cut| (cut.range, cut.text)).collect();
    assert_eq!(
        got,
        [
            (0..7, "Look at".to_owned()),
            (7..13, " this.".to_owned()),
            (13..18, " Then".to_owned()),
        ]
    );
}

#[test]
fn debug_shows_no_text() {
    let marker = "kumochi-segment-sentinel";
    let mut segmenter = Segmenter::default();
    let mut out = Vec::new();
    segmenter.push(&format!("{marker}. {marker}"), &mut out);
    let debug = format!("{segmenter:?} {out:?}");
    let bytes = format!("{:?}", marker.as_bytes());
    assert!(!debug.contains(marker), "{debug}");
    assert!(!debug.contains(&bytes[1..bytes.len() - 1]), "{debug}");
}
