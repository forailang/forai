#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    Eof,
    Identifier,
    String,
    TemplateString,
    Number,
    Newline,
    // Keywords
    Let,
    Var,
    Def,
    Type,
    Enum,
    If,
    Else,
    Case,
    When,
    Default,
    For,
    In,
    While,
    Break,
    Continue,
    Return,
    Try,
    Catch,
    Finally,
    End,
    Throw,
    Private,
    Test,
    It,
    BeforeAll,
    BeforeEach,
    AfterEach,
    AfterAll,
    True,
    False,
    Null,
    Use,
    From,
    And,
    Or,
    Not,
    Extern,
    Nowait,
    Do,
    With,
    Remote,
    // Symbols
    LeftParen,
    RightParen,
    LeftBracket,
    RightBracket,
    LeftBrace,
    RightBrace,
    Comma,
    Colon,
    DotDot,
    DotDotDot,
    Dot,
    Question,
    Dollar,
    Assign,
    Arrow,
    EqualEqual,
    Bang,
    BangEqual,
    Greater,
    GreaterEqual,
    Less,
    LessEqual,
    SlashSlash,
    Percent,
    StarStar,
    Plus,
    Minus,
    Star,
    Slash,
    At,
    Comment,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub token_type: TokenType,
    pub lexeme: String,
    pub line: u32,
    pub column: u32,
}

impl TokenType {
    /// Returns true if this token type is a keyword (not a symbol or literal).
    pub fn is_keyword(&self) -> bool {
        matches!(
            self,
            TokenType::Let
                | TokenType::Var
                | TokenType::Def
                | TokenType::Type
                | TokenType::Enum
                | TokenType::If
                | TokenType::Else
                | TokenType::Case
                | TokenType::When
                | TokenType::Default
                | TokenType::For
                | TokenType::In
                | TokenType::While
                | TokenType::Break
                | TokenType::Continue
                | TokenType::Try
                | TokenType::Catch
                | TokenType::Finally
                | TokenType::End
                | TokenType::Throw
                | TokenType::Private
                | TokenType::Test
                | TokenType::It
                | TokenType::BeforeAll
                | TokenType::BeforeEach
                | TokenType::AfterEach
                | TokenType::AfterAll
                | TokenType::Use
                | TokenType::From
                | TokenType::And
                | TokenType::Or
                | TokenType::Not
                | TokenType::Extern
                | TokenType::Nowait
                | TokenType::Do
                | TokenType::With
                | TokenType::Remote
        )
    }
}

pub fn keyword_type(lexeme: &str) -> Option<TokenType> {
    match lexeme {
        "let" => Some(TokenType::Let),
        "var" => Some(TokenType::Var),
        "def" => Some(TokenType::Def),
        "type" => Some(TokenType::Type),
        "enum" => Some(TokenType::Enum),
        "if" => Some(TokenType::If),
        "else" => Some(TokenType::Else),
        "case" => Some(TokenType::Case),
        "when" => Some(TokenType::When),
        "default" => Some(TokenType::Default),
        "for" => Some(TokenType::For),
        "in" => Some(TokenType::In),
        "while" => Some(TokenType::While),
        "break" => Some(TokenType::Break),
        "continue" => Some(TokenType::Continue),
        "return" => Some(TokenType::Return),
        "try" => Some(TokenType::Try),
        "catch" => Some(TokenType::Catch),
        "finally" => Some(TokenType::Finally),
        "end" => Some(TokenType::End),
        "throw" => Some(TokenType::Throw),
        "private" => Some(TokenType::Private),
        "test" => Some(TokenType::Test),
        "it" => Some(TokenType::It),
        "beforeAll" => Some(TokenType::BeforeAll),
        "beforeEach" => Some(TokenType::BeforeEach),
        "afterEach" => Some(TokenType::AfterEach),
        "afterAll" => Some(TokenType::AfterAll),
        "true" => Some(TokenType::True),
        "false" => Some(TokenType::False),
        "null" => Some(TokenType::Null),
        "use" => Some(TokenType::Use),
        "from" => Some(TokenType::From),
        "and" => Some(TokenType::And),
        "or" => Some(TokenType::Or),
        "not" => Some(TokenType::Not),
        "extern" => Some(TokenType::Extern),
        "nowait" => Some(TokenType::Nowait),
        "do" => Some(TokenType::Do),
        "with" => Some(TokenType::With),
        "remote" => Some(TokenType::Remote),
        _ => None,
    }
}
