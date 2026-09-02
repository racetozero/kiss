//! Single-pass tokenizer for the workflow script subset.
//!
//! The tokenizer walks the source bytes once and never uses a regular
//! expression. Identifiers are interned as they are seen, so each distinct name
//! allocates once and the parser compares integers instead of strings.

use crate::diagnostic::Diagnostic;
use std::collections::HashMap;
use std::sync::Arc;

/// A symbol is an index into [`Interner`].
pub(crate) type Symbol = u32;

/// Identifier storage shared by the tokenizer and the parsed script.
#[derive(Debug, Default)]
pub(crate) struct Interner {
    names: Vec<Arc<str>>,
    index: HashMap<Arc<str>, Symbol>,
}

impl Interner {
    pub(crate) fn intern(&mut self, name: &str) -> Symbol {
        if let Some(symbol) = self.index.get(name) {
            return *symbol;
        }
        let shared: Arc<str> = Arc::from(name);
        let symbol = self.names.len() as Symbol;
        self.names.push(shared.clone());
        self.index.insert(shared, symbol);
        symbol
    }

    pub(crate) fn resolve(&self, symbol: Symbol) -> &str {
        self.names
            .get(symbol as usize)
            .map(Arc::as_ref)
            .unwrap_or("")
    }

    pub(crate) fn shared(&self, symbol: Symbol) -> Arc<str> {
        self.names
            .get(symbol as usize)
            .cloned()
            .unwrap_or_else(|| Arc::from(""))
    }

    /// The number of distinct names, so callers can build a table indexed by
    /// symbol.
    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }
}

/// A keyword the script subset understands, or one it deliberately rejects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Keyword {
    Const,
    Let,
    If,
    Else,
    For,
    Of,
    While,
    Break,
    Continue,
    Return,
    Await,
    True,
    False,
    Null,
    Export,
    New,
    /// A JavaScript keyword this subset does not support. The parser turns it
    /// into an error naming the supported alternative.
    Unsupported(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    Num(f64),
    Str(Arc<str>),
    Ident(Symbol),
    Key(Keyword),

    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Dot,
    Semi,
    Colon,
    Question,
    Arrow,

    Assign,
    EqEqEq,
    BangEqEq,
    Lt,
    Le,
    Gt,
    Ge,
    Bang,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    AndAnd,
    OrOr,
    QuestionQuestion,
    /// Tokenized rather than rejected here so the parser can report the more
    /// useful counted-for-loop error when `i++` appears inside `for (...)`.
    PlusPlus,
    MinusMinus,

    /// A back-quoted string opens with this token.
    TemplateStart,
    /// One literal run inside a back-quoted string. Always present between
    /// interpolations, even when empty.
    TemplateChunk(Arc<str>),
    /// `${` inside a back-quoted string.
    TemplateExprStart,
    /// The `}` that closes an interpolation.
    TemplateExprEnd,
    TemplateEnd,

    Eof,
}

