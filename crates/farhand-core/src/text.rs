//! Text shaping for tool results: bounded output and numbered line windows.

/// Keep the head and tail of `bytes` within `max`, marking the cut. Output
/// is lossy UTF-8; the model sees text, never raw bytes.
pub fn bounded(bytes: &[u8], max: usize) -> (String, bool) {
    if bytes.len() <= max {
        return (String::from_utf8_lossy(bytes).into_owned(), false);
    }
    let head_len = max * 2 / 3;
    let tail_len = max - head_len;
    let head = String::from_utf8_lossy(&bytes[..head_len]);
    let tail = String::from_utf8_lossy(&bytes[bytes.len() - tail_len..]);
    let omitted = bytes.len() - head_len - tail_len;
    (
        format!(
            "{head}\n\n[... {omitted} bytes omitted; output limit is {max} bytes ...]\n\n{tail}"
        ),
        true,
    )
}

/// A window of numbered lines, `cat -n` style, starting at 1-based `offset`
/// and at most `limit` lines long. Returns the text and whether more lines
/// follow the window.
pub fn numbered_window(text: &str, offset: usize, limit: usize) -> (String, usize, bool) {
    let start = offset.max(1) - 1;
    let total = text.lines().count();
    let mut out = String::new();
    let mut shown = 0;
    for (i, line) in text.lines().enumerate().skip(start).take(limit) {
        out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
        shown += 1;
    }
    let more = start + shown < total;
    (out, total, more)
}

/// Escape a string as a single-quoted POSIX shell word.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_keeps_head_and_tail() {
        let data: Vec<u8> = (0..1000).map(|i| b'a' + (i % 26) as u8).collect();
        let (s, cut) = bounded(&data, 300);
        assert!(cut);
        assert!(s.starts_with("abcdefghij"));
        assert!(s.contains("omitted"));
        assert!(s.ends_with(std::str::from_utf8(&data[900..]).unwrap()));
        let (s, cut) = bounded(b"short", 300);
        assert!(!cut);
        assert_eq!(s, "short");
    }

    #[test]
    fn window_numbers_from_offset() {
        let (s, total, more) = numbered_window("a\nb\nc\nd", 2, 2);
        assert_eq!(s, "     2\tb\n     3\tc\n");
        assert_eq!(total, 4);
        assert!(more);
        let (_, _, more) = numbered_window("a\nb", 1, 10);
        assert!(!more);
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("abc/def.txt"), "abc/def.txt");
        assert_eq!(shell_quote("it's here"), "'it'\\''s here'");
        assert_eq!(shell_quote(""), "''");
    }
}
