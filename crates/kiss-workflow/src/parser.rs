//! Recursive-descent parser for the workflow script subset.
//!
//! The parser is deliberately strict: anything outside the documented subset is
//! an error that names a supported alternative, because these errors are handed
//! back to the model that wrote the script so it can correct itself.

use crate::ast::{
    Ast, BinOp, Expr, ExprId, Field, FnBody, FnDef, LogOp, Pos, Span, Stmt, StmtId, TemplatePart,
    UnOp,
};
use crate::diagnostic::Diagnostic;
use crate::lexer::{Interner, Keyword, Symbol, Tok, Token};

/// The deepest expression or block nesting the parser accepts. The release
/// profile aborts on a stack overflow, so recursion is bounded explicitly.
pub(crate) const MAX_DEPTH: u32 = 64;

pub(crate) struct Parsed {
    pub ast: Ast,
    pub body: Span,
    /// The object literal from `export const meta = { ... }`, when present.
    pub meta: Option<ExprId>,
}

pub(crate) fn parse(tokens: &[Token], interner: &mut Interner) -> Result<Parsed, Diagnostic> {
    let meta_symbol = interner.intern("meta");
    let mut parser = Parser {
        tokens,
        position: 0,
        ast: Ast::default(),
        depth: 0,
        meta: None,
        meta_symbol,
    };
    let mut statements = Vec::new();
    while !parser.at_end() {
        let statement = parser.statement(interner)?;
        statements.push(statement);
    }
    let body = parser.ast.add_stmts(&statements);
    Ok(Parsed {
        ast: parser.ast,
        body,
        meta: parser.meta,
    })
}

struct Parser<'a> {
    tokens: &'a [Token],
    position: usize,
    ast: Ast,
    depth: u32,
    meta: Option<ExprId>,
    meta_symbol: Symbol,
}

