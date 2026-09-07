use alloc::string::String;
use alloc::vec::Vec;

/// Split a command line into tokens, with bash-like support for
/// single quotes (`'...'`, literal), double quotes (`"..."`, with `\"` and
/// `\\` escapes) and backslash escapes outside quotes.
pub(crate) fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut chars = line.chars();

    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if started {
                    tokens.push(core::mem::take(&mut cur));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        None | Some('\'') => break,
                        Some(c) => cur.push(c),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        None | Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(n @ ('"' | '\\' | '$' | '`')) => cur.push(n),
                            Some(n) => {
                                cur.push('\\');
                                cur.push(n);
                            }
                            None => break,
                        },
                        Some(c) => cur.push(c),
                    }
                }
            }
            '\\' => {
                started = true;
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c => {
                started = true;
                cur.push(c);
            }
        }
    }
    if started {
        tokens.push(cur);
    }
    tokens
}

/// Longest common prefix of a list of strings (char-boundary safe).
pub(crate) fn common_prefix<'a>(items: &[&'a str]) -> &'a str {
    let Some((&first, rest)) = items.split_first() else {
        return "";
    };
    let mut end = first.len();
    for item in rest {
        let mut n = 0;
        for (_, b) in first.char_indices() {
            if !item[n..].starts_with(b) {
                break;
            }
            n += b.len_utf8();
        }
        end = end.min(n);
    }
    // `n` only ever advances in whole characters, but be defensive anyway.
    while end > 0 && !first.is_char_boundary(end) {
        end -= 1;
    }
    &first[..end]
}

/// Previous character boundary before `i` (`i` must be > 0).
pub(crate) fn prev_char_boundary(s: &str, i: usize) -> usize {
    let mut i = i - 1;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Next character boundary at or after `i`.
pub(crate) fn next_char_boundary(s: &str, i: usize) -> usize {
    let mut i = i;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_plain() {
        assert_eq!(tokenize("  hello   world  "), ["hello", "world"]);
    }

    #[test]
    fn tokenize_quotes() {
        assert_eq!(
            tokenize(r#"a "b c" 'd e' f\ g"#),
            ["a", "b c", "d e", "f g"]
        );
        assert_eq!(tokenize(r#""esc \" inside""#), ["esc \" inside"]);
        assert_eq!(tokenize(r#"'no \esc' x"#), ["no \\esc", "x"]);
    }

    #[test]
    fn tokenize_empty_quoted_token() {
        assert_eq!(tokenize(r#"" " a"#), [" ", "a"]);
        assert_eq!(tokenize("'' b"), ["", "b"]);
    }

    #[test]
    fn common_prefix_basic() {
        assert_eq!(common_prefix(&["led", "len", "ler"]), "le");
        assert_eq!(common_prefix(&["led"]), "led");
        let empty: &[&str] = &[];
        assert_eq!(common_prefix(empty), "");
        assert_eq!(common_prefix(&["abc", "xyz"]), "");
        assert_eq!(common_prefix(&["привет", "прикол"]), "при");
    }
}
