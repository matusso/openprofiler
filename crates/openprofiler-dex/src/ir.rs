//! Intermediate representation of a method body: expressions over recovered variables, statements,
//! and basic-block terminators.

use std::sync::Arc;

pub type VarId = u32;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldRef {
    pub class: Arc<str>,
    pub name: Arc<str>,
    pub ty: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MethodRef {
    pub class: Arc<str>,
    pub name: Arc<str>,
    pub params: Arc<[String]>,
    pub ret: Arc<str>,
}

impl MethodRef {
    pub fn desc(&self) -> String {
        format!("({}){}", self.params.concat(), self.ret)
    }
}

/// A lambda or method reference created by `invokedynamic` through `LambdaMetafactory`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Lambda {
    /// Functional interface descriptor.
    pub iface: Arc<str>,
    /// Name of the interface method the lambda implements.
    pub method: Arc<str>,
    /// Parameter descriptors of that method (as instantiated at the call site).
    pub params: Arc<[String]>,
    /// Its return descriptor, as instantiated.
    pub ret: Arc<str>,
    /// The implementation method (a synthetic `lambda$...` method or a referenced one).
    pub target: MethodRef,
    /// Method handle kind of the target (JVMS §4.4.8): 5 virtual, 6 static, 7 special, 8 new,
    /// 9 interface.
    pub kind: u8,
}

impl Lambda {
    pub const NEW: u8 = 8;
    pub const STATIC: u8 = 6;

    /// Whether the target takes a receiver (the first captured value or lambda parameter).
    pub fn has_receiver(&self) -> bool {
        matches!(self.kind, 5 | 7 | 9)
    }
}