impl Parser<'_> {
    // ----- token helpers ----------------------------------------------------

    fn peek(&self) -> &Tok {
        self.kind_at(self.position)
    }

    fn kind_at(&self, index: usize) -> &Tok {
        self.tokens
            .get(index)
            .map(|token| &token.kind)
            .unwrap_or(&Tok::Eof)
    }

    fn here(&self) -> Pos {
        self.tokens
            .get(self.position)
            .or_else(|| self.tokens.last())
            .map(|token| Pos {
                line: token.line,
                column: token.column,
            })
            .unwrap_or_default()
    }

    fn at_end(&self) -> bool {
        matches!(self.peek(), Tok::Eof)
    }

    fn eat(&mut self, expected: &Tok) -> bool {
        if self.peek() == expected {
            self.position += 1;
            return true;
        }
        false
    }

    fn error(&self, message: impl Into<String>) -> Diagnostic {
        let pos = self.here();
        Diagnostic::new(pos.line, pos.column, message)
    }

    fn expect(&mut self, expected: Tok, context: &str) -> Result<(), Diagnostic> {
        if self.eat(&expected) {
            return Ok(());
        }
        Err(self.error(format!(
            "expected {} {context}, found {}",
            expected.describe(),
            self.peek().describe()
        )))
    }

    /// Enter one level of recursion.
    ///
    /// The matching [`Parser::leave`] is skipped on the error path on purpose:
    /// an error ends parsing immediately, so the depth counter is never read
    /// again.
    fn enter(&mut self) -> Result<(), Diagnostic> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self
                .error("this script nests too deeply")
                .with_help("split the work into separate statements or separate phases"));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn identifier(&mut self, context: &str) -> Result<Symbol, Diagnostic> {
        match *self.peek() {
            Tok::Ident(symbol) => {
                self.position += 1;
                Ok(symbol)
            }
            Tok::Key(Keyword::Unsupported(word)) => Err(self.error(format!(
                "`{word}` is a reserved word and cannot be used as {context}"
            ))),
            _ => {
                let found = self.peek().describe();
                Err(self.error(format!("expected {context}, found {found}")))
            }
        }
    }

    /// True when the current token begins a line later than the previous token.
    ///
    /// This stands in for JavaScript's automatic semicolon insertion in the one
    /// place it matters: a call or index on a new line that would otherwise
    /// glue itself onto the value produced by the previous line.
    fn starts_new_line(&self) -> bool {
        let (Some(current), Some(previous)) = (
            self.tokens.get(self.position),
            self.position
                .checked_sub(1)
                .and_then(|index| self.tokens.get(index)),
        ) else {
            return false;
        };
        current.line > previous.line
    }

    // ----- statements -------------------------------------------------------

    fn statement(&mut self, interner: &mut Interner) -> Result<StmtId, Diagnostic> {
        self.enter()?;
        let statement = self.statement_inner(interner)?;
        self.leave();
        Ok(statement)
    }

    fn statement_inner(&mut self, interner: &mut Interner) -> Result<StmtId, Diagnostic> {
        let pos = self.here();
        match *self.peek() {
            Tok::Key(Keyword::Export) => self.meta_declaration(interner, pos),
            Tok::Key(Keyword::Const | Keyword::Let) => {
                self.position += 1;
                let name = self.identifier("a variable name")?;
                self.expect(Tok::Assign, "after a variable name")?;
                let value = self.expression(interner)?;
                self.eat(&Tok::Semi);
                Ok(self.ast.push_stmt(Stmt::Declare { name, value }, pos))
            }
            Tok::Key(Keyword::If) => {
                self.position += 1;
                self.expect(Tok::LParen, "after `if`")?;
                let test = self.expression(interner)?;
                self.expect(Tok::RParen, "after an `if` condition")?;
                let consequent = self.block_or_statement(interner)?;
                let alternate = if self.eat(&Tok::Key(Keyword::Else)) {
                    self.block_or_statement(interner)?
                } else {
                    Span::default()
                };
                Ok(self.ast.push_stmt(
                    Stmt::If {
                        test,
                        consequent,
                        alternate,
                    },
                    pos,
                ))
            }
            Tok::Key(Keyword::For) => self.for_of(interner, pos),
            Tok::Key(Keyword::While) => {
                self.position += 1;
                self.expect(Tok::LParen, "after `while`")?;
                let test = self.expression(interner)?;
                self.expect(Tok::RParen, "after a `while` condition")?;
                let body = self.block_or_statement(interner)?;
                Ok(self.ast.push_stmt(Stmt::While { test, body }, pos))
            }
            Tok::Key(Keyword::Return) => {
                self.position += 1;
                let value = if matches!(self.peek(), Tok::Semi | Tok::RBrace | Tok::Eof)
                    || self.starts_new_line()
                {
                    None
                } else {
                    Some(self.expression(interner)?)
                };
                self.eat(&Tok::Semi);
                Ok(self.ast.push_stmt(Stmt::Return(value), pos))
            }
            Tok::Key(Keyword::Break) => {
                self.position += 1;
                self.eat(&Tok::Semi);
                Ok(self.ast.push_stmt(Stmt::Break, pos))
            }
            Tok::Key(Keyword::Continue) => {
                self.position += 1;
                self.eat(&Tok::Semi);
                Ok(self.ast.push_stmt(Stmt::Continue, pos))
            }
            Tok::Key(Keyword::Unsupported(word)) => Err(unsupported_keyword(pos, word)),
            Tok::Semi => {
                self.position += 1;
                let empty = self.ast.push_expr(Expr::Null, pos);
                Ok(self.ast.push_stmt(Stmt::Expr(empty), pos))
            }
            _ => {
                let expr = self.expression(interner)?;
                if self.eat(&Tok::Assign) {
                    if !matches!(
                        self.ast.expr(expr),
                        Expr::Name(_) | Expr::Member { .. } | Expr::Index { .. }
                    ) {
                        return Err(self.error("only a name, a field, or an index can be assigned"));
                    }
                    let value = self.expression(interner)?;
                    self.eat(&Tok::Semi);
                    return Ok(self.ast.push_stmt(
                        Stmt::Assign {
                            target: expr,
                            value,
                        },
                        pos,
                    ));
                }
                self.eat(&Tok::Semi);
                Ok(self.ast.push_stmt(Stmt::Expr(expr), pos))
            }
        }
    }

    fn meta_declaration(
        &mut self,
        interner: &mut Interner,
        pos: Pos,
    ) -> Result<StmtId, Diagnostic> {
        self.position += 1;
        if !matches!(self.peek(), Tok::Key(Keyword::Const)) {
            return Err(self
                .error("only `export const meta = { ... }` may be exported")
                .with_help("declare everything else with `const` and no `export`"));
        }
        self.position += 1;
        let name = self.identifier("a name")?;
        if name != self.meta_symbol {
            return Err(self
                .error("only `meta` may be exported")
                .with_help("a script exports one metadata object and nothing else"));
        }
        self.expect(Tok::Assign, "after `meta`")?;
        let value = self.expression(interner)?;
        if !matches!(self.ast.expr(value), Expr::Object(_)) {
            return Err(self
                .error("`meta` must be an object literal")
                .with_help("write `export const meta = { name: '...', description: '...' }`"));
        }
        if self.meta.is_some() {
            return Err(self.error("`meta` is declared more than once"));
        }
        self.meta = Some(value);
        self.eat(&Tok::Semi);
        Ok(self.ast.push_stmt(Stmt::Declare { name, value }, pos))
    }

    fn for_of(&mut self, interner: &mut Interner, pos: Pos) -> Result<StmtId, Diagnostic> {
        const HELP: &str = "counted loops are not available; to run one agent per item use \
                            `pipeline(list, item => agent(...))`";
        self.position += 1;
        self.expect(Tok::LParen, "after `for`")?;
        if !matches!(self.peek(), Tok::Key(Keyword::Const | Keyword::Let)) {
            return Err(self
                .error("only `for (const item of list)` is supported")
                .with_help(HELP));
        }
        self.position += 1;
        let name = self.identifier("a loop variable name")?;
        if !self.eat(&Tok::Key(Keyword::Of)) {
            return Err(self
                .error("only `for (const item of list)` is supported")
                .with_help(HELP));
        }
        let iterable = self.expression(interner)?;
        self.expect(Tok::RParen, "after the list in a `for` loop")?;
        let body = self.block_or_statement(interner)?;
        Ok(self.ast.push_stmt(
            Stmt::ForOf {
                name,
                iterable,
                body,
            },
            pos,
        ))
    }

    /// A braced block, or a single statement used as a one-statement body.
    fn block_or_statement(&mut self, interner: &mut Interner) -> Result<Span, Diagnostic> {
        if self.eat(&Tok::LBrace) {
            let statements = self.block_statements(interner)?;
            return Ok(self.ast.add_stmts(&statements));
        }
        let statement = self.statement(interner)?;
        Ok(self.ast.add_stmts(&[statement]))
    }

    /// Statements up to the closing brace, which this consumes.
    fn block_statements(&mut self, interner: &mut Interner) -> Result<Vec<StmtId>, Diagnostic> {
        let mut statements = Vec::new();
        while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
            statements.push(self.statement(interner)?);
        }
        self.expect(Tok::RBrace, "to close a block")?;
        Ok(statements)
    }

    // ----- expressions ------------------------------------------------------

    fn expression(&mut self, interner: &mut Interner) -> Result<ExprId, Diagnostic> {
        self.enter()?;
        let expr = self.conditional(interner)?;
        self.leave();
        Ok(expr)
    }

    fn conditional(&mut self, interner: &mut Interner) -> Result<ExprId, Diagnostic> {
        let test = self.binary(interner, 0)?;
        if !self.eat(&Tok::Question) {
            return Ok(test);
        }
        let pos = self.ast.expr_position(test);
        let consequent = self.expression(interner)?;
        self.expect(Tok::Colon, "in a `? :` expression")?;
        let alternate = self.expression(interner)?;
        Ok(self.ast.push_expr(
            Expr::Conditional {
                test,
                consequent,
                alternate,
            },
            pos,
        ))
    }

    fn binary(&mut self, interner: &mut Interner, level: u8) -> Result<ExprId, Diagnostic> {
        if level > MAX_BINARY_LEVEL {
            return self.unary(interner);
        }
        self.enter()?;
        let mut left = self.binary(interner, level + 1)?;
        while let Some((operator_level, operator)) = binary_operator(self.peek()) {
            if operator_level != level {
                break;
            }
            let pos = self.here();
            self.position += 1;
            let right = self.binary(interner, level + 1)?;
            left = match operator {
                Operator::Binary(op) => self.ast.push_expr(Expr::Binary { op, left, right }, pos),
                Operator::Logical(op) => self.ast.push_expr(Expr::Logical { op, left, right }, pos),
            };
        }
        self.leave();
        Ok(left)
    }

    fn unary(&mut self, interner: &mut Interner) -> Result<ExprId, Diagnostic> {
        let pos = self.here();
        match self.peek() {
            Tok::Bang => {
                self.position += 1;
                let operand = self.unary(interner)?;
                Ok(self.ast.push_expr(
                    Expr::Unary {
                        op: UnOp::Not,
                        operand,
                    },
                    pos,
                ))
            }
            Tok::Minus => {
                self.position += 1;
                let operand = self.unary(interner)?;
                Ok(self.ast.push_expr(
                    Expr::Unary {
                        op: UnOp::Negate,
                        operand,
                    },
                    pos,
                ))
            }
            Tok::Plus => Err(self
                .error("a leading `+` is not supported")
                .with_help("use `Number(value)` to convert a string to a number")),
            Tok::PlusPlus | Tok::MinusMinus => Err(increment_error(self.here(), self.peek())),
            Tok::Key(Keyword::Await) => {
                self.position += 1;
                let operand = self.unary(interner)?;
                Ok(self.ast.push_expr(Expr::Await(operand), pos))
            }
            Tok::Key(Keyword::New) => {
                self.position += 1;
                let callee = self.unary(interner)?;
                Ok(self.ast.push_expr(Expr::New { callee }, pos))
            }
            _ => self.postfix(interner),
        }
    }

    fn postfix(&mut self, interner: &mut Interner) -> Result<ExprId, Diagnostic> {
        let mut expr = self.primary(interner)?;
        loop {
            match self.peek() {
                Tok::Dot => {
                    let pos = self.here();
                    self.position += 1;
                    let name = match *self.peek() {
                        Tok::Ident(symbol) => {
                            self.position += 1;
                            symbol
                        }
                        Tok::Key(keyword) => {
                            self.position += 1;
                            interner.intern(keyword_text(keyword))
                        }
                        _ => {
                            let found = self.peek().describe();
                            return Err(self
                                .error(format!("expected a field name after `.`, found {found}")));
                        }
                    };
                    expr = self.ast.push_expr(Expr::Member { object: expr, name }, pos);
                }
                Tok::LBracket if !self.starts_new_line() => {
                    let pos = self.here();
                    self.position += 1;
                    let index = self.expression(interner)?;
                    self.expect(Tok::RBracket, "to close an index")?;
                    expr = self.ast.push_expr(
                        Expr::Index {
                            object: expr,
                            index,
                        },
                        pos,
                    );
                }
                Tok::LParen if !self.starts_new_line() => {
                    let pos = self.here();
                    self.position += 1;
                    let args = self.argument_list(interner)?;
                    expr = self.ast.push_expr(Expr::Call { callee: expr, args }, pos);
                }
                Tok::PlusPlus | Tok::MinusMinus => {
                    return Err(increment_error(self.here(), self.peek()));
                }
                _ => return Ok(expr),
            }
        }
    }

    /// Arguments up to the closing parenthesis, which this consumes.
    fn argument_list(&mut self, interner: &mut Interner) -> Result<Span, Diagnostic> {
        let mut args = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                args.push(self.expression(interner)?);
                if self.eat(&Tok::Comma) {
                    // Allow one trailing comma before the closing parenthesis.
                    if self.eat(&Tok::RParen) {
                        break;
                    }
                    continue;
                }
                self.expect(Tok::RParen, "to close a call")?;
                break;
            }
        }
        Ok(self.ast.add_exprs(&args))
    }

    fn primary(&mut self, interner: &mut Interner) -> Result<ExprId, Diagnostic> {
        let pos = self.here();
        match self.peek().clone() {
            Tok::Num(value) => {
                self.position += 1;
                Ok(self.ast.push_expr(Expr::Number(value), pos))
            }
            Tok::Str(text) => {
                self.position += 1;
                Ok(self.ast.push_expr(Expr::Text(text), pos))
            }
            Tok::Key(Keyword::True) => {
                self.position += 1;
                Ok(self.ast.push_expr(Expr::Bool(true), pos))
            }
            Tok::Key(Keyword::False) => {
                self.position += 1;
                Ok(self.ast.push_expr(Expr::Bool(false), pos))
            }
            Tok::Key(Keyword::Null) => {
                self.position += 1;
                Ok(self.ast.push_expr(Expr::Null, pos))
            }
            Tok::TemplateStart => self.template(interner),
            Tok::Ident(symbol) => {
                // `name => ...` is a one-parameter arrow function.
                if matches!(self.kind_at(self.position + 1), Tok::Arrow) {
                    self.position += 2;
                    return self.arrow_body(vec![symbol], interner, pos);
                }
                self.position += 1;
                Ok(self.ast.push_expr(Expr::Name(symbol), pos))
            }
            Tok::LBracket => {
                self.position += 1;
                let mut items = Vec::new();
                if !self.eat(&Tok::RBracket) {
                    loop {
                        items.push(self.expression(interner)?);
                        if self.eat(&Tok::Comma) {
                            if self.eat(&Tok::RBracket) {
                                break;
                            }
                            continue;
                        }
                        self.expect(Tok::RBracket, "to close an array")?;
                        break;
                    }
                }
                let span = self.ast.add_exprs(&items);
                Ok(self.ast.push_expr(Expr::Array(span), pos))
            }
            Tok::LBrace => self.object(interner),
            Tok::LParen => {
                if let Some(params) = self.arrow_parameters()? {
                    return self.arrow_body(params, interner, pos);
                }
                self.position += 1;
                let inner = self.expression(interner)?;
                self.expect(Tok::RParen, "to close a group")?;
                Ok(inner)
            }
            Tok::Key(Keyword::Unsupported(word)) => Err(unsupported_keyword(pos, word)),
            other => Err(self.error(format!("expected a value, found {}", other.describe()))),
        }
    }

    fn object(&mut self, interner: &mut Interner) -> Result<ExprId, Diagnostic> {
        let pos = self.here();
        self.position += 1;
        let mut fields = Vec::new();
        if !self.eat(&Tok::RBrace) {
            loop {
                let name = match self.peek().clone() {
                    Tok::Ident(symbol) => {
                        self.position += 1;
                        symbol
                    }
                    Tok::Str(text) => {
                        self.position += 1;
                        interner.intern(&text)
                    }
                    Tok::Key(keyword) => {
                        self.position += 1;
                        interner.intern(keyword_text(keyword))
                    }
                    Tok::LBracket => {
                        return Err(self
                            .error("computed object keys are not supported")
                            .with_help("write the key directly, for example `{ label: value }`"));
                    }
                    other => {
                        return Err(self
                            .error(format!("expected a field name, found {}", other.describe())));
                    }
                };
                let value = if self.eat(&Tok::Colon) {
                    self.expression(interner)?
                } else {
                    // Shorthand: `{ files }` means `{ files: files }`.
                    let field_pos = self.here();
                    self.ast.push_expr(Expr::Name(name), field_pos)
                };
                fields.push(Field { name, value });
                if self.eat(&Tok::Comma) {
                    if self.eat(&Tok::RBrace) {
                        break;
                    }
                    continue;
                }
                self.expect(Tok::RBrace, "to close an object")?;
                break;
            }
        }
        let span = self.ast.add_fields(&fields);
        Ok(self.ast.push_expr(Expr::Object(span), pos))
    }

    fn template(&mut self, interner: &mut Interner) -> Result<ExprId, Diagnostic> {
        let pos = self.here();
        self.position += 1;
        let mut parts = Vec::new();
        loop {
            match self.peek().clone() {
                Tok::TemplateChunk(text) => {
                    self.position += 1;
                    if !text.is_empty() {
                        parts.push(TemplatePart::Chunk(text));
                    }
                }
                Tok::TemplateExprStart => {
                    self.position += 1;
                    let expr = self.expression(interner)?;
                    self.expect(Tok::TemplateExprEnd, "to close `${`")?;
                    parts.push(TemplatePart::Expr(expr));
                }
                Tok::TemplateEnd => {
                    self.position += 1;
                    let span = self.ast.add_parts(parts);
                    return Ok(self.ast.push_expr(Expr::Template(span), pos));
                }
                other => {
                    return Err(self.error(format!(
                        "unexpected {} inside a template string",
                        other.describe()
                    )));
                }
            }
        }
    }

    /// Detect `(a, b) => ...` and consume the parameter list when it is one.
    ///
    /// Returns `None` when the parenthesis opens an ordinary grouped
    /// expression, leaving the position unchanged.
    fn arrow_parameters(&mut self) -> Result<Option<Vec<Symbol>>, Diagnostic> {
        let mut depth = 0usize;
        let mut scan = self.position;
        loop {
            match self.kind_at(scan) {
                Tok::LParen => depth += 1,
                Tok::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Tok::Eof => return Ok(None),
                _ => {}
            }
            scan += 1;
        }
        if !matches!(self.kind_at(scan + 1), Tok::Arrow) {
            return Ok(None);
        }

        self.position += 1;
        let mut params = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                params.push(self.identifier("a parameter name")?);
                if self.eat(&Tok::Comma) {
                    if self.eat(&Tok::RParen) {
                        break;
                    }
                    continue;
                }
                self.expect(Tok::RParen, "to close a parameter list")?;
                break;
            }
        }
        self.expect(Tok::Arrow, "after a parameter list")?;
        Ok(Some(params))
    }

    fn arrow_body(
        &mut self,
        params: Vec<Symbol>,
        interner: &mut Interner,
        pos: Pos,
    ) -> Result<ExprId, Diagnostic> {
        self.enter()?;
        let body = if self.eat(&Tok::LBrace) {
            let statements = self.block_statements(interner)?;
            FnBody::Block(self.ast.add_stmts(&statements))
        } else {
            FnBody::Expr(self.expression(interner)?)
        };
        let id = self.ast.add_function(FnDef { params, body });
        self.leave();
        Ok(self.ast.push_expr(Expr::Arrow(id), pos))
    }
}

