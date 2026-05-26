//! Tokenizer for the .sflow DSL.
//!
//! The grammar is line-oriented but the lexer keeps it permissive — whitespace
//! between tokens is collapsed, line breaks are emitted as [`Token::Newline`]
//! markers, and comments (`// …`) are stripped. The parser uses Newline tokens
//! to terminate statements; it does not need columns.
//!
//! String literals support `${var}` interpolation. The lexer does NOT split
//! interpolation out at this layer — the literal body is kept verbatim and the
//! executor performs substitution at run time. This keeps the parser dumb and
//! the substitution model identical to other interpolation sites.

use super::SflowError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token {
    // Keywords
    Step,
    Set,
    If,
    Elif,
    Else,
    End,
    Loop,
    Times,
    While,
    Until,
    /// `for <ident> in <value>` — list iteration. Distinct
    /// from `loop N times` because the loop variable is bound
    /// to the element at each iteration, not to the index.
    For,
    /// `in` — keyword between `for <ident>` and the iterable
    /// expression. Only valid inside a `for` header.
    In,
    Try,
    Catch,
    Rethrow,
    Return,
    And,
    Or,
    Not,
    Contains,
    Matches,
    Exists,
    True,
    False,

    /// Bare identifier (not a keyword). Includes dotted names like
    /// `peer.method`, `var.name`, `step.name.field` — the lexer keeps these as
    /// a single token and the parser splits them by `.` when needed.
    Ident(String),
    /// Integer literal (only used as `loop N times`'s N; otherwise treated
    /// as part of an identifier or a string).
    Integer(i64),
    /// Double-quoted string literal with the surrounding quotes stripped.
    /// The body is kept verbatim — `${var}` substitution happens at run time.
    String(String),

    // Punctuation
    Colon,
    Eq,
    EqEq,
    BangEq,
    /// `[` — opens a list literal in a value-expression
    /// position. Statement positions never expect a `[`, so
    /// the parser only consumes one when it's reading a value.
    LSquare,
    /// `]` — closes a list literal.
    RSquare,
    /// `{` — opens a map literal in a value-expression
    /// position. Same statement-vs-value rule as LSquare.
    LCurly,
    /// `}` — closes a map literal.
    RCurly,
    /// `(` — opens a built-in function call argument list
    /// (`list_len(...)`, `map_get(...)`, etc.). Only valid in
    /// a value-expression position; statement keywords never
    /// take parentheses.
    LParen,
    /// `)` — closes a built-in function call argument list.
    RParen,
    /// `,` — separates list elements, map pairs, and function
    /// call arguments. Plain commas anywhere else are a parse
    /// error (Sflow has no tuples / multi-arg statements).
    Comma,

    /// End of a line that contained at least one non-whitespace, non-comment
    /// token. Consecutive blank lines collapse into a single Newline so the
    /// parser doesn't have to skip them.
    Newline,
}

#[derive(Clone, Debug)]
pub struct Lexed {
    pub token: Token,
    /// 1-indexed line of the token's first character.
    pub line: usize,
}

