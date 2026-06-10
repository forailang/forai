use crate::token::{keyword_type, Token, TokenType};

pub struct Lexer {
    source: Vec<char>,
    index: usize,
    line: u32,
    column: u32,
    tokens: Vec<Token>,
    /// Depth of open ( and [ brackets. Newlines are suppressed when
    /// bracket_depth > block_depth (we're between arguments, not inside a block body).
    bracket_depth: u32,
    /// Count of block-opening keywords (do, if, for, etc.) encountered while
    /// bracket_depth > 0. When bracket_depth > block_depth, we're in the
    /// "inter-argument" zone and suppress newlines. When bracket_depth <= block_depth
    /// we're inside a block body and preserve newlines for statement separation.
    block_depth: u32,
    /// Lex errors accumulated during scanning. Surfaced as an Err from
    /// `scan_tokens` so the parser never sees a panicking lexer.
    errors: Vec<String>,
}

impl Lexer {
    pub fn new(source: &str) -> Self {
        Self {
            source: source.chars().collect(),
            index: 0,
            line: 1,
            column: 1,
            tokens: Vec::new(),
            bracket_depth: 0,
            block_depth: 0,
            errors: Vec::new(),
        }
    }

    pub fn scan_tokens(mut self) -> Result<Vec<Token>, String> {
        while !self.is_at_end() {
            let ch = self.peek();
            match ch {
                ' ' | '\t' | '\r' => {
                    self.advance();
                }
                '\n' => {
                    // Suppress newlines in two situations:
                    //   1. bracket_depth > block_depth: we're between function arguments
                    //      or array elements (not inside a do/if/for block body).
                    //      This allows multi-line calls: f(a,\n  b,\n  c)
                    //   2. Next non-blank line starts with '.': method chain continuation
                    //      Label()\n  .padding(20)\n  .fontSize(34)
                    let suppress =
                        self.bracket_depth > self.block_depth || self.next_line_starts_with_dot();
                    if !suppress {
                        self.push_token(TokenType::Newline, "\n".into());
                    }
                    self.advance_newline();
                }
                '#' => self.skip_comment(),
                '\'' => self.scan_string(),
                '"' => self.scan_template_string(),
                '0'..='9' => self.scan_number(),
                'a'..='z' | 'A'..='Z' | '_' => {
                    self.scan_identifier();
                    // When inside brackets, track block-opening keywords so we can
                    // distinguish "inter-argument" zones (suppress newlines) from
                    // block bodies (preserve newlines as statement separators).
                    if self.bracket_depth > 0 {
                        if let Some(last) = self.tokens.last() {
                            match last.token_type {
                                TokenType::Do
                                | TokenType::If
                                | TokenType::For
                                | TokenType::While
                                | TokenType::Case
                                | TokenType::Try
                                | TokenType::Type
                                | TokenType::Enum
                                | TokenType::Test
                                | TokenType::It
                                | TokenType::Extern => {
                                    self.block_depth += 1;
                                }
                                TokenType::End => {
                                    self.block_depth = self.block_depth.saturating_sub(1);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => self.scan_symbol(),
            }
        }

        self.tokens.push(Token {
            token_type: TokenType::Eof,
            lexeme: String::new(),
            line: self.line,
            column: self.column,
        });

        if self.errors.is_empty() {
            Ok(self.tokens)
        } else {
            Err(self.errors.join("\n"))
        }
    }

    fn scan_string(&mut self) {
        let start_line = self.line;
        let start_col = self.column;
        self.advance(); // skip opening '
        let mut value = String::new();

        while !self.is_at_end() && self.peek() != '\'' {
            if self.peek() == '\n' {
                self.errors.push(format!(
                    "Unterminated string at {}:{}",
                    start_line, start_col
                ));
                break;
            }
            if self.peek() == '\\' {
                self.advance();
                value.push(self.read_escape());
                continue;
            }
            value.push(self.advance());
        }

        if self.is_at_end() {
            self.errors.push(format!(
                "Unterminated string at {}:{}",
                start_line, start_col
            ));
        } else {
            self.advance(); // skip closing '
        }

        self.tokens.push(Token {
            token_type: TokenType::String,
            lexeme: value,
            line: start_line,
            column: start_col,
        });
    }

    fn scan_template_string(&mut self) {
        let start_line = self.line;
        let start_col = self.column;
        let multiline = self.peek() == '"' && self.peek_next() == '"' && self.peek_ahead(2) == '"';

        if multiline {
            self.advance();
            self.advance();
            self.advance();
        } else {
            self.advance();
        }

        let mut value = String::new();

        while !self.is_at_end() {
            if multiline {
                if self.peek() == '"' && self.peek_next() == '"' && self.peek_ahead(2) == '"' {
                    self.advance();
                    self.advance();
                    self.advance();
                    self.tokens.push(Token {
                        token_type: TokenType::TemplateString,
                        lexeme: value,
                        line: start_line,
                        column: start_col,
                    });
                    return;
                }
            } else if self.peek() == '"' {
                self.advance();
                self.tokens.push(Token {
                    token_type: TokenType::TemplateString,
                    lexeme: value,
                    line: start_line,
                    column: start_col,
                });
                return;
            }

            if !multiline && self.peek() == '\n' {
                self.errors.push(format!(
                    "Unterminated template string at {}:{}",
                    start_line, start_col
                ));
                self.tokens.push(Token {
                    token_type: TokenType::TemplateString,
                    lexeme: value,
                    line: start_line,
                    column: start_col,
                });
                return;
            }

            if self.peek() == '\n' {
                value.push('\n');
                self.advance_newline();
                continue;
            }

            if self.peek() == '\\' {
                self.advance();
                value.push(self.read_escape());
                continue;
            }

            value.push(self.advance());
        }

        // Fell off the end without a closing `"`.
        self.errors.push(format!(
            "Unterminated template string at {}:{}",
            start_line, start_col
        ));
        self.tokens.push(Token {
            token_type: TokenType::TemplateString,
            lexeme: value,
            line: start_line,
            column: start_col,
        });
    }

    fn scan_number(&mut self) {
        let start = self.index;
        let line = self.line;
        let column = self.column;
        let mut has_fraction = false;
        let mut has_exponent = false;

        while self.peek().is_ascii_digit() {
            self.advance();
        }

        if self.peek() == '.' && self.peek_next().is_ascii_digit() {
            has_fraction = true;
            self.advance();
            while self.peek().is_ascii_digit() {
                self.advance();
            }
        }

        if matches!(self.peek(), 'e' | 'E') {
            has_exponent = true;
            self.advance();
            if matches!(self.peek(), '+' | '-') {
                self.advance();
            }
            if !self.peek().is_ascii_digit() {
                self.errors.push(format!(
                    "Malformed scientific notation at {}:{}",
                    line, column
                ));
                while matches!(self.peek(), '0'..='9' | 'e' | 'E' | '+' | '-') {
                    self.advance();
                }
            } else {
                while self.peek().is_ascii_digit() {
                    self.advance();
                }
            }
        }

        if (has_fraction || has_exponent) && self.peek() == '.' && self.peek_next().is_ascii_digit()
        {
            self.errors
                .push(format!("Malformed number literal at {}:{}", line, column));
            while matches!(self.peek(), '0'..='9' | '.' | 'e' | 'E' | '+' | '-') {
                self.advance();
            }
        }

        let lexeme: String = self.source[start..self.index].iter().collect();
        self.tokens.push(Token {
            token_type: TokenType::Number,
            lexeme,
            line,
            column,
        });
    }

    fn scan_identifier(&mut self) {
        let start = self.index;
        let line = self.line;
        let column = self.column;

        while self.is_identifier_part(self.peek()) {
            self.advance();
        }

        let lexeme: String = self.source[start..self.index].iter().collect();
        let token_type = keyword_type(&lexeme).unwrap_or(TokenType::Identifier);
        self.tokens.push(Token {
            token_type,
            lexeme,
            line,
            column,
        });
    }

    fn scan_symbol(&mut self) {
        let line = self.line;
        let column = self.column;
        let ch = self.advance();

        // Three-character tokens — `...` must be checked before `..`
        // so `0...10` doesn't lex as `0.. . 10`.
        if ch == '.' && self.peek() == '.' && self.peek_ahead(1) == '.' {
            self.advance();
            self.advance();
            self.tokens.push(Token {
                token_type: TokenType::DotDotDot,
                lexeme: "...".to_string(),
                line,
                column,
            });
            return;
        }

        // Two-character tokens
        let next = self.peek();
        let two = format!("{}{}", ch, next);
        let two_char = match two.as_str() {
            ".." => Some(TokenType::DotDot),
            "->" => Some(TokenType::Arrow),
            "==" => Some(TokenType::EqualEqual),
            "!=" => Some(TokenType::BangEqual),
            "//" => Some(TokenType::SlashSlash),
            "**" => Some(TokenType::StarStar),
            "<=" => Some(TokenType::LessEqual),
            ">=" => Some(TokenType::GreaterEqual),
            _ => None,
        };

        if let Some(tt) = two_char {
            self.advance();
            self.tokens.push(Token {
                token_type: tt,
                lexeme: two,
                line,
                column,
            });
            return;
        }

        // Single-character tokens
        let token_type = match ch {
            '(' => {
                self.bracket_depth += 1;
                TokenType::LeftParen
            }
            ')' => {
                self.bracket_depth = self.bracket_depth.saturating_sub(1);
                if self.bracket_depth == 0 {
                    self.block_depth = 0;
                }
                TokenType::RightParen
            }
            '[' => {
                self.bracket_depth += 1;
                TokenType::LeftBracket
            }
            ']' => {
                self.bracket_depth = self.bracket_depth.saturating_sub(1);
                if self.bracket_depth == 0 {
                    self.block_depth = 0;
                }
                TokenType::RightBracket
            }
            '{' => TokenType::LeftBrace,
            '}' => TokenType::RightBrace,
            ',' => TokenType::Comma,
            ':' => TokenType::Colon,
            '.' => TokenType::Dot,
            '?' => TokenType::Question,
            '$' => TokenType::Dollar,
            '=' => TokenType::Assign,
            '!' => TokenType::Bang,
            '>' => TokenType::Greater,
            '<' => TokenType::Less,
            '+' => TokenType::Plus,
            '-' => TokenType::Minus,
            '*' => TokenType::Star,
            '/' => TokenType::Slash,
            '%' => TokenType::Percent,
            '@' => TokenType::At,
            _ => {
                // Accumulate as a lex error and keep going — if we
                // bail here, downstream diagnostics (which might be
                // more informative) never fire.
                self.errors.push(format!(
                    "Unexpected character '{}' at {}:{}",
                    ch, line, column
                ));
                return;
            }
        };

        self.tokens.push(Token {
            token_type,
            lexeme: ch.to_string(),
            line,
            column,
        });
    }

    /// Returns true if the next non-blank line (after this newline) starts with
    /// a single '.' — indicating a method chain continuation.
    /// Double dots ('..') are the range operator and do NOT continue.
    fn next_line_starts_with_dot(&self) -> bool {
        // self.index points at the current '\n'; look at chars after it.
        let mut i = self.index + 1;
        loop {
            match self.source.get(i).copied().unwrap_or('\0') {
                ' ' | '\t' | '\r' => i += 1,
                '#' => {
                    // Skip comment — advance to end of line, then keep looking
                    while i < self.source.len() && self.source[i] != '\n' {
                        i += 1;
                    }
                    // Skip the '\n' that ends the comment line
                    if i < self.source.len() {
                        i += 1;
                    }
                }
                '.' => {
                    // Single dot = method chain; double dot = range operator
                    let after = self.source.get(i + 1).copied().unwrap_or('\0');
                    return after != '.';
                }
                _ => return false,
            }
        }
    }

    fn push_token(&mut self, token_type: TokenType, lexeme: String) {
        self.tokens.push(Token {
            token_type,
            lexeme,
            line: self.line,
            column: self.column,
        });
    }

    fn skip_comment(&mut self) {
        let mut text = String::new();
        while !self.is_at_end() && self.peek() != '\n' {
            text.push(self.advance());
        }
        // Strip the `#` prefix plus ONE following space/tab. Keep any
        // additional leading whitespace so indentation survives inside
        // fenced code blocks in doc comments — `fai fmt` re-adds the
        // single `# ` on output, so round-tripping matches the original.
        let after_hash = text.trim_start_matches('#');
        let trimmed = if let Some(rest) = after_hash.strip_prefix(' ') {
            rest
        } else if let Some(rest) = after_hash.strip_prefix('\t') {
            rest
        } else {
            after_hash
        };
        self.push_token(TokenType::Comment, trimmed.to_string());
    }

    fn read_escape(&mut self) -> char {
        if self.is_at_end() {
            return '\\';
        }
        let ch = self.advance();
        match ch {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            '\\' => '\\',
            '\'' => '\'',
            '"' => '"',
            _ => ch,
        }
    }

    fn is_at_end(&self) -> bool {
        self.index >= self.source.len()
    }

    fn peek(&self) -> char {
        self.source.get(self.index).copied().unwrap_or('\0')
    }

    fn peek_next(&self) -> char {
        self.source.get(self.index + 1).copied().unwrap_or('\0')
    }

    fn peek_ahead(&self, offset: usize) -> char {
        self.source
            .get(self.index + offset)
            .copied()
            .unwrap_or('\0')
    }

    fn advance(&mut self) -> char {
        let ch = self.peek();
        self.index += 1;
        self.column += 1;
        ch
    }

    fn advance_newline(&mut self) {
        self.index += 1;
        self.line += 1;
        self.column = 1;
    }

    fn is_identifier_part(&self, ch: char) -> bool {
        ch.is_ascii_alphanumeric() || ch == '_'
    }
}