#[derive(Debug, Clone, Copy)]
enum Operator {
    Binary(BinOp),
    Logical(LogOp),
}

/// The loosest-binding operator level. Levels count upward from zero.
const MAX_BINARY_LEVEL: u8 = 5;

/// Map an operator token to its precedence level and meaning.
///
/// A function rather than a table, so that comparing the current token costs a
/// match on a discriminant and never materializes a temporary.
fn binary_operator(token: &Tok) -> Option<(u8, Operator)> {
    Some(match token {
        Tok::OrOr => (0, Operator::Logical(LogOp::Or)),
        Tok::QuestionQuestion => (0, Operator::Logical(LogOp::NullCoalesce)),
        Tok::AndAnd => (1, Operator::Logical(LogOp::And)),
        Tok::EqEqEq => (2, Operator::Binary(BinOp::Equal)),
        Tok::BangEqEq => (2, Operator::Binary(BinOp::NotEqual)),
        Tok::Lt => (3, Operator::Binary(BinOp::Less)),
        Tok::Le => (3, Operator::Binary(BinOp::LessOrEqual)),
        Tok::Gt => (3, Operator::Binary(BinOp::Greater)),
        Tok::Ge => (3, Operator::Binary(BinOp::GreaterOrEqual)),
        Tok::Plus => (4, Operator::Binary(BinOp::Add)),
        Tok::Minus => (4, Operator::Binary(BinOp::Subtract)),
        Tok::Star => (5, Operator::Binary(BinOp::Multiply)),
        Tok::Slash => (5, Operator::Binary(BinOp::Divide)),
        Tok::Percent => (5, Operator::Binary(BinOp::Remainder)),
        _ => return None,
    })
}

