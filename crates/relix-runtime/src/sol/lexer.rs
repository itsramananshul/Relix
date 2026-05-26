#[derive(Debug, Clone)]
pub enum Token {
    Ident(String),
    For,
    In,
    As,
    Func,
    If,
    Else,
    Import,
    While,
    Struct,
    Enum,
    Let,
    Return,
    True,
    False,
    Try,
    Catch,
    Rethrow,

    Integer(i128),
    Float(f64),
    Char(char),
    String(String),

    LParen,
    RParen,
    LSquare,
    RSquare,
    LCurly,
    RCurly,

    Dot,
    Comma,
    Colon,
    ColonColon,
    Semi,
    Arrow,

    Eq,
    Bang,
    BangEq,
    MoreThan,
    LessThan,
    MoreEq,
    LessEq,
    EqEq,
    Plus,
    Dash,
    Star,
    Slash,
    Ampersand,
    AmpAmp,
    Pipe,
    PipePipe,
    LShift,
    RShift,
    Caret,
    Tilde,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Ident,
    For,
    In,
    As,
    Func,
    If,
    Else,
    Import,
    While,
    Struct,
    Enum,
    Let,
    Return,
    True,
    False,
    Try,
    Catch,
    Rethrow,

    Integer,
    Float,
    Char,
    String,

    LParen,
    RParen,
    LSquare,
    RSquare,
    LCurly,
    RCurly,

    Dot,
    Comma,
    Colon,
    ColonColon,
    Semi,
    Arrow,

    Eq,
    Bang,
    BangEq,
    MoreThan,
    LessThan,
    MoreEq,
    LessEq,
    EqEq,
    Plus,
    Dash,
    Star,
    Slash,
    Ampersand,
    AmpAmp,
    Pipe,
    PipePipe,
    LShift,
    RShift,
    Caret,
    Tilde,
}
impl Token {
    pub fn get_kind(&self) -> TokenKind {
        match self {
            Token::Ident(_) => TokenKind::Ident,
            Token::For => TokenKind::For,
            Token::In => TokenKind::In,
            Token::As => TokenKind::As,
            Token::Func => TokenKind::Func,
            Token::If => TokenKind::If,
            Token::Else => TokenKind::Else,
            Token::Import => TokenKind::Import,
            Token::While => TokenKind::While,
            Token::Struct => TokenKind::Struct,
            Token::Enum => TokenKind::Enum,
            Token::Let => TokenKind::Let,
            Token::Return => TokenKind::Return,
            Token::True => TokenKind::True,
            Token::False => TokenKind::False,
            Token::Try => TokenKind::Try,
            Token::Catch => TokenKind::Catch,
            Token::Rethrow => TokenKind::Rethrow,

            Token::Integer(_) => TokenKind::Integer,
            Token::Float(_) => TokenKind::Float,
            Token::Char(_) => TokenKind::Char,
            Token::String(_) => TokenKind::String,

            Token::LParen => TokenKind::LParen,
            Token::RParen => TokenKind::RParen,
            Token::LSquare => TokenKind::LSquare,
            Token::RSquare => TokenKind::RSquare,
            Token::LCurly => TokenKind::LCurly,
            Token::RCurly => TokenKind::RCurly,

            Token::Dot => TokenKind::Dot,
            Token::Comma => TokenKind::Comma,
            Token::Colon => TokenKind::Colon,
            Token::ColonColon => TokenKind::ColonColon,
            Token::Semi => TokenKind::Semi,
            Token::Arrow => TokenKind::Arrow,

            Token::Eq => TokenKind::Eq,
            Token::Bang => TokenKind::Bang,
            Token::BangEq => TokenKind::BangEq,
            Token::MoreThan => TokenKind::MoreThan,
            Token::LessThan => TokenKind::LessThan,
            Token::MoreEq => TokenKind::MoreEq,
            Token::LessEq => TokenKind::LessEq,
            Token::EqEq => TokenKind::EqEq,
            Token::Plus => TokenKind::Plus,
            Token::Dash => TokenKind::Dash,
            Token::Star => TokenKind::Star,
            Token::Slash => TokenKind::Slash,
            Token::Ampersand => TokenKind::Ampersand,
            Token::AmpAmp => TokenKind::AmpAmp,
            Token::Pipe => TokenKind::Pipe,
            Token::PipePipe => TokenKind::PipePipe,
            Token::LShift => TokenKind::LShift,
            Token::RShift => TokenKind::RShift,
            Token::Caret => TokenKind::Caret,
            Token::Tilde => TokenKind::Tilde,
        }
    }
}

pub struct Lexer {
    source: Vec<char>,
    index: usize,
}

impl Lexer {
    fn get(&mut self, i: usize) -> Option<char> {
        if i >= self.source.len() {
            return None;
        }

        Some(self.source[i])
    }
    pub fn from(path: &str) -> Self {
        let source: Vec<char> = std::fs::read_to_string(path).unwrap().chars().collect();
        Self {
            source,
            index: 0usize,
        }
    }

    /// Build a lexer from an in-memory source string. The
    /// `Lexer::from` constructor reads from disk (and panics on
    /// I/O failure) — this variant skips both, so the bridge's
    /// validate endpoint can lex a user-supplied buffer without
    /// touching the filesystem.
    pub fn from_source(source: &str) -> Self {
        Self {
            source: source.chars().collect(),
            index: 0usize,
        }
    }

