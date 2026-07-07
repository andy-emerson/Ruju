//! A minimal Julia front-end: a Rust lexer, parser, and lowering that turn a
//! subset of Julia source into the interpreter's lowered IR.
//!
//! This is deliberately **not** JuliaSyntax/JuliaLowering — those are Julia
//! packages that require a running Julia, which is the very thing being built.
//! Until the runtime can host them (AOT-compiled, much later), this hand-written
//! Rust front-end lets real Julia source execute. It covers integer and float
//! literals, variables, assignment, arithmetic (`+ - * / ÷ %`), bitwise ops
//! (`& | << >> >>>`), comparisons (incl. `===`), `if`/`elseif`/`else`, and
//! `while`. `/` always yields `Float64`, as in Julia.

use std::collections::HashMap;

use crate::interp::{self, Body, Builtin, Op, Stmt};
use crate::object::Value;

// --- lexer ------------------------------------------------------------------

#[derive(Clone, PartialEq, Debug)]
enum Tok {
    Int(i64),
    Float(f64),
    Ident(String),
    Plus,
    Minus,
    Star,
    Slash,
    IDiv, // ÷
    Percent,
    Amp,
    Pipe,
    Veebar, // ⊻ (xor)
    Shl,  // <<
    Shr,  // >>
    Ushr, // >>>
    Lt,
    Le,
    Gt,
    Ge,
    EqEq,
    EqEqEq,
    Assign,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    Comma,
    ColonColon,
    Sep, // newline or `;`
    If,
    Struct,
    Mutable,
    Else,
    Elseif,
    End,
    While,
    Try,
    Catch,
    Eof,
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\r' => i += 1,
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'\n' | b';' => {
                out.push(Tok::Sep);
                i += 1;
            }
            b'+' => {
                out.push(Tok::Plus);
                i += 1;
            }
            b'-' => {
                out.push(Tok::Minus);
                i += 1;
            }
            b'*' => {
                out.push(Tok::Star);
                i += 1;
            }
            b'/' => {
                out.push(Tok::Slash);
                i += 1;
            }
            b'%' => {
                out.push(Tok::Percent);
                i += 1;
            }
            b'&' => {
                out.push(Tok::Amp);
                i += 1;
            }
            b'|' => {
                out.push(Tok::Pipe);
                i += 1;
            }
            0xC3 if b.get(i + 1) == Some(&0xB7) => {
                out.push(Tok::IDiv); // ÷ (U+00F7)
                i += 2;
            }
            0xE2 if b.get(i + 1) == Some(&0x8A) && b.get(i + 2) == Some(&0xBB) => {
                out.push(Tok::Veebar); // ⊻ (U+22BB, xor)
                i += 3;
            }
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b'.' => {
                out.push(Tok::Dot);
                i += 1;
            }
            b',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            b':' if b.get(i + 1) == Some(&b':') => {
                out.push(Tok::ColonColon);
                i += 2;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b'[' => {
                out.push(Tok::LBracket);
                i += 1;
            }
            b']' => {
                out.push(Tok::RBracket);
                i += 1;
            }
            b'<' => {
                if b.get(i + 1) == Some(&b'<') {
                    out.push(Tok::Shl);
                    i += 2;
                } else if b.get(i + 1) == Some(&b'=') {
                    out.push(Tok::Le);
                    i += 2;
                } else {
                    out.push(Tok::Lt);
                    i += 1;
                }
            }
            b'>' => {
                if b.get(i + 1) == Some(&b'>') && b.get(i + 2) == Some(&b'>') {
                    out.push(Tok::Ushr);
                    i += 3;
                } else if b.get(i + 1) == Some(&b'>') {
                    out.push(Tok::Shr);
                    i += 2;
                } else if b.get(i + 1) == Some(&b'=') {
                    out.push(Tok::Ge);
                    i += 2;
                } else {
                    out.push(Tok::Gt);
                    i += 1;
                }
            }
            b'=' => {
                if b.get(i + 1) == Some(&b'=') && b.get(i + 2) == Some(&b'=') {
                    out.push(Tok::EqEqEq);
                    i += 3;
                } else if b.get(i + 1) == Some(&b'=') {
                    out.push(Tok::EqEq);
                    i += 2;
                } else {
                    out.push(Tok::Assign);
                    i += 1;
                }
            }
            b'0'..=b'9' => {
                let s = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                // A `.` followed by a digit makes it a float literal.
                if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                    let f = src[s..i].parse().map_err(|_| "invalid float".to_string())?;
                    out.push(Tok::Float(f));
                } else {
                    let n = src[s..i].parse().map_err(|_| "invalid integer".to_string())?;
                    out.push(Tok::Int(n));
                }
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let s = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                // Julia identifiers may end in `!` (mutating convention: push!).
                while i < b.len() && b[i] == b'!' {
                    i += 1;
                }
                out.push(match &src[s..i] {
                    "if" => Tok::If,
                    "else" => Tok::Else,
                    "elseif" => Tok::Elseif,
                    "end" => Tok::End,
                    "while" => Tok::While,
                    "try" => Tok::Try,
                    "catch" => Tok::Catch,
                    "struct" => Tok::Struct,
                    "mutable" => Tok::Mutable,
                    w => Tok::Ident(w.to_string()),
                });
            }
            _ => return Err(format!("unexpected character '{}'", c as char)),
        }
    }
    out.push(Tok::Eof);
    Ok(out)
}