fn increment_error(pos: Pos, token: &Tok) -> Diagnostic {
    let (operator, replacement) = match token {
        Tok::MinusMinus => ("--", "count = count - 1"),
        _ => ("++", "count = count + 1"),
    };
    Diagnostic::new(
        pos.line,
        pos.column,
        format!("`{operator}` is not supported"),
    )
    .with_help(format!(
        "use `{replacement}`, or walk a list with `for (const item of list)`"
    ))
}

fn keyword_text(keyword: Keyword) -> &'static str {
    match keyword {
        Keyword::Const => "const",
        Keyword::Let => "let",
        Keyword::If => "if",
        Keyword::Else => "else",
        Keyword::For => "for",
        Keyword::Of => "of",
        Keyword::While => "while",
        Keyword::Break => "break",
        Keyword::Continue => "continue",
        Keyword::Return => "return",
        Keyword::Await => "await",
        Keyword::True => "true",
        Keyword::False => "false",
        Keyword::Null => "null",
        Keyword::Export => "export",
        Keyword::New => "new",
        Keyword::Unsupported(word) => word,
    }
}

fn unsupported_keyword(pos: Pos, word: &str) -> Diagnostic {
    let help = match word {
        "var" => "declare values with `const`, or `let` when they change",
        "function" => "use an arrow function, for example `file => agent(...)`",
        "class" => "workflow scripts hold data in objects and arrays only",
        "try" | "catch" | "finally" | "throw" => {
            "an agent that fails returns null; test for it, for example `results.filter(Boolean)`"
        }
        "switch" | "case" => "use `if` and `else if`",
        "do" => "use `while (condition) { ... }`",
        "import" | "require" => {
            "a workflow script loads no modules; put work that needs a library into an agent's task"
        }
        "typeof" | "instanceof" => {
            "test the shape directly, for example `Array.isArray(value)` or `value === null`"
        }
        "in" => "use `Object.keys(value).includes(name)`",
        "delete" => "build a new object with the fields you want",
        "async" => "the whole script is already asynchronous; use `await` directly",
        "undefined" => "use `null`, which is the only empty value in a workflow script",
        _ => "this keyword is not part of the workflow script subset",
    };
    Diagnostic::new(pos.line, pos.column, format!("`{word}` is not supported")).with_help(help)
}
