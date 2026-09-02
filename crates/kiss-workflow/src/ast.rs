//! The parsed script, stored as a flat arena.
//!
//! Nodes live in vectors and refer to each other by index rather than through
//! boxes. This keeps them contiguous in memory, avoids one allocation per node,
//! and turns evaluation into a walk over integers.

use crate::lexer::Symbol;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ExprId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct StmtId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FnId(pub u32);

/// A half-open span into one of the arena's flat child lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Span {
    pub start: u32,
    pub len: u32,
}

impl Span {
    pub(crate) fn range(self) -> std::ops::Range<usize> {
        let start = self.start as usize;
        start..start + self.len as usize
    }
}

/// A source position, kept for every node so runtime errors can point at the
/// script line that produced them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Pos {
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnOp {
    Not,
    Negate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogOp {
    And,
    Or,
    NullCoalesce,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TemplatePart {
    Chunk(Arc<str>),
    Expr(ExprId),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Field {
    pub name: Symbol,
    pub value: ExprId,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Null,
    Bool(bool),
    Number(f64),
    Text(Arc<str>),
    Template(Span),
    Name(Symbol),
    Array(Span),
    Object(Span),
    Member {
        object: ExprId,
        name: Symbol,
    },
    Index {
        object: ExprId,
        index: ExprId,
    },
    Call {
        callee: ExprId,
        args: Span,
    },
    Arrow(FnId),
    Await(ExprId),
    Unary {
        op: UnOp,
        operand: ExprId,
    },
    Binary {
        op: BinOp,
        left: ExprId,
        right: ExprId,
    },
    Logical {
        op: LogOp,
        left: ExprId,
        right: ExprId,
    },
    Conditional {
        test: ExprId,
        consequent: ExprId,
        alternate: ExprId,
    },
    /// `new X(...)`. Parsed so that the evaluator can explain why constructors
    /// are unavailable, naming `new Date()` in particular.
    New {
        callee: ExprId,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stmt {
    Declare {
        name: Symbol,
        value: ExprId,
    },
    Assign {
        target: ExprId,
        value: ExprId,
    },
    Expr(ExprId),
    If {
        test: ExprId,
        consequent: Span,
        alternate: Span,
    },
    ForOf {
        name: Symbol,
        iterable: ExprId,
        body: Span,
    },
    While {
        test: ExprId,
        body: Span,
    },
    Return(Option<ExprId>),
    Break,
    Continue,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FnBody {
    /// `x => expression`
    Expr(ExprId),
    /// `x => { statements }`
    Block(Span),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FnDef {
    pub params: Vec<Symbol>,
    pub body: FnBody,
}

/// Every node in one script.
#[derive(Debug, Default)]
pub(crate) struct Ast {
    pub exprs: Vec<Expr>,
    pub expr_pos: Vec<Pos>,
    pub stmts: Vec<Stmt>,
    pub stmt_pos: Vec<Pos>,
    /// Backing storage for array elements and call arguments.
    pub expr_list: Vec<ExprId>,
    /// Backing storage for statement blocks, including the script body.
    pub stmt_list: Vec<StmtId>,
    pub fields: Vec<Field>,
    pub template_parts: Vec<TemplatePart>,
    pub functions: Vec<FnDef>,
}

impl Ast {
    pub(crate) fn push_expr(&mut self, expr: Expr, pos: Pos) -> ExprId {
        let id = ExprId(self.exprs.len() as u32);
        self.exprs.push(expr);
        self.expr_pos.push(pos);
        id
    }

    pub(crate) fn push_stmt(&mut self, stmt: Stmt, pos: Pos) -> StmtId {
        let id = StmtId(self.stmts.len() as u32);
        self.stmts.push(stmt);
        self.stmt_pos.push(pos);
        id
    }

    pub(crate) fn expr(&self, id: ExprId) -> &Expr {
        self.exprs.get(id.0 as usize).unwrap_or(&Expr::Null)
    }

    pub(crate) fn expr_position(&self, id: ExprId) -> Pos {
        self.expr_pos
            .get(id.0 as usize)
            .copied()
            .unwrap_or_default()
    }

    pub(crate) fn stmt(&self, id: StmtId) -> &Stmt {
        self.stmts.get(id.0 as usize).unwrap_or(&Stmt::Break)
    }

    pub(crate) fn stmt_position(&self, id: StmtId) -> Pos {
        self.stmt_pos
            .get(id.0 as usize)
            .copied()
            .unwrap_or_default()
    }

    pub(crate) fn function(&self, id: FnId) -> Option<&FnDef> {
        self.functions.get(id.0 as usize)
    }

    pub(crate) fn exprs_in(&self, span: Span) -> &[ExprId] {
        self.expr_list.get(span.range()).unwrap_or_default()
    }

    pub(crate) fn stmts_in(&self, span: Span) -> &[StmtId] {
        self.stmt_list.get(span.range()).unwrap_or_default()
    }

    pub(crate) fn fields_in(&self, span: Span) -> &[Field] {
        self.fields.get(span.range()).unwrap_or_default()
    }

    pub(crate) fn parts_in(&self, span: Span) -> &[TemplatePart] {
        self.template_parts.get(span.range()).unwrap_or_default()
    }

    /// Append child ids to a flat list and return the span covering them.
    pub(crate) fn add_exprs(&mut self, items: &[ExprId]) -> Span {
        let start = self.expr_list.len() as u32;
        self.expr_list.extend_from_slice(items);
        Span {
            start,
            len: items.len() as u32,
        }
    }

    pub(crate) fn add_stmts(&mut self, items: &[StmtId]) -> Span {
        let start = self.stmt_list.len() as u32;
        self.stmt_list.extend_from_slice(items);
        Span {
            start,
            len: items.len() as u32,
        }
    }

    pub(crate) fn add_fields(&mut self, items: &[Field]) -> Span {
        let start = self.fields.len() as u32;
        self.fields.extend_from_slice(items);
        Span {
            start,
            len: items.len() as u32,
        }
    }

    pub(crate) fn add_parts(&mut self, items: Vec<TemplatePart>) -> Span {
        let start = self.template_parts.len() as u32;
        let len = items.len() as u32;
        self.template_parts.extend(items);
        Span { start, len }
    }

    pub(crate) fn add_function(&mut self, def: FnDef) -> FnId {
        let id = FnId(self.functions.len() as u32);
        self.functions.push(def);
        id
    }
}
