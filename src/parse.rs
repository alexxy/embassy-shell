use alloc::string::String;
use alloc::vec::Vec;

/// Make sure slot `i` exists in `tokens` and is empty, returning nothing:
/// pushes a new `String` if `i == tokens.len()`, otherwise clears the
/// existing entry so its heap allocation is reused.
fn reset_slot(tokens: &mut Vec<String>, i: usize) {
    if i == tokens.len() {
        tokens.push(String::new());
    } else {
        tokens[i].clear();
    }
}

/// Split a command line into tokens, with bash-like support for
/// single quotes (`'...'`, literal), double quotes (`"..."`, with `\"` and
/// `\\` escapes) and backslash escapes outside quotes.
///
/// Tokens are written into a caller-owned `Vec<String>` whose existing slots
/// are reused across calls (each slot is cleared only when a new token
/// starts); the return value is the token count, so callers use
/// `&tokens[..n]` and the vector is never shrunk. Repeated calls on a
/// long-lived vector therefore stop allocating once the largest line seen
/// so far has been processed.
pub(crate) fn tokenize_into(line: &str, tokens: &mut Vec<String>) -> usize {
    let mut i: usize = 0;
    let mut started = false;
    let mut chars = line.chars();

    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if started {
                    i += 1;
                    started = false;
                }
            }
            '\'' => {
                if !started {
                    reset_slot(tokens, i);
                    started = true;
                }
                loop {
                    match chars.next() {
                        None | Some('\'') => break,
                        Some(c) => tokens[i].push(c),
                    }
                }
            }
            '"' => {
                if !started {
                    reset_slot(tokens, i);
                    started = true;
                }
                loop {
                    match chars.next() {
                        None | Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(n @ ('"' | '\\' | '$' | '`')) => tokens[i].push(n),
                            Some(n) => {
                                tokens[i].push('\\');
                                tokens[i].push(n);
                            }
                            None => break,
                        },
                        Some(c) => tokens[i].push(c),
                    }
                }
            }
            '\\' => {
                if !started {
                    reset_slot(tokens, i);
                    started = true;
                }
                if let Some(n) = chars.next() {
                    tokens[i].push(n);
                }
            }
            c => {
                if !started {
                    reset_slot(tokens, i);
                    started = true;
                }
                tokens[i].push(c);
            }
        }
    }
    if started { i + 1 } else { i }
}

/// Allocating convenience wrapper around [`tokenize_into`], for tests.
#[cfg(test)]
pub(crate) fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let n = tokenize_into(line, &mut tokens);
    tokens.truncate(n);
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
    fn tokenize_into_matches_tokenize() {
        let cases = [
            "  hello   world  ",
            r#"a "b c" 'd e' f\ g"#,
            r#"" " a"#,
            "'' b",
            "",
            "   ",
            "one",
        ];
        let mut reused = Vec::new();
        for line in cases {
            let n = tokenize_into(line, &mut reused);
            assert_eq!(&reused[..n], &tokenize(line)[..], "line: {line:?}");
        }
    }

    #[test]
    fn tokenize_into_reuses_slots() {
        let mut tokens = Vec::new();
        assert_eq!(tokenize_into("alpha beta gamma", &mut tokens), 3);
        assert_eq!(&tokens[..3], ["alpha", "beta", "gamma"]);
        let ptr = tokens[0].as_str().as_ptr();

        // Fewer, shorter tokens reuse the existing allocations.
        assert_eq!(tokenize_into("x y", &mut tokens), 2);
        assert_eq!(&tokens[..2], ["x", "y"]);
        assert_eq!(tokens[0].as_str().as_ptr(), ptr);

        // Growing back does not corrupt stale slots.
        assert_eq!(tokenize_into("gamma", &mut tokens), 1);
        assert_eq!(&tokens[..1], ["gamma"]);
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
