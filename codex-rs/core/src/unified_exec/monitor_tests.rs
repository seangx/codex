use pretty_assertions::assert_eq;

use super::*;

#[test]
fn decoder_joins_split_lines_and_trims_crlf() {
    let mut decoder = LineDecoder::default();
    assert!(decoder.push_chunk(b"first").is_empty());
    let lines = decoder.push_chunk(b" line\r\nsecond\n");
    assert_eq!(
        lines.into_iter().map(|line| line.text).collect::<Vec<_>>(),
        vec!["first line".to_string(), "second".to_string()]
    );
}

#[test]
fn decoder_caps_a_single_line_until_the_next_newline() {
    let mut decoder = LineDecoder::default();
    let mut input = vec![b'x'; MONITOR_MAX_LINE_BYTES + 100];
    input.extend_from_slice(b"\nnext\n");
    let lines = decoder.push_chunk(&input);
    assert_eq!(lines.len(), 2);
    assert!(lines[0].truncated);
    assert!(lines[0].text.ends_with(" …[line truncated]"));
    assert_eq!(lines[1].text, "next");
}

#[test]
fn batch_caps_lines_and_reports_omissions() {
    let mut batch = MonitorBatch::default();
    for index in 0..=MONITOR_MAX_BATCH_LINES {
        batch.push(DecodedLine {
            text: format!("line-{index}"),
            truncated: false,
        });
    }
    assert_eq!(batch.lines.len(), MONITOR_MAX_BATCH_LINES);
    assert_eq!(batch.omitted_lines, 1);
}