    pub fn tokens(&mut self) -> Vec<Token> {
        std::iter::from_fn(|| self.next_token()).collect()
    }
    fn next_token(&mut self) -> Option<Token> {
        self.skip_trivia();
        if self.index >= self.source.len() {
            return None;
        }

        if self.source[self.index].is_alphabetic() {
            return Some(self.identifier());
        }
        if self.source[self.index] == '\'' {
            self.index += 1;
            let c = self.source[self.index];
            self.index += 2;
            return Some(Token::Char(c));
        }
        if self.source[self.index] == '\"' {
            self.index += 1;
            let mut buf = String::new();
            while self.index < self.source.len() && self.source[self.index] != '\"' {
                buf.push(self.source[self.index]);
                self.index += 1;
            }
            self.index += 1;
            return Some(Token::String(buf));
        }
        if self.source[self.index].is_numeric() {
            return Some(self.number());
        }

        let token = match self.source[self.index] {
            '(' => Token::LParen,
            ')' => Token::RParen,
            '[' => Token::LSquare,
            ']' => Token::RSquare,
            '{' => Token::LCurly,
            '}' => Token::RCurly,

            '.' => Token::Dot,
            ',' => Token::Comma,
            ':' => {
                if self.get(self.index + 1) == Some(':') {
                    self.index += 1;
                    Token::ColonColon
                } else {
                    Token::Colon
                }
            }
            ';' => Token::Semi,

            '=' => {
                if self.get(self.index + 1) == Some('=') {
                    self.index += 1;
                    Token::EqEq
                } else {
                    Token::Eq
                }
            }
            '>' => {
                if self.get(self.index + 1) == Some('=') {
                    self.index += 1;
                    Token::MoreEq
                } else if self.get(self.index + 1) == Some('>') {
                    self.index += 1;
                    Token::RShift
                } else {
                    Token::MoreThan
                }
            }
            '<' => {
                if self.get(self.index + 1) == Some('=') {
                    self.index += 1;
                    Token::LessEq
                } else if self.get(self.index + 1) == Some('<') {
                    self.index += 1;
                    Token::LShift
                } else {
                    Token::LessThan
                }
            }
            '+' => Token::Plus,
            '-' => {
                if self.get(self.index + 1) == Some('>') {
                    self.index += 1;
                    Token::Arrow
                } else {
                    Token::Dash
                }
            }
            '*' => Token::Star,
            '/' => Token::Slash,

            '!' => {
                if self.get(self.index + 1) == Some('=') {
                    self.index += 1;
                    Token::BangEq
                } else {
                    Token::Bang
                }
            }
            '&' => {
                if self.get(self.index + 1) == Some('&') {
                    self.index += 1;
                    Token::AmpAmp
                } else {
                    Token::Ampersand
                }
            }
            '|' => {
                if self.get(self.index + 1) == Some('|') {
                    self.index += 1;
                    Token::PipePipe
                } else {
                    Token::Pipe
                }
            }
            '^' => Token::Caret,
            '~' => Token::Tilde,

            c => {
                eprintln!("unrecognized character: '{c}'");
                panic!("sol: aborted");
            }
        };
        self.index += 1;
        Some(token)
    }
    fn skip_trivia(&mut self) {
        loop {
            while self.index < self.source.len()
                && (self.source[self.index].is_whitespace() || self.source[self.index] == '_')
            {
                self.index += 1;
            }

            if self.index + 1 < self.source.len()
                && self.source[self.index] == '/'
                && self.source[self.index + 1] == '/'
            {
                self.index += 2;
                while self.index < self.source.len() && self.source[self.index] != '\n' {
                    self.index += 1;
                }
                continue;
            }

            if self.index + 1 < self.source.len()
                && self.source[self.index] == '/'
                && self.source[self.index + 1] == '*'
            {
                self.index += 2;
                while self.index + 1 < self.source.len()
                    && !(self.source[self.index] == '*' && self.source[self.index + 1] == '/')
                {
                    self.index += 1;
                }
                if self.index + 1 < self.source.len() {
                    self.index += 2;
                }
                continue;
            }

            break;
        }
    }
    fn identifier(&mut self) -> Token {
        let mut buf = String::new();

        while self.index < self.source.len()
            && (self.source[self.index].is_alphanumeric() || self.source[self.index] == '_')
        {
            buf.push(self.source[self.index]);
            self.index += 1;
        }

        match buf.as_str() {
            "for" => Token::For,
            "in" => Token::In,
            "as" => Token::As,
            "function" => Token::Func,
            "if" => Token::If,
            "else" => Token::Else,
            "import" => Token::Import,
            "while" => Token::While,
            "struct" => Token::Struct,
            "enum" => Token::Enum,
            "let" => Token::Let,
            "return" => Token::Return,
            "true" => Token::True,
            "false" => Token::False,
            "try" => Token::Try,
            "catch" => Token::Catch,
            "rethrow" => Token::Rethrow,
            _ => Token::Ident(buf),
        }
    }
    fn number(&mut self) -> Token {
        let mut buf = String::new();
        let mut is_float = false;

        while self.index < self.source.len() {
            let c = self.source[self.index];

            if c.is_ascii_digit() {
                buf.push(c);
                self.index += 1;
            } else if c == '.' && !is_float {
                if self.index + 1 < self.source.len()
                    && self.get(self.index + 1).is_some_and(|c| c.is_ascii_digit())
                {
                    is_float = true;
                    buf.push(c);
                    self.index += 1;
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        if is_float {
            let val = buf.parse::<f64>().unwrap_or(0.0);
            Token::Float(val)
        } else {
            let val = buf.parse::<i128>().unwrap_or(0);
            Token::Integer(val)
        }
    }
}
