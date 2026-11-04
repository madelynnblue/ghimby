use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),
    Str(String),
    Int(i64),
    /// Seconds.
    Duration(i64),
    LParen,
    RParen,
    LBrack,
    RBrack,
    Comma,
    Colon,
    Op(&'static str),
}

/// A token and its byte offset in the line.
pub type Spanned = (Tok, usize);

/// Returns the tokens and the byte offset where code ends (start of a comment, or the line length).
pub fn lex(line: &str) -> Result<(Vec<Spanned>, usize)> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        let start = i;
        match c {
            ' ' | '\t' | '\r' => i += 1,
            '#' => return Ok((out, i)),
            '(' | ')' | '[' | ']' | ',' | ':' => {
                out.push((
                    match c {
                        '(' => Tok::LParen,
                        ')' => Tok::RParen,
                        '[' => Tok::LBrack,
                        ']' => Tok::RBrack,
                        ',' => Tok::Comma,
                        _ => Tok::Colon,
                    },
                    start,
                ));
                i += 1;
            }
            '=' | '!' | '<' | '>' => {
                let two = line.get(i..i + 2).unwrap_or("");
                let op = match two {
                    "==" => "==",
                    "!=" => "!=",
                    "<=" => "<=",
                    ">=" => ">=",
                    _ => match c {
                        '<' => "<",
                        '>' => ">",
                        _ => bail!("unexpected '{c}' at column {}", i + 1),
                    },
                };
                i += op.len();
                out.push((Tok::Op(op), start));
            }
            '"' => {
                let mut s = String::new();
                i += 1;
                loop {
                    let Some(&ch) = b.get(i) else {
                        bail!("unterminated string at column {}", start + 1);
                    };
                    match ch {
                        b'"' => {
                            i += 1;
                            break;
                        }
                        b'\\' => {
                            match b.get(i + 1) {
                                Some(b'"') => s.push('"'),
                                Some(b'\\') => s.push('\\'),
                                _ => bail!("bad escape at column {}", i + 1),
                            }
                            i += 2;
                        }
                        _ => {
                            let ch = line[i..].chars().next().unwrap();
                            s.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                }
                out.push((Tok::Str(s), start));
            }
            '0'..='9' | '-' => {
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                let n: i64 = line[start..i]
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad number at column {}", start + 1))?;
                let unit = match b.get(i) {
                    Some(b's') => Some(1),
                    Some(b'm') => Some(60),
                    Some(b'h') => Some(3600),
                    Some(b'd') => Some(86400),
                    Some(b'w') => Some(7 * 86400),
                    _ => None,
                };
                match unit {
                    Some(u) => {
                        i += 1;
                        out.push((Tok::Duration(n * u), start));
                    }
                    None => out.push((Tok::Int(n), start)),
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push((Tok::Ident(line[start..i].to_string()), start));
            }
            _ => bail!("unexpected '{c}' at column {}", i + 1),
        }
    }
    Ok((out, line.len()))
}