pub fn tokenize(source: &str) -> Result<Vec<Lexed>, SflowError> {
    let mut out: Vec<Lexed> = Vec::new();
    let mut line = 1usize;
    let chars: Vec<char> = source.chars().collect();
    let mut i = 0usize;
    let mut at_line_start = true;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' => {
                i += 1;
            }
            '\n' => {
                if !at_line_start
                    && !out
                        .last()
                        .map(|t| matches!(t.token, Token::Newline))
                        .unwrap_or(true)
                {
                    out.push(Lexed {
                        token: Token::Newline,
                        line,
                    });
                }
                line += 1;
                at_line_start = true;
                i += 1;
            }
            '/' if i + 1 < chars.len() && chars[i + 1] == '/' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '"' => {
                let start_line = line;
                i += 1;
                let mut buf = String::new();
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\n' {
                        return Err(SflowError::new(
                            start_line,
                            "unterminated string literal (newline before closing quote)",
                        ));
                    }
                    buf.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() {
                    return Err(SflowError::new(
                        start_line,
                        "unterminated string literal (end of file)",
                    ));
                }
                i += 1; // closing quote
                out.push(Lexed {
                    token: Token::String(buf),
                    line: start_line,
                });
                at_line_start = false;
            }
            ':' => {
                out.push(Lexed {
                    token: Token::Colon,
                    line,
                });
                i += 1;
                at_line_start = false;
            }
            '[' => {
                out.push(Lexed {
                    token: Token::LSquare,
                    line,
                });
                i += 1;
                at_line_start = false;
            }
            ']' => {
                out.push(Lexed {
                    token: Token::RSquare,
                    line,
                });
                i += 1;
                at_line_start = false;
            }
            '{' => {
                out.push(Lexed {
                    token: Token::LCurly,
                    line,
                });
                i += 1;
                at_line_start = false;
            }
            '}' => {
                out.push(Lexed {
                    token: Token::RCurly,
                    line,
                });
                i += 1;
                at_line_start = false;
            }
            '(' => {
                out.push(Lexed {
                    token: Token::LParen,
                    line,
                });
                i += 1;
                at_line_start = false;
            }
            ')' => {
                out.push(Lexed {
                    token: Token::RParen,
                    line,
                });
                i += 1;
                at_line_start = false;
            }
            ',' => {
                out.push(Lexed {
                    token: Token::Comma,
                    line,
                });
                i += 1;
                at_line_start = false;
            }
            '=' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    out.push(Lexed {
                        token: Token::EqEq,
                        line,
                    });
                    i += 2;
                } else {
                    out.push(Lexed {
                        token: Token::Eq,
                        line,
                    });
                    i += 1;
                }
                at_line_start = false;
            }
            '!' if i + 1 < chars.len() && chars[i + 1] == '=' => {
                out.push(Lexed {
                    token: Token::BangEq,
                    line,
                });
                i += 2;
                at_line_start = false;
            }
            c if c.is_ascii_digit()
                || (c == '-' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) =>
            {
                let start_line = line;
                let start = i;
                if c == '-' {
                    i += 1;
                }
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                let v: i64 = text.parse().map_err(|e| {
                    SflowError::new(start_line, format!("bad integer literal `{text}`: {e}"))
                })?;
                out.push(Lexed {
                    token: Token::Integer(v),
                    line: start_line,
                });
                at_line_start = false;
            }
            c if is_ident_start(c) => {
                let start_line = line;
                let start = i;
                while i < chars.len() && is_ident_cont(chars[i]) {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                let token = match text.as_str() {
                    "step" => Token::Step,
                    "set" => Token::Set,
                    "if" => Token::If,
                    "elif" => Token::Elif,
                    "else" => Token::Else,
                    "end" => Token::End,
                    "loop" => Token::Loop,
                    "times" => Token::Times,
                    "while" => Token::While,
                    "until" => Token::Until,
                    "for" => Token::For,
                    "in" => Token::In,
                    "try" => Token::Try,
                    "catch" => Token::Catch,
                    "rethrow" => Token::Rethrow,
                    "return" => Token::Return,
                    "and" => Token::And,
                    "or" => Token::Or,
                    "not" => Token::Not,
                    "contains" => Token::Contains,
                    "matches" => Token::Matches,
                    "exists" => Token::Exists,
                    "true" => Token::True,
                    "false" => Token::False,
                    _ => Token::Ident(text),
                };
                out.push(Lexed {
                    token,
                    line: start_line,
                });
                at_line_start = false;
            }
            other => {
                return Err(SflowError::new(
                    line,
                    format!("unexpected character `{other}`"),
                ));
            }
        }
    }
    // Drop trailing Newline if any, then push a sentinel so the parser knows
    // the final statement terminates cleanly.
    while matches!(out.last().map(|t| &t.token), Some(Token::Newline)) {
        out.pop();
    }
    out.push(Lexed {
        token: Token::Newline,
        line,
    });
    Ok(out)
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_cont(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Token> {
        tokenize(s).unwrap().into_iter().map(|t| t.token).collect()
    }

    #[test]
    fn step_keyword_tokenises() {
        let t = toks("step reply: ai.chat \"hi\"\n");
        assert!(matches!(t[0], Token::Step));
        assert!(matches!(t[1], Token::Ident(ref s) if s == "reply"));
        assert!(matches!(t[2], Token::Colon));
        assert!(matches!(t[3], Token::Ident(ref s) if s == "ai.chat"));
        assert!(matches!(t[4], Token::String(ref s) if s == "hi"));
    }

    #[test]
    fn if_elif_else_end_tokenise() {
        let t = toks("if x\nelif y\nelse\nend\n");
        let kinds: Vec<&str> = t
            .iter()
            .filter_map(|tok| match tok {
                Token::If => Some("if"),
                Token::Elif => Some("elif"),
                Token::Else => Some("else"),
                Token::End => Some("end"),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, ["if", "elif", "else", "end"]);
    }

    #[test]
    fn loop_while_until_tokenise() {
        let t = toks("loop 3 times\nwhile cond\nuntil cond\n");
        let kinds: Vec<&str> = t
            .iter()
            .filter_map(|tok| match tok {
                Token::Loop => Some("loop"),
                Token::Times => Some("times"),
                Token::While => Some("while"),
                Token::Until => Some("until"),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, ["loop", "times", "while", "until"]);
    }

    #[test]
    fn try_catch_rethrow_tokenise() {
        let t = toks("try\ncatch any\nrethrow\nend\n");
        let kinds: Vec<&str> = t
            .iter()
            .filter_map(|tok| match tok {
                Token::Try => Some("try"),
                Token::Catch => Some("catch"),
                Token::Rethrow => Some("rethrow"),
                Token::End => Some("end"),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, ["try", "catch", "rethrow", "end"]);
    }

    #[test]
    fn set_and_return_tokenise() {
        let t = toks("set x = \"y\"\nreturn var.x\n");
        assert!(matches!(t[0], Token::Set));
        assert!(matches!(t[1], Token::Ident(ref s) if s == "x"));
        assert!(matches!(t[2], Token::Eq));
        assert!(matches!(t[3], Token::String(_)));
        // After Newline:
        let after_nl: usize = t
            .iter()
            .position(|tok| matches!(tok, Token::Return))
            .unwrap();
        assert!(matches!(t[after_nl + 1], Token::Ident(ref s) if s == "var.x"));
    }

    #[test]
    fn string_with_interpolation_kept_verbatim() {
        let t = toks("sol.log \"iter ${loop.iter}\"\n");
        let lit = t
            .iter()
            .find_map(|tok| match tok {
                Token::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap();
        // Body is kept as-is; ${...} substitution is the executor's job.
        assert_eq!(lit, "iter ${loop.iter}");
    }

    #[test]
    fn unterminated_string_rejected() {
        let err = tokenize("\"oops\n").unwrap_err();
        assert!(err.message.contains("unterminated"));
        assert_eq!(err.line, 1);
    }

    #[test]
    fn comments_stripped() {
        let t = toks("// header\nset x = \"1\" // trailing\nreturn\n");
        assert!(matches!(t[0], Token::Set));
        assert!(t.iter().any(|tok| matches!(tok, Token::Return)));
    }

    #[test]
    fn unexpected_char_rejected_with_line() {
        let err = tokenize("set x = $\n").unwrap_err();
        assert!(err.message.contains("unexpected character"));
        assert_eq!(err.line, 1);
    }
}