// --- AST --------------------------------------------------------------------

#[derive(Clone, Copy)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Rem,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    Ushr,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Egal,
}

enum Expr {
    Int(i64),
    Float(f64),
    Var(String),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    /// `Name(args...)` — a struct constructor call.
    Call(String, Vec<Expr>),
    /// `base.field` — field access.
    Field(Box<Expr>, String),
    /// `[e1, e2, ...]` — a 1-D array literal.
    ArrayLit(Vec<Expr>),
    /// `base[index]` — 1-based indexing.
    Index(Box<Expr>, Box<Expr>),
}

enum SrcStmt {
    Assign(String, Expr),
    /// `var.field = expr` (`setfield!`).
    FieldAssign(String, String, Expr),
    Expr(Expr),
    If(Expr, Vec<SrcStmt>, Vec<SrcStmt>),
    While(Expr, Vec<SrcStmt>),
    /// `try <body> catch [e] <handler> end`; the optional name binds the caught
    /// exception. `finally` is a later slice.
    Try(Vec<SrcStmt>, Option<String>, Vec<SrcStmt>),
    /// `var[index] = expr` (1-based `setindex!`).
    IndexAssign(String, Expr, Expr),
    /// `[mutable] struct Name; field[::Type]...; end`.
    StructDef { name: String, mutabl: bool, fields: Vec<(String, Option<String>)> },
}