/// A resolved `invokedynamic` call site.
#[derive(Debug, Clone, PartialEq)]
pub enum CallSite {
    Lambda(Arc<Lambda>),
    /// Any other bootstrap method: printed as a call of `name`.
    Dynamic {
        name: Arc<str>,
        /// Descriptor of the class declaring the bootstrap method.
        owner: Arc<str>,
        bootstrap: Arc<str>,
        params: Arc<[String]>,
        ret: Arc<str>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokeKind {
    Virtual,
    Super,
    Direct,
    Static,
    Interface,
    Polymorphic,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    Ushr,
    Eq,
    Ne,
    Lt,
    Ge,
    Gt,
    Le,
    LogAnd,
    LogOr,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        use BinOp::*;
        match self {
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            Rem => "%",
            And => "&",
            Or => "|",
            Xor => "^",
            Shl => "<<",
            Shr => ">>",
            Ushr => ">>>",
            Eq => "==",
            Ne => "!=",
            Lt => "<",
            Ge => ">=",
            Gt => ">",
            Le => "<=",
            LogAnd => "&&",
            LogOr => "||",
        }
    }

    /// Java precedence (higher binds tighter).
    pub fn precedence(self) -> u8 {
        use BinOp::*;
        match self {
            Mul | Div | Rem => 12,
            Add | Sub => 11,
            Shl | Shr | Ushr => 10,
            Lt | Ge | Gt | Le => 9,
            Eq | Ne => 8,
            And => 7,
            Xor => 6,
            Or => 5,
            LogAnd => 4,
            LogOr => 3,
        }
    }

    pub fn is_compare(self) -> bool {
        use BinOp::*;
        matches!(self, Eq | Ne | Lt | Ge | Gt | Le)
    }

    pub fn negate(self) -> Option<BinOp> {
        use BinOp::*;
        Some(match self {
            Eq => Ne,
            Ne => Eq,
            Lt => Ge,
            Ge => Lt,
            Gt => Le,
            Le => Gt,
            _ => return None,
        })
    }

    /// The operator with swapped operands (`a < b` ⇔ `b > a`).
    pub fn swap(self) -> BinOp {
        use BinOp::*;
        match self {
            Lt => Gt,
            Gt => Lt,
            Le => Ge,
            Ge => Le,
            o => o,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Var(VarId),
    /// Integer/float bits; the printed form depends on the type expected at the use site.
    Lit {
        bits: i64,
        wide: bool,
    },
    Str(Arc<str>),
    Class(Arc<str>),
    /// Raw text for constructs without a Java equivalent (method handles, call sites).
    Raw(String),
    /// `type` is the operand type (`I`, `J`, `F`, `D`) when known from the opcode.
    Bin(BinOp, Box<Expr>, Box<Expr>, Option<&'static str>),
    Neg(Box<Expr>),
    /// Bitwise `~` (`not-int`, `not-long`).
    BitNot(Box<Expr>),
    /// Logical `!` on a boolean.
    Not(Box<Expr>),
    /// Primitive conversion or reference cast to the descriptor.
    Cast(Arc<str>, Box<Expr>),
    InstanceOf(Box<Expr>, Arc<str>),
    ArrayLen(Box<Expr>),
    /// `elem` is the element kind from the opcode (`I`/`F` ambiguous for plain aget).
    ArrayGet(Box<Expr>, Box<Expr>, &'static str),
    IGet(Box<Expr>, FieldRef),
    SGet(FieldRef),
    /// Arguments include the receiver for non-static calls.
    Invoke(InvokeKind, MethodRef, Vec<Expr>),
    New(MethodRef, Vec<Expr>),
    NewArray(Arc<str>, Box<Expr>),
    /// `new T[]{...}`; descriptor is the array type.
    ArrayLit(Arc<str>, Vec<Expr>),
    /// `cmp-long` / `cmpl-float` / ... (the three-way result).
    Cmp(&'static str, Box<Expr>, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// The caught exception at a handler entry.
    CaughtException(Option<Arc<str>>),
    /// A lambda or method reference with its captured values.
    Lambda(Arc<Lambda>, Vec<Expr>),
}

impl Expr {
    pub fn lit(bits: i64) -> Expr {
        Expr::Lit { bits, wide: false }
    }

    pub fn bin(op: BinOp, a: Expr, b: Expr) -> Expr {
        Expr::Bin(op, Box::new(a), Box::new(b), None)
    }

    /// Visits every variable read, in evaluation order.
    pub fn visit_vars(&self, f: &mut dyn FnMut(VarId)) {
        self.visit(&mut |e| {
            if let Expr::Var(v) = e {
                f(*v)
            }
        });
    }

    /// Pre-order visit, operands in evaluation order.
    pub fn visit(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Bin(_, a, b, _) | Expr::ArrayGet(a, b, _) | Expr::Cmp(_, a, b) => {
                a.visit(f);
                b.visit(f);
            }
            Expr::Neg(a)
            | Expr::BitNot(a)
            | Expr::Not(a)
            | Expr::Cast(_, a)
            | Expr::InstanceOf(a, _)
            | Expr::ArrayLen(a)
            | Expr::IGet(a, _)
            | Expr::NewArray(_, a) => a.visit(f),
            Expr::Invoke(_, _, args)
            | Expr::New(_, args)
            | Expr::ArrayLit(_, args)
            | Expr::Lambda(_, args) => {
                for a in args {
                    a.visit(f);
                }
            }
            Expr::Ternary(c, a, b) => {
                c.visit(f);
                a.visit(f);
                b.visit(f);
            }
            _ => {}
        }
    }

    pub fn visit_mut(&mut self, f: &mut dyn FnMut(&mut Expr)) {
        f(self);
        match self {
            Expr::Bin(_, a, b, _) | Expr::ArrayGet(a, b, _) | Expr::Cmp(_, a, b) => {
                a.visit_mut(f);
                b.visit_mut(f);
            }
            Expr::Neg(a)
            | Expr::BitNot(a)
            | Expr::Not(a)
            | Expr::Cast(_, a)
            | Expr::InstanceOf(a, _)
            | Expr::ArrayLen(a)
            | Expr::IGet(a, _)
            | Expr::NewArray(_, a) => a.visit_mut(f),
            Expr::Invoke(_, _, args)
            | Expr::New(_, args)
            | Expr::ArrayLit(_, args)
            | Expr::Lambda(_, args) => {
                for a in args {
                    a.visit_mut(f);
                }
            }
            Expr::Ternary(c, a, b) => {
                c.visit_mut(f);
                a.visit_mut(f);
                b.visit_mut(f);
            }
            _ => {}
        }
    }

    /// No side effects and no memory reads, so it may move past any statement.
    pub fn is_pure(&self) -> bool {
        let mut pure = true;
        self.visit(&mut |e| {
            if matches!(
                e,
                Expr::IGet(..)
                    | Expr::SGet(_)
                    | Expr::ArrayGet(..)
                    | Expr::Invoke(..)
                    | Expr::New(..)
                    | Expr::NewArray(..)
                    | Expr::ArrayLit(..)
                    | Expr::CaughtException(_)
                    | Expr::Lambda(..)
                    | Expr::Bin(BinOp::Div | BinOp::Rem, ..)
            ) {
                pure = false;
            }
        });
        pure
    }

    /// Like [`Expr::is_pure`], but `StringBuilder` calls whose only effect is on the builder
    /// (appending strings or primitives) don't count: they may be reordered with other code.
    pub fn is_order_free(&self) -> bool {
        let mut free = true;
        self.visit(&mut |e| match e {
            Expr::Invoke(_, m, _) | Expr::New(m, _) if is_builder_call(m) => {}
            Expr::IGet(..)
            | Expr::SGet(_)
            | Expr::ArrayGet(..)
            | Expr::Invoke(..)
            | Expr::New(..)
            | Expr::NewArray(..)
            | Expr::ArrayLit(..)
            | Expr::CaughtException(_)
            | Expr::Lambda(..)
            | Expr::Bin(BinOp::Div | BinOp::Rem, ..) => free = false,
            _ => {}
        });
        free
    }

    /// Logical negation, simplifying comparisons and double negation.
    pub fn negate(self) -> Expr {
        match self {
            Expr::Not(e) => *e,
            Expr::Bin(op, a, b, t) if op.is_compare() && !matches!(t, Some("F" | "D")) => {
                Expr::Bin(op.negate().unwrap_or(op), a, b, t)
            }
            Expr::Bin(BinOp::LogAnd, a, b, t) => {
                Expr::Bin(BinOp::LogOr, Box::new(a.negate()), Box::new(b.negate()), t)
            }
            Expr::Bin(BinOp::LogOr, a, b, t) => {
                Expr::Bin(BinOp::LogAnd, Box::new(a.negate()), Box::new(b.negate()), t)
            }
            e => Expr::Not(Box::new(e)),
        }
    }
}

/// `StringBuilder` construction, `append` of a string or primitive, `toString`, `length`.
pub fn is_builder_call(m: &MethodRef) -> bool {
    &*m.class == "Ljava/lang/StringBuilder;"
        && match &*m.name {
            "<init>" => m.params.is_empty() || m.params[0] == "Ljava/lang/String;" || m.params[0] == "I",
            "append" => {
                m.params.len() == 1 && (m.params[0].len() == 1 || m.params[0] == "Ljava/lang/String;")
            }
            "toString" | "length" => m.params.is_empty(),
            _ => false,
        }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Assign(VarId, Expr),
    Expr(Expr),
    IPut(Expr, FieldRef, Expr),
    SPut(FieldRef, Expr),
    APut(Expr, Expr, Expr, &'static str),
    MonitorEnter(Expr),
    MonitorExit(Expr),
    /// Line number marker for the following statements (from debug info).
    Line(u32),
}

impl Stmt {
    pub fn visit_exprs(&self, f: &mut dyn FnMut(&Expr)) {
        match self {
            Stmt::Assign(_, e)
            | Stmt::Expr(e)
            | Stmt::SPut(_, e)
            | Stmt::MonitorEnter(e)
            | Stmt::MonitorExit(e) => f(e),
            Stmt::IPut(o, _, v) => {
                f(o);
                f(v);
            }
            Stmt::APut(a, i, v, _) => {
                f(a);
                f(i);
                f(v);
            }
            Stmt::Line(_) => {}
        }
    }

    pub fn visit_exprs_mut(&mut self, f: &mut dyn FnMut(&mut Expr)) {
        match self {
            Stmt::Assign(_, e)
            | Stmt::Expr(e)
            | Stmt::SPut(_, e)
            | Stmt::MonitorEnter(e)
            | Stmt::MonitorExit(e) => f(e),
            Stmt::IPut(o, _, v) => {
                f(o);
                f(v);
            }
            Stmt::APut(a, i, v, _) => {
                f(a);
                f(i);
                f(v);
            }
            Stmt::Line(_) => {}
        }
    }

    /// Whether moving other code across this statement could change behaviour.
    pub fn orders(&self) -> bool {
        match self {
            Stmt::Assign(_, e) | Stmt::Expr(e) => !e.is_order_free(),
            Stmt::Line(_) => false,
            _ => true,
        }
    }

    pub fn has_side_effects(&self) -> bool {
        match self {
            Stmt::Assign(_, e) => !e.is_pure(),
            Stmt::Line(_) => false,
            _ => true,
        }
    }
}

pub type BlockId = usize;

#[derive(Debug, Clone, PartialEq)]
pub enum Term {
    Goto(BlockId),
    If { cond: Expr, then: BlockId, els: BlockId },
    Switch { value: Expr, cases: Vec<(i32, BlockId)>, default: BlockId },
    Return(Option<Expr>),
    Throw(Expr),
}

impl Term {
    pub fn succs(&self) -> Vec<BlockId> {
        match self {
            Term::Goto(b) => vec![*b],
            Term::If { then, els, .. } => vec![*then, *els],
            Term::Switch { cases, default, .. } => {
                let mut v: Vec<BlockId> = cases.iter().map(|c| c.1).collect();
                v.push(*default);
                v
            }
            Term::Return(_) | Term::Throw(_) => vec![],
        }
    }

    pub fn visit_exprs(&self, f: &mut dyn FnMut(&Expr)) {
        match self {
            Term::If { cond: e, .. }
            | Term::Switch { value: e, .. }
            | Term::Throw(e)
            | Term::Return(Some(e)) => f(e),
            _ => {}
        }
    }

    pub fn visit_exprs_mut(&mut self, f: &mut dyn FnMut(&mut Expr)) {
        match self {
            Term::If { cond: e, .. }
            | Term::Switch { value: e, .. }
            | Term::Throw(e)
            | Term::Return(Some(e)) => f(e),
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
pub struct Block {
    /// Address of the first instruction (code units).
    pub start: u32,
    pub end: u32,
    pub stmts: Vec<Stmt>,
    pub term: Term,
    pub preds: Vec<BlockId>,
    /// Exception handlers reachable from this block: `(handler block, catch types; None = catch-all)`.
    pub handlers: Vec<(BlockId, Option<Arc<str>>)>,
    /// Index of the try region this block belongs to.
    pub try_region: Option<usize>,
}

/// Information about each recovered local variable.
#[derive(Debug, Clone, Default)]
pub struct VarInfo {
    pub name: Option<String>,
    /// Descriptor, when known (from debug info, definitions or uses).
    pub ty: Option<String>,
    /// Generic signature from debug info.
    pub signature: Option<String>,
    pub is_this: bool,
    pub param: Option<usize>,
    pub reg: u16,
}