impl Tok {
    /// A short name used in "expected X, found Y" messages.
    pub(crate) fn describe(&self) -> String {
        match self {
            Tok::Num(value) => format!("the number `{value}`"),
            Tok::Str(_) => "a string".into(),
            Tok::Ident(_) => "a name".into(),
            Tok::Key(Keyword::Unsupported(word)) => format!("`{word}`"),
            Tok::Key(_) => "a keyword".into(),
            Tok::LParen => "`(`".into(),
            Tok::RParen => "`)`".into(),
            Tok::LBracket => "`[`".into(),
            Tok::RBracket => "`]`".into(),
            Tok::LBrace => "`{`".into(),
            Tok::RBrace => "`}`".into(),
            Tok::Comma => "`,`".into(),
            Tok::Dot => "`.`".into(),
            Tok::Semi => "`;`".into(),
            Tok::Colon => "`:`".into(),
            Tok::Question => "`?`".into(),
            Tok::Arrow => "`=>`".into(),
            Tok::Assign => "`=`".into(),
            Tok::PlusPlus => "`++`".into(),
            Tok::MinusMinus => "`--`".into(),
            Tok::Eof => "the end of the script".into(),
            _ => "an operator".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub kind: Tok,
    pub line: u32,
    pub column: u32,
}

/// One back-quoted string the tokenizer is inside.
struct TemplateFrame {
    /// True when the next thing to read is a literal run rather than an
    /// ordinary token.
    expect_chunk: bool,
    /// Brace nesting inside the current `${ ... }`, so that an object literal
    /// written inside an interpolation does not end it early.
    depth: u32,
}

pub(crate) struct Lexer<'a> {
    source: &'a [u8],
    position: usize,
    line: u32,
    column: u32,
    templates: Vec<TemplateFrame>,
}

/// The deepest back-quoted string nesting the tokenizer accepts. The release
/// profile aborts on a stack overflow, so every nesting limit is explicit.
const MAX_TEMPLATE_DEPTH: usize = 16;

impl<'a> Lexer<'a> {
    /// Tokenize the whole source, interning identifiers into `interner`.
    pub(crate) fn tokenize(
        source: &'a str,
        interner: &mut Interner,
    ) -> Result<Vec<Token>, Diagnostic> {
        let mut lexer = Lexer {
            source: source.as_bytes(),
            position: 0,
            line: 1,
            column: 1,
            templates: Vec::new(),
        };
        let mut out = Vec::with_capacity(source.len() / 4 + 8);
        lexer.scan(&mut out, interner)?;
        out.push(Token {
            kind: Tok::Eof,
            line: lexer.line,
            column: lexer.column,
        });
        Ok(out)
    }

    fn scan(&mut self, out: &mut Vec<Token>, interner: &mut Interner) -> Result<(), Diagnostic> {
        loop {
            if self
                .templates
                .last()
                .is_some_and(|frame| frame.expect_chunk)
            {
                self.template_chunk(out)?;
                continue;
            }

            self.skip_trivia()?;
            let line = self.line;
            let column = self.column;
            let Some(byte) = self.peek() else {
                if self.templates.is_empty() {
                    return Ok(());
                }
                return Err(Diagnostic::new(
                    line,
                    column,
                    "unterminated template string",
                ));
            };

            let kind = match byte {
                b'0'..=b'9' => self.number()?,
                b'"' | b'\'' => self.quoted_string()?,
                b'`' => {
                    if self.templates.len() >= MAX_TEMPLATE_DEPTH {
                        return Err(self.error("template strings are nested too deeply"));
                    }
                    self.bump();
                    self.templates.push(TemplateFrame {
                        expect_chunk: true,
                        depth: 0,
                    });
                    Tok::TemplateStart
                }
                b'_' | b'$' | b'a'..=b'z' | b'A'..=b'Z' => self.word(interner),
                _ => match self.punctuation(out, line, column)? {
                    Some(kind) => kind,
                    None => continue,
                },
            };
            out.push(Token { kind, line, column });
        }
    }

    fn peek(&self) -> Option<u8> {
        self.source.get(self.position).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.source.get(self.position + offset).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.source.get(self.position).copied()?;
        self.position += 1;
        if byte == b'\n' {
            self.line += 1;
            self.column = 1;
        } else if byte & 0xC0 != 0x80 {
            // Count one column per character, not per UTF-8 continuation byte.
            self.column += 1;
        }
        Some(byte)
    }

    fn error(&self, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(self.line, self.column, message)
    }

