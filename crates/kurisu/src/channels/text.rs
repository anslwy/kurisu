//! The existing Telegram scanner, shared without changing its behavior.
fn clen(s: &str) -> usize {
    s.chars().count()
}
/// Safe whitespace boundaries outside code spans, fences and Markdown targets.
/// Non-whitespace runs (including URLs and words) are always atomic. Byte
/// offsets let both the splitter and streaming relay preserve Unicode exactly.
pub(crate) fn text_boundaries(text: &str) -> Vec<(usize, u8)> {
    let mut boundaries = Vec::new();
    let mut code: Option<(char, usize)> = None;
    let mut target_depth = 0usize;
    let mut i = 0;
    while i < text.len() {
        let c = text[i..].chars().next().unwrap();
        if c == '\\' {
            i += c.len_utf8();
            if let Some(next) = text[i..].chars().next() {
                i += next.len_utf8();
            }
            continue;
        }
        if c == '`' || c == '~' {
            let run = text[i..].chars().take_while(|&x| x == c).count();
            if code == Some((c, run)) {
                code = None;
            } else if code.is_none() && (c == '`' || run >= 3) {
                code = Some((c, run));
            }
            i += run;
            continue;
        }
        if code.is_none() {
            if c == '(' && (target_depth > 0 || text[..i].ends_with(']')) {
                target_depth += 1;
            } else if c == ')' && target_depth > 0 {
                target_depth -= 1;
            } else if c.is_whitespace() && target_depth == 0 {
                let rank = if text[i..].starts_with("\n\n") {
                    3
                } else if c == '\n' {
                    2
                } else {
                    1
                };
                boundaries.push((i, rank));
            }
        }
        i += c.len_utf8();
    }
    boundaries
}

/// Split only between atomic tokens. An oversized token is sent alone rather
/// than corrupting it; the transport logs this exception to the normal cap.
pub fn split_text(text: &str, limit: usize) -> Vec<String> {
    let mut rest = text.trim();
    let mut chunks = Vec::new();
    while !rest.is_empty() {
        if clen(rest) <= limit {
            chunks.push(rest.to_string());
            break;
        }
        let boundaries = text_boundaries(rest);
        let cap = rest
            .char_indices()
            .nth(limit)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let at = boundaries
            .iter()
            .filter(|(i, _)| *i > 0 && *i <= cap)
            .max_by_key(|(i, rank)| (*rank, *i))
            .map(|(i, _)| *i)
            .or_else(|| boundaries.iter().find(|(i, _)| *i > 0).map(|(i, _)| *i))
            .unwrap_or(rest.len());
        let chunk = rest[..at].trim_end();
        if !chunk.is_empty() {
            chunks.push(chunk.to_string());
        }
        rest = rest[at..].trim_start();
    }
    chunks
}

#[derive(Debug, PartialEq)]
pub enum Chunk {
    Text(String),
    File(String),
}
/// Protect entire Discord Markdown links, including labels with whitespace.
pub fn discord_boundaries(text: &str) -> Vec<(usize, u8)> {
    let mut spans = Vec::new();
    for (at, _) in text.match_indices("](") {
        let Some(start) = text[..at].rfind('[') else {
            continue;
        };
        let mut depth = 1;
        let mut end = text.len();
        let mut escaped = false;
        for (offset, c) in text[at + 2..].char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if c == '\\' {
                escaped = true;
                continue;
            }
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth -= 1;
                if depth == 0 {
                    end = at + 2 + offset + 1;
                    break;
                }
            }
        }
        spans.push((start, end));
    }
    text_boundaries(text)
        .into_iter()
        .filter(|(at, _)| !spans.iter().any(|(a, b)| at > a && at < b))
        .collect()
}
/// Conservative UTF-16 budget also handles astral emoji without oversize sends.
pub const MAX_DISCORD_UTF16: usize = 2000;

pub fn discord(text: &str) -> Vec<Chunk> {
    let mut rest = text.trim();
    let mut chunks = Vec::new();
    while !rest.is_empty() {
        if rest.encode_utf16().count() <= MAX_DISCORD_UTF16 {
            chunks.push(Chunk::Text(rest.into()));
            break;
        }
        let mut units = 0;
        let cap = rest
            .char_indices()
            .find_map(|(i, c)| {
                units += c.len_utf16();
                (units > MAX_DISCORD_UTF16).then_some(i)
            })
            .unwrap_or(rest.len());
        let boundaries = discord_boundaries(rest);
        let at = boundaries
            .iter()
            .filter(|(i, _)| *i > 0 && *i <= cap)
            .max_by_key(|(i, rank)| (*rank, *i))
            .map(|(i, _)| *i)
            .or_else(|| boundaries.iter().find(|(i, _)| *i > 0).map(|(i, _)| *i))
            .unwrap_or(rest.len());
        let piece = rest[..at].trim_end();
        if !piece.is_empty() {
            chunks.push(if piece.encode_utf16().count() > MAX_DISCORD_UTF16 {
                Chunk::File(piece.into())
            } else {
                Chunk::Text(piece.into())
            });
        }
        rest = rest[at..].trim_start();
    }
    chunks
}