// --- parser -----------------------------------------------------------------

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos].clone();
        self.pos += 1;
        t
    }

    fn expect(&mut self, t: &Tok) -> Result<(), String> {
        if self.peek() == t {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected {:?}, found {:?}", t, self.peek()))
        }
    }

    fn skip_seps(&mut self) {
        while *self.peek() == Tok::Sep {
            self.pos += 1;
        }
    }

    fn parse_program(&mut self) -> Result<Vec<SrcStmt>, String> {
        let block = self.parse_block()?;
        if *self.peek() != Tok::Eof {
            return Err(format!("unexpected {:?}", self.peek()));
        }
        Ok(block)
    }

    /// Parse statements up to a block terminator (`end`/`else`/`elseif`/EOF).
    fn parse_block(&mut self) -> Result<Vec<SrcStmt>, String> {
        let mut out = Vec::new();
        loop {
            self.skip_seps();
            match self.peek() {
                Tok::End | Tok::Else | Tok::Elseif | Tok::Catch | Tok::Eof => break,
                _ => out.push(self.parse_stmt()?),
            }
        }
        Ok(out)
    }

    fn parse_stmt(&mut self) -> Result<SrcStmt, String> {
        match self.peek().clone() {
            Tok::Struct => {
                self.next();
                self.parse_struct(false)
            }
            Tok::Mutable => {
                self.next();
                self.expect(&Tok::Struct)?;
                self.parse_struct(true)
            }
            // `var.field = expr` — setfield! on a variable's field.
            Tok::Ident(name)
                if self.toks.get(self.pos + 1) == Some(&Tok::Dot)
                    && matches!(self.toks.get(self.pos + 2), Some(Tok::Ident(_)))
                    && self.toks.get(self.pos + 3) == Some(&Tok::Assign) =>
            {
                self.next(); // var
                self.next(); // `.`
                let field = match self.next() {
                    Tok::Ident(f) => f,
                    _ => unreachable!(),
                };
                self.next(); // `=`
                Ok(SrcStmt::FieldAssign(name, field, self.parse_expr()?))
            }
            Tok::While => {
                self.next();
                let cond = self.parse_expr()?;
                let body = self.parse_block()?;
                self.expect(&Tok::End)?;
                Ok(SrcStmt::While(cond, body))
            }
            Tok::If => {
                self.next();
                self.parse_if()
            }
            Tok::Try => {
                self.next();
                let body = self.parse_block()?;
                self.expect(&Tok::Catch)?;
                // `catch e` — an identifier before the newline names the exception.
                let var = match self.peek().clone() {
                    Tok::Ident(name) => {
                        self.next();
                        Some(name)
                    }
                    _ => None,
                };
                let handler = self.parse_block()?;
                self.expect(&Tok::End)?;
                Ok(SrcStmt::Try(body, var, handler))
            }
            Tok::Ident(name) if self.toks.get(self.pos + 1) == Some(&Tok::Assign) => {
                self.next(); // identifier
                self.next(); // `=`
                Ok(SrcStmt::Assign(name, self.parse_expr()?))
            }
            // `var[index] = expr` — tried speculatively: if no `=` follows the
            // closing `]`, rewind and let the expression path have it.
            Tok::Ident(name) if self.toks.get(self.pos + 1) == Some(&Tok::LBracket) => {
                let save = self.pos;
                self.next(); // identifier
                self.next(); // `[`
                let idx = self.parse_expr()?;
                if *self.peek() == Tok::RBracket
                    && self.toks.get(self.pos + 1) == Some(&Tok::Assign)
                {
                    self.next(); // `]`
                    self.next(); // `=`
                    return Ok(SrcStmt::IndexAssign(name, idx, self.parse_expr()?));
                }
                self.pos = save;
                Ok(SrcStmt::Expr(self.parse_expr()?))
            }
            _ => Ok(SrcStmt::Expr(self.parse_expr()?)),
        }
    }

    /// Parse the body of an `if`/`elseif` (the keyword is already consumed).
    fn parse_if(&mut self) -> Result<SrcStmt, String> {
        let cond = self.parse_expr()?;
        let then = self.parse_block()?;
        let els = match self.peek() {
            Tok::End => {
                self.next();
                Vec::new()
            }
            Tok::Else => {
                self.next();
                let e = self.parse_block()?;
                self.expect(&Tok::End)?;
                e
            }
            Tok::Elseif => {
                self.next();
                vec![self.parse_if()?] // the nested if consumes the shared `end`
            }
            other => return Err(format!("expected else/elseif/end, found {:?}", other)),
        };
        Ok(SrcStmt::If(cond, then, els))
    }

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_cmp()
    }

    fn parse_cmp(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_add()?;
        loop {
            let op = match self.peek() {
                Tok::Lt => BinOp::Lt,
                Tok::Le => BinOp::Le,
                Tok::Gt => BinOp::Gt,
                Tok::Ge => BinOp::Ge,
                Tok::EqEq => BinOp::Eq,
                Tok::EqEqEq => BinOp::Egal,
                _ => break,
            };
            self.next();
            let rhs = self.parse_add()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    // Julia's precedence: `|` sits at the additive level and `&` at the
    // multiplicative level; shifts bind tighter than `*`.
    fn parse_add(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                Tok::Pipe => BinOp::Or,
                Tok::Veebar => BinOp::Xor,
                _ => break,
            };
            self.next();
            let rhs = self.parse_mul()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_shift()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::IDiv => BinOp::IDiv,
                Tok::Percent => BinOp::Rem,
                Tok::Amp => BinOp::And,
                _ => break,
            };
            self.next();
            let rhs = self.parse_shift()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_shift(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_atom()?;
        loop {
            let op = match self.peek() {
                Tok::Shl => BinOp::Shl,
                Tok::Shr => BinOp::Shr,
                Tok::Ushr => BinOp::Ushr,
                _ => break,
            };
            self.next();
            let rhs = self.parse_atom()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// An atom with its postfix forms: `Name(args...)` and `.field` chains.
    fn parse_atom(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_primary()?;
        loop {
            match self.peek() {
                Tok::Dot => {
                    self.next();
                    match self.next() {
                        Tok::Ident(f) => e = Expr::Field(Box::new(e), f),
                        t => return Err(format!("expected field name after `.`, found {:?}", t)),
                    }
                }
                Tok::LBracket => {
                    self.next();
                    let idx = self.parse_expr()?;
                    self.expect(&Tok::RBracket)?;
                    e = Expr::Index(Box::new(e), Box::new(idx));
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Tok::Int(n) => Ok(Expr::Int(n)),
            Tok::Float(f) => Ok(Expr::Float(f)),
            Tok::Ident(s) => {
                if *self.peek() == Tok::LParen {
                    self.next(); // `(`
                    let mut args = Vec::new();
                    if *self.peek() == Tok::RParen {
                        self.next();
                    } else {
                        loop {
                            args.push(self.parse_expr()?);
                            match self.next() {
                                Tok::Comma => continue,
                                Tok::RParen => break,
                                t => return Err(format!("expected `,` or `)`, found {:?}", t)),
                            }
                        }
                    }
                    Ok(Expr::Call(s, args))
                } else {
                    Ok(Expr::Var(s))
                }
            }
            Tok::LParen => {
                let e = self.parse_expr()?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Tok::LBracket => {
                let mut elems = Vec::new();
                if *self.peek() == Tok::RBracket {
                    self.next();
                } else {
                    loop {
                        elems.push(self.parse_expr()?);
                        match self.next() {
                            Tok::Comma => continue,
                            Tok::RBracket => break,
                            t => return Err(format!("expected `,` or `]`, found {:?}", t)),
                        }
                    }
                }
                Ok(Expr::ArrayLit(elems))
            }
            Tok::Minus => Ok(Expr::Bin(BinOp::Sub, Box::new(Expr::Int(0)), Box::new(self.parse_atom()?))),
            t => Err(format!("unexpected token {:?}", t)),
        }
    }

    /// Field declarations up to `end`; `struct`/`mutable struct` is consumed.
    fn parse_struct(&mut self, mutabl: bool) -> Result<SrcStmt, String> {
        let name = match self.next() {
            Tok::Ident(s) => s,
            t => return Err(format!("expected struct name, found {:?}", t)),
        };
        let mut fields = Vec::new();
        loop {
            self.skip_seps();
            match self.peek().clone() {
                Tok::End => {
                    self.next();
                    break;
                }
                Tok::Ident(fname) => {
                    self.next();
                    let fty = if *self.peek() == Tok::ColonColon {
                        self.next();
                        match self.next() {
                            Tok::Ident(tn) => Some(tn),
                            t => return Err(format!("expected field type, found {:?}", t)),
                        }
                    } else {
                        None
                    };
                    fields.push((fname, fty));
                }
                t => return Err(format!("expected field or `end` in struct, found {:?}", t)),
            }
        }
        Ok(SrcStmt::StructDef { name, mutabl, fields })
    }
}

// --- lowering to interpreter IR ---------------------------------------------

fn binop(op: BinOp) -> (Builtin, bool) {
    match op {
        BinOp::Add => (Builtin::Add, false),
        BinOp::Sub => (Builtin::Sub, false),
        BinOp::Mul => (Builtin::Mul, false),
        BinOp::Div => (Builtin::Div, false),
        BinOp::IDiv => (Builtin::IDiv, false),
        BinOp::Rem => (Builtin::Rem, false),
        BinOp::And => (Builtin::And, false),
        BinOp::Or => (Builtin::Or, false),
        BinOp::Xor => (Builtin::Xor, false),
        BinOp::Shl => (Builtin::Shl, false),
        BinOp::Shr => (Builtin::Shr, false),
        BinOp::Ushr => (Builtin::Lshr, false),
        BinOp::Lt => (Builtin::Slt, false),
        BinOp::Le => (Builtin::Sle, false),
        BinOp::Gt => (Builtin::Slt, true), // a > b  ==  b < a
        BinOp::Ge => (Builtin::Sle, true), // a >= b ==  b <= a
        BinOp::Eq => (Builtin::Eq, false),
        BinOp::Egal => (Builtin::Egal, false),
    }
}

struct Lower {
    code: Vec<Stmt>,
    slots: HashMap<String, usize>,
    nslots: usize,
}

impl Lower {
    fn slot(&mut self, name: &str) -> usize {
        if let Some(&s) = self.slots.get(name) {
            return s;
        }
        let s = self.nslots;
        self.nslots += 1;
        self.slots.insert(name.to_string(), s);
        s
    }

    fn emit(&mut self, s: Stmt) -> usize {
        self.code.push(s);
        self.code.len() - 1
    }

    fn lower_expr(&mut self, e: &Expr) -> Result<Op, String> {
        Ok(match e {
            Expr::Int(n) => Op::Int(*n),
            Expr::Float(f) => Op::Float(*f),
            Expr::Var(s) => Op::Slot(self.slot(s)),
            Expr::Bin(op, l, r) => {
                let lo = self.lower_expr(l)?;
                let ro = self.lower_expr(r)?;
                let (b, swap) = binop(*op);
                let (a0, a1) = if swap { (ro, lo) } else { (lo, ro) };
                Op::Ssa(self.emit(Stmt::Call(b, vec![a0, a1])))
            }
            Expr::Call(name, args) if name == "throw" && args.len() == 1 => {
                let v = self.lower_expr(&args[0])?;
                Op::Ssa(self.emit(Stmt::Throw(v)))
            }
            Expr::Call(name, args) if name == "push!" && args.len() == 2 => {
                let a = self.lower_expr(&args[0])?;
                let v = self.lower_expr(&args[1])?;
                Op::Ssa(self.emit(Stmt::Push(a, v)))
            }
            Expr::Call(name, args) if name == "length" && args.len() == 1 => {
                let a = self.lower_expr(&args[0])?;
                Op::Ssa(self.emit(Stmt::Len(a)))
            }
            Expr::Call(name, args) => {
                let t = resolve_type(name)?;
                let ops = args.iter().map(|a| self.lower_expr(a)).collect::<Result<Vec<_>, _>>()?;
                Op::Ssa(self.emit(Stmt::New(t, ops)))
            }
            Expr::Field(base, fname) => {
                let b = self.lower_expr(base)?;
                let sym = crate::symbol::intern(crate::types::builtin(crate::types::id::SYMBOL), fname);
                Op::Ssa(self.emit(Stmt::GetField(b, sym)))
            }
            Expr::ArrayLit(elems) => {
                let ops = elems.iter().map(|e| self.lower_expr(e)).collect::<Result<Vec<_>, _>>()?;
                Op::Ssa(self.emit(Stmt::ArrayLit(ops)))
            }
            Expr::Index(base, idx) => {
                let b = self.lower_expr(base)?;
                let i = self.lower_expr(idx)?;
                Op::Ssa(self.emit(Stmt::ArrayRef(b, i)))
            }
        })
    }

    fn lower_block(&mut self, stmts: &[SrcStmt]) -> Result<Option<Op>, String> {
        let mut last = None;
        for s in stmts {
            last = self.lower_stmt(s)?;
        }
        Ok(last)
    }

    fn lower_stmt(&mut self, s: &SrcStmt) -> Result<Option<Op>, String> {
        Ok(match s {
            SrcStmt::Assign(name, e) => {
                let op = self.lower_expr(e)?;
                let slot = self.slot(name);
                self.emit(Stmt::Assign(slot, op));
                Some(Op::Slot(slot))
            }
            SrcStmt::FieldAssign(var, field, e) => {
                let rhs = self.lower_expr(e)?;
                let obj = Op::Slot(self.slot(var));
                let sym = crate::symbol::intern(crate::types::builtin(crate::types::id::SYMBOL), field);
                self.emit(Stmt::SetField(obj, sym, rhs));
                Some(rhs)
            }
            SrcStmt::IndexAssign(var, idx, e) => {
                let i = self.lower_expr(idx)?;
                let rhs = self.lower_expr(e)?;
                let a = Op::Slot(self.slot(var));
                self.emit(Stmt::ArraySet(a, i, rhs));
                Some(rhs)
            }
            SrcStmt::Expr(e) => Some(self.lower_expr(e)?),
            SrcStmt::If(cond, then, els) => {
                let c = self.lower_expr(cond)?;
                let gif = self.emit(Stmt::GotoIfNot(c, 0));
                self.lower_block(then)?;
                let gend = self.emit(Stmt::Goto(0));
                let else_start = self.code.len();
                self.lower_block(els)?;
                let end = self.code.len();
                self.patch(gif, else_start);
                self.patch(gend, end);
                None
            }
            SrcStmt::While(cond, body) => {
                let start = self.code.len();
                let c = self.lower_expr(cond)?;
                let gif = self.emit(Stmt::GotoIfNot(c, 0));
                self.lower_block(body)?;
                self.emit(Stmt::Goto(start));
                let end = self.code.len();
                self.patch(gif, end);
                None
            }
            SrcStmt::Try(body, var, handler) => {
                // Enter pushes the handler; on normal completion Leave pops it and
                // Goto skips the catch block. A throw inside the body diverts to
                // `catch_start` (patched below), where `catch e` binds the
                // exception before the handler runs.
                let enter = self.emit(Stmt::Enter(0));
                self.lower_block(body)?;
                self.emit(Stmt::Leave(1));
                let gend = self.emit(Stmt::Goto(0));
                let catch_start = self.code.len();
                self.patch(enter, catch_start);
                if let Some(name) = var {
                    let c = self.emit(Stmt::Caught);
                    let slot = self.slot(name);
                    self.emit(Stmt::Assign(slot, Op::Ssa(c)));
                }
                self.lower_block(handler)?;
                let end = self.code.len();
                self.patch(gend, end);
                None
            }
            // A struct definition is a lowering-time side effect (a top-level
            // form); it contributes no IR and its value is not an expression.
            SrcStmt::StructDef { name, mutabl, fields } => {
                let resolved = fields
                    .iter()
                    .map(|(fname, ftyname)| {
                        let ft = match ftyname {
                            Some(tn) => resolve_type(tn)?,
                            None => crate::types::builtin(crate::types::id::ANY),
                        };
                        Ok((fname.as_str(), ft))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                crate::types::define_struct_from_source(name, &resolved, *mutabl)?;
                None
            }
        })
    }

    fn patch(&mut self, idx: usize, target: usize) {
        match &mut self.code[idx] {
            Stmt::Goto(t) | Stmt::GotoIfNot(_, t) | Stmt::Enter(t) => *t = target,
            _ => {}
        }
    }
}

fn lower_program(stmts: &[SrcStmt]) -> Result<Body, String> {
    let mut l = Lower {
        code: Vec::new(),
        slots: HashMap::new(),
        nslots: 0,
    };
    let last = l.lower_block(stmts)?;
    let ret = last.unwrap_or(Op::Int(0));
    l.emit(Stmt::Return(ret));
    Ok(Body {
        nslots: l.nslots,
        code: l.code,
    })
}

/// Resolve a type name from source: the builtin tower by name, then the
/// source-defined struct registry.
fn resolve_type(name: &str) -> Result<crate::region::Offset, String> {
    use crate::types::id as tid;
    let i = match name {
        "Any" => tid::ANY,
        "Number" => tid::NUMBER,
        "Real" => tid::REAL,
        "Integer" => tid::INTEGER,
        "Signed" => tid::SIGNED,
        "Unsigned" => tid::UNSIGNED,
        "AbstractFloat" => tid::ABSTRACTFLOAT,
        "Bool" => tid::BOOL,
        "Int8" => tid::INT8,
        "Int16" => tid::INT16,
        "Int32" => tid::INT32,
        "Int64" | "Int" => tid::INT64,
        "UInt8" => tid::UINT8,
        "UInt16" => tid::UINT16,
        "UInt32" => tid::UINT32,
        "UInt64" => tid::UINT64,
        "Float32" => tid::FLOAT32,
        "Float64" => tid::FLOAT64,
        "Char" => tid::CHAR,
        "Nothing" => tid::NOTHING,
        _ => {
            let sym = crate::symbol::intern(crate::types::builtin(tid::SYMBOL), name);
            return crate::types::lookup_struct(sym)
                .ok_or_else(|| format!("UndefVarError: `{}` not defined", name));
        }
    };
    Ok(crate::types::builtin(i))
}

/// Parse, lower, and evaluate a Julia source string, returning its value.
pub fn eval_source(src: &str) -> Result<Value, String> {
    let toks = lex(src)?;
    let mut parser = Parser { toks, pos: 0 };
    let program = parser.parse_program()?;
    interp::eval(&lower_program(&program)?)
}