    fn skip_trivia(&mut self) -> Result<(), Diagnostic> {
        loop {
            match self.peek() {
                Some(byte) if byte.is_ascii_whitespace() => {
                    self.bump();
                }
                Some(b'/') if self.peek_at(1) == Some(b'/') => {
                    while let Some(byte) = self.peek() {
                        if byte == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                Some(b'/') if self.peek_at(1) == Some(b'*') => {
                    let line = self.line;
                    let column = self.column;
                    self.bump();
                    self.bump();
                    loop {
                        match self.peek() {
                            None => {
                                return Err(Diagnostic::new(
                                    line,
                                    column,
                                    "unterminated block comment",
                                ));
                            }
                            Some(b'*') if self.peek_at(1) == Some(b'/') => {
                                self.bump();
                                self.bump();
                                break;
                            }
                            _ => {
                                self.bump();
                            }
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn number(&mut self) -> Result<Tok, Diagnostic> {
        let start = self.position;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.bump();
        }
        if self.peek() == Some(b'.') && matches!(self.peek_at(1), Some(b'0'..=b'9')) {
            self.bump();
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let offset = if matches!(self.peek_at(1), Some(b'+' | b'-')) {
                2
            } else {
                1
            };
            if matches!(self.peek_at(offset), Some(b'0'..=b'9')) {
                for _ in 0..offset {
                    self.bump();
                }
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.bump();
                }
            }
        }
        let text = self
            .source
            .get(start..self.position)
            .and_then(|slice| std::str::from_utf8(slice).ok())
            .unwrap_or_default();
        let value: f64 = text
            .parse()
            .map_err(|_| self.error(format!("`{text}` is not a valid number")))?;
        Ok(Tok::Num(value))
    }

    fn quoted_string(&mut self) -> Result<Tok, Diagnostic> {
        let line = self.line;
        let column = self.column;
        let quote = self.bump().unwrap_or(b'"');
        let mut out = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(Diagnostic::new(line, column, "unterminated string"));
            };
            if byte == quote {
                self.bump();
                return Ok(Tok::Str(Arc::from(out.as_str())));
            }
            if byte == b'\n' {
                return Err(Diagnostic::new(line, column, "unterminated string"));
            }
            if byte == b'\\' {
                self.bump();
                self.escape(&mut out)?;
                continue;
            }
            self.push_char(&mut out);
        }
    }

    /// Copy one whole character, which may be several UTF-8 bytes.
    fn push_char(&mut self, out: &mut String) {
        let start = self.position;
        self.bump();
        while self.peek().is_some_and(|byte| byte & 0xC0 == 0x80) {
            self.bump();
        }
        if let Some(text) = self
            .source
            .get(start..self.position)
            .and_then(|slice| std::str::from_utf8(slice).ok())
        {
            out.push_str(text);
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), Diagnostic> {
        let Some(byte) = self.bump() else {
            return Err(self.error("unterminated escape"));
        };
        match byte {
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'0' => out.push('\0'),
            b'\\' => out.push('\\'),
            b'\'' => out.push('\''),
            b'"' => out.push('"'),
            b'`' => out.push('`'),
            b'$' => out.push('$'),
            b'\n' => {}
            other => {
                return Err(self.error(format!("`\\{}` is not a supported escape", other as char)));
            }
        }
        Ok(())
    }

    fn word(&mut self, interner: &mut Interner) -> Tok {
        let start = self.position;
        while matches!(
            self.peek(),
            Some(b'_' | b'$' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9')
        ) {
            self.bump();
        }
        let text = self
            .source
            .get(start..self.position)
            .and_then(|slice| std::str::from_utf8(slice).ok())
            .unwrap_or_default();
        match keyword(text) {
            Some(keyword) => Tok::Key(keyword),
            None => Tok::Ident(interner.intern(text)),
        }
    }

    /// Read one operator. Returns `None` when the operator was a `}` that ended
    /// a template interpolation, which pushes its own tokens.
    fn punctuation(
        &mut self,
        out: &mut Vec<Token>,
        line: u32,
        column: u32,
    ) -> Result<Option<Tok>, Diagnostic> {
        let byte = self.peek().unwrap_or(b'\0');
        let next = self.peek_at(1);
        let third = self.peek_at(2);

        // A `}` at interpolation depth zero closes `${ ... }` rather than a
        // block, and returns the tokenizer to reading literal text.
        if byte == b'}'
            && let Some(frame) = self.templates.last_mut()
            && !frame.expect_chunk
            && frame.depth == 0
        {
            frame.expect_chunk = true;
            self.bump();
            out.push(Token {
                kind: Tok::TemplateExprEnd,
                line,
                column,
            });
            return Ok(None);
        }

        let (kind, width) = match (byte, next, third) {
            (b'=', Some(b'='), Some(b'=')) => (Tok::EqEqEq, 3),
            (b'!', Some(b'='), Some(b'=')) => (Tok::BangEqEq, 3),
            (b'=', Some(b'>'), _) => (Tok::Arrow, 2),
            (b'&', Some(b'&'), _) => (Tok::AndAnd, 2),
            (b'|', Some(b'|'), _) => (Tok::OrOr, 2),
            (b'?', Some(b'?'), _) => (Tok::QuestionQuestion, 2),
            (b'<', Some(b'='), _) => (Tok::Le, 2),
            (b'>', Some(b'='), _) => (Tok::Ge, 2),
            (b'=', Some(b'='), _) => {
                return Err(self
                    .error("`==` is not supported")
                    .with_help("use `===`, which does not convert types"));
            }
            (b'!', Some(b'='), _) => {
                return Err(self
                    .error("`!=` is not supported")
                    .with_help("use `!==`, which does not convert types"));
            }
            (b'+', Some(b'+'), _) => (Tok::PlusPlus, 2),
            (b'-', Some(b'-'), _) => (Tok::MinusMinus, 2),
            (b'.', Some(b'.'), Some(b'.')) => {
                return Err(self
                    .error("the spread operator `...` is not supported")
                    .with_help("use `first.concat(second)` to join two arrays"));
            }
            (b'?', Some(b'.'), _) => {
                return Err(self
                    .error("optional chaining `?.` is not supported")
                    .with_help("test the value first, for example `value && value.field`"));
            }
            (b'(', _, _) => (Tok::LParen, 1),
            (b')', _, _) => (Tok::RParen, 1),
            (b'[', _, _) => (Tok::LBracket, 1),
            (b']', _, _) => (Tok::RBracket, 1),
            (b'{', _, _) => (Tok::LBrace, 1),
            (b'}', _, _) => (Tok::RBrace, 1),
            (b',', _, _) => (Tok::Comma, 1),
            (b'.', _, _) => (Tok::Dot, 1),
            (b';', _, _) => (Tok::Semi, 1),
            (b':', _, _) => (Tok::Colon, 1),
            (b'?', _, _) => (Tok::Question, 1),
            (b'=', _, _) => (Tok::Assign, 1),
            (b'<', _, _) => (Tok::Lt, 1),
            (b'>', _, _) => (Tok::Gt, 1),
            (b'!', _, _) => (Tok::Bang, 1),
            (b'+', _, _) => (Tok::Plus, 1),
            (b'-', _, _) => (Tok::Minus, 1),
            (b'*', _, _) => (Tok::Star, 1),
            (b'/', _, _) => (Tok::Slash, 1),
            (b'%', _, _) => (Tok::Percent, 1),
            _ => {
                let shown = self
                    .source
                    .get(self.position..)
                    .and_then(|rest| std::str::from_utf8(rest).ok())
                    .and_then(|rest| rest.chars().next())
                    .unwrap_or('?');
                return Err(self.error(format!("`{shown}` is not valid in a workflow script")));
            }
        };

        // Track brace nesting so an object literal inside `${ ... }` does not
        // end the interpolation at its own closing brace.
        if let Some(frame) = self.templates.last_mut() {
            match kind {
                Tok::LBrace => frame.depth += 1,
                Tok::RBrace => frame.depth = frame.depth.saturating_sub(1),
                _ => {}
            }
        }

        for _ in 0..width {
            self.bump();
        }
        Ok(Some(kind))
    }

    /// Read one literal run of a back-quoted string, then the token that ends
    /// it: either the start of an interpolation or the end of the string.
    fn template_chunk(&mut self, out: &mut Vec<Token>) -> Result<(), Diagnostic> {
        let line = self.line;
        let column = self.column;
        let mut text = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(Diagnostic::new(
                    line,
                    column,
                    "unterminated template string",
                ));
            };
            match byte {
                b'`' => {
                    self.bump();
                    self.templates.pop();
                    out.push(Token {
                        kind: Tok::TemplateChunk(Arc::from(text.as_str())),
                        line,
                        column,
                    });
                    out.push(Token {
                        kind: Tok::TemplateEnd,
                        line: self.line,
                        column: self.column,
                    });
                    return Ok(());
                }
                b'$' if self.peek_at(1) == Some(b'{') => {
                    self.bump();
                    self.bump();
                    if let Some(frame) = self.templates.last_mut() {
                        frame.expect_chunk = false;
                        frame.depth = 0;
                    }
                    out.push(Token {
                        kind: Tok::TemplateChunk(Arc::from(text.as_str())),
                        line,
                        column,
                    });
                    out.push(Token {
                        kind: Tok::TemplateExprStart,
                        line: self.line,
                        column: self.column,
                    });
                    return Ok(());
                }
                b'\\' => {
                    self.bump();
                    self.escape(&mut text)?;
                }
                _ => self.push_char(&mut text),
            }
        }
    }
}

fn keyword(text: &str) -> Option<Keyword> {
    Some(match text {
        "const" => Keyword::Const,
        "let" => Keyword::Let,
        "if" => Keyword::If,
        "else" => Keyword::Else,
        "for" => Keyword::For,
        "of" => Keyword::Of,
        "while" => Keyword::While,
        "break" => Keyword::Break,
        "continue" => Keyword::Continue,
        "return" => Keyword::Return,
        "await" => Keyword::Await,
        "true" => Keyword::True,
        "false" => Keyword::False,
        "null" => Keyword::Null,
        "export" => Keyword::Export,
        "new" => Keyword::New,
        "var" => Keyword::Unsupported("var"),
        "function" => Keyword::Unsupported("function"),
        "class" => Keyword::Unsupported("class"),
        "try" => Keyword::Unsupported("try"),
        "catch" => Keyword::Unsupported("catch"),
        "finally" => Keyword::Unsupported("finally"),
        "throw" => Keyword::Unsupported("throw"),
        "switch" => Keyword::Unsupported("switch"),
        "case" => Keyword::Unsupported("case"),
        "do" => Keyword::Unsupported("do"),
        "import" => Keyword::Unsupported("import"),
        "require" => Keyword::Unsupported("require"),
        "typeof" => Keyword::Unsupported("typeof"),
        "instanceof" => Keyword::Unsupported("instanceof"),
        "in" => Keyword::Unsupported("in"),
        "delete" => Keyword::Unsupported("delete"),
        "yield" => Keyword::Unsupported("yield"),
        "async" => Keyword::Unsupported("async"),
        "undefined" => Keyword::Unsupported("undefined"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(source: &str) -> Result<Vec<Tok>, Diagnostic> {
        let mut interner = Interner::default();
        Ok(Lexer::tokenize(source, &mut interner)?
            .into_iter()
            .map(|token| token.kind)
            .collect())
    }

    #[test]
    fn identifiers_intern_once_per_name() {
        let mut interner = Interner::default();
        let tokens = Lexer::tokenize("const a = a + a", &mut interner).unwrap();
        let symbols: Vec<Symbol> = tokens
            .iter()
            .filter_map(|token| match token.kind {
                Tok::Ident(symbol) => Some(symbol),
                _ => None,
            })
            .collect();
        assert_eq!(symbols, [0, 0, 0]);
        assert_eq!(interner.resolve(0), "a");
    }

    #[test]
    fn template_strings_split_into_chunks_and_expressions() {
        let tokens = lex("`Audit ${file} now`").unwrap();
        assert_eq!(
            tokens,
            [
                Tok::TemplateStart,
                Tok::TemplateChunk(Arc::from("Audit ")),
                Tok::TemplateExprStart,
                Tok::Ident(0),
                Tok::TemplateExprEnd,
                Tok::TemplateChunk(Arc::from(" now")),
                Tok::TemplateEnd,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn an_object_inside_an_interpolation_does_not_end_it_early() {
        let tokens = lex("`x ${ f({ a: 1 }) } y`").unwrap();
        let ends = tokens
            .iter()
            .filter(|token| **token == Tok::TemplateExprEnd)
            .count();
        assert_eq!(ends, 1);
        assert!(tokens.contains(&Tok::TemplateChunk(Arc::from(" y"))));
    }

    #[test]
    fn nested_template_strings_track_their_own_frames() {
        let tokens = lex("`a ${ `b ${c} d` } e`").unwrap();
        assert_eq!(
            tokens
                .iter()
                .filter(|token| **token == Tok::TemplateEnd)
                .count(),
            2
        );
        assert!(tokens.contains(&Tok::TemplateChunk(Arc::from(" e"))));
    }

    #[test]
    fn comments_and_whitespace_are_trivia() {
        let tokens = lex("// leading\nconst /* inline */ a = 1\n").unwrap();
        assert_eq!(
            tokens,
            [
                Tok::Key(Keyword::Const),
                Tok::Ident(0),
                Tok::Assign,
                Tok::Num(1.0),
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn numbers_cover_the_shapes_a_script_uses() {
        assert_eq!(lex("1").unwrap()[0], Tok::Num(1.0));
        assert_eq!(lex("1.5").unwrap()[0], Tok::Num(1.5));
        assert_eq!(lex("2e3").unwrap()[0], Tok::Num(2000.0));
        assert_eq!(lex("2e-3").unwrap()[0], Tok::Num(0.002));
        // A trailing dot is member access, not part of the number.
        assert_eq!(lex("1..toString").unwrap()[0], Tok::Num(1.0));
    }

    #[test]
    fn strings_handle_escapes_and_reject_unterminated_text() {
        assert_eq!(lex(r#""a\nb""#).unwrap()[0], Tok::Str(Arc::from("a\nb")));
        assert!(lex("\"open").is_err());
        assert!(lex("\"line\nbreak\"").is_err());
    }

    #[test]
    fn confusable_operators_name_their_replacement() {
        let error = lex("a == b").unwrap_err();
        assert!(error.message.contains("`==`"));
        assert_eq!(
            error.help.as_deref(),
            Some("use `===`, which does not convert types")
        );

        // `++` is tokenized so the parser can give the counted-loop error.
        assert_eq!(lex("i++").unwrap()[1], Tok::PlusPlus);

        let error = lex("value?.field").unwrap_err();
        assert!(error.message.contains("optional chaining"));

        let error = lex("[...list]").unwrap_err();
        assert!(error.message.contains("spread"));
    }

    #[test]
    fn unsupported_keywords_are_recognized_rather_than_treated_as_names() {
        assert_eq!(
            lex("import x").unwrap()[0],
            Tok::Key(Keyword::Unsupported("import"))
        );
        assert_eq!(
            lex("function f").unwrap()[0],
            Tok::Key(Keyword::Unsupported("function"))
        );
    }

    #[test]
    fn positions_count_characters_not_bytes() {
        // Prompts routinely contain non-ASCII text. A multi-byte character must
        // advance the column by one, so a later diagnostic points at the right
        // place on the line.
        let mut interner = Interner::default();
        let tokens = Lexer::tokenize("const a = 'señor' + b", &mut interner).unwrap();
        let plus = tokens
            .iter()
            .find(|token| token.kind == Tok::Plus)
            .expect("plus token");
        assert_eq!(plus.line, 1);
        assert_eq!(plus.column, 19);
    }

    #[test]
    fn identifiers_are_ascii_only() {
        // Model-written scripts use ASCII names. Rejecting the rest keeps the
        // scanner a byte loop, and the error names the character.
        let error = lex("const señor = 1").unwrap_err();
        assert!(error.message.contains('ñ'));
    }

    #[test]
    fn an_unterminated_template_is_an_error_not_a_hang() {
        assert!(lex("`open ${a}").is_err());
        assert!(lex("`open").is_err());
    }
}
