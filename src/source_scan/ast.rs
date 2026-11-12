// SPDX-License-Identifier: BUSL-1.1

//! The AST view of the sources, for the source guards that must not be
//! fooled by formatting.
//!
//! A text guard (`line.contains("X(")`, a count of `.current_dir(`) is
//! bypassed by a call split over two lines, an alias (`use ... as C`), a
//! `<T as Trait>::f` form or a file nobody listed. This module parses each
//! file ONCE per test binary with `syn` and keeps the facts a guard asks
//! for: every call (method or path) with its line and enclosing fn, every
//! field read, every path, every `==`, every `let`/`const` binding and every
//! struct pattern. A production scan leaves out `#[cfg(test)]` items and
//! `#[test]` fns the way the compiler does (no brace counting).
//!
//! A macro body is read in three steps (`macro_body`), so a call inside one is
//! seen: (1) a comma list of expressions (`assert!`, `format!`, `matches!`);
//! (2) the repeat form `[x; n]` (`vec![x; n]`); (3) otherwise the body is cut
//! at top-level `,` `;` `=>` and every part that parses as an expression
//! is one (`tokio::select!` arms, `x = fut`), and every part that does not
//! (a `target` key, a `field = %x` pair, `else`) goes to a token scan that finds
//! paths, calls, method calls and field reads, groups included. No macro body
//! is skipped.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ImplItem, Item, Pat, TraitItem};

/// `(line, column)` of a node's first token: the identity of a call.
pub(crate) type Pos = (usize, usize);

fn pos(span: Span) -> Pos {
    let s = span.start();
    (s.line, s.column)
}

impl Call {
    /// Is this a call of `name`: the method name, or the last path segment?
    pub(crate) fn named(&self, name: &str) -> bool {
        if self.method {
            self.name == name
        } else {
            self.segs.last().is_some_and(|s| s == name)
        }
    }
}

/// A method call (`recv.name(args)`) or a path call (`a::b(args)`).
pub(crate) struct Call {
    /// 1-based line of the method name, or of the start of the path.
    pub line: usize,
    pub pos: Pos,
    /// Id of the enclosing fn (0 = none); see `Facts::fn_name`.
    pub func: usize,
    /// The method name, or the path's segments joined by `::` (no generics).
    pub name: String,
    pub method: bool,
    /// Path calls: the segments (a `<T as Tr>::f` has `T`'s segments + `f`).
    pub segs: Vec<String>,
    /// Idents of every `::<..>` argument on the callee.
    pub generics: Vec<String>,
    /// Number of arguments.
    pub argc: usize,
    /// Per argument: `Some(b)` when it is the literal `true`/`false`.
    pub arg_bools: Vec<Option<bool>>,
    /// Method calls: every ident in the receiver chain (fields, methods, path segments).
    pub receiver_idents: Vec<String>,
    /// Every string literal in the arguments (any depth, macro bodies too).
    pub arg_strings: Vec<String>,
    /// Per argument: the ident when it is a plain path (`&x`, `x`).
    pub arg_idents: Vec<Option<String>>,
    /// Every ident in the arguments (path segments, fields, methods).
    pub arg_idents_all: Vec<String>,
    /// Every macro in the arguments, as `(name, tokens text)`.
    pub arg_macros: Vec<(String, String)>,
    /// Every path call in the arguments, as its `::`-joined segments.
    pub arg_calls: Vec<String>,
    /// Method calls: position of the call at the root of the receiver chain
    /// (`Command::new(x).arg(y).current_dir(z)` → the `Command::new` call).
    pub root_pos: Option<Pos>,
    /// Method calls: the last path segment at the root of the receiver chain
    /// (`cmd` in `cmd.arg(y).current_dir(z)`, `X` in `crate::m::X.inc()`).
    pub root_ident: Option<String>,
    /// Inside the arguments of an `assert!`-family macro.
    pub in_assert: bool,
}

/// A field read `base.name` (not a method call).
pub(crate) struct Field {
    pub line: usize,
    pub func: usize,
    pub name: String,
}

/// A `==` / `!=`.
pub(crate) struct Cmp {
    pub line: usize,
    pub func: usize,
    pub lhs: Side,
    pub rhs: Side,
    pub in_assert: bool,
}

/// One side of a `==`.
pub(crate) struct Side {
    pub strings: Vec<String>,
    /// The outermost method name (`x.to_string()` → `to_string`).
    pub method: Option<String>,
    /// The side is a string literal (after `&` and `(..)`).
    pub literal: bool,
    /// Every ident of the side (path segments, fields, methods).
    pub idents: Vec<String>,
}

impl Side {
    fn of(e: &Expr) -> Side {
        Side {
            strings: string_literals(e),
            method: match e {
                Expr::MethodCall(m) => Some(m.method.to_string()),
                _ => None,
            },
            literal: is_str_lit(e),
            idents: idents_in(e),
        }
    }
}

/// A `let x = init;`, `const X: T = init;` or `static`.
pub(crate) struct Binding {
    /// 1-based line of the initialiser.
    pub line: usize,
    pub func: usize,
    pub name: String,
    /// A `const` or `static` item (visible crate-wide), not a `let`.
    pub item: bool,
    /// The initialiser is a literal, a path or a `concat!` (after `&`, `(..)`):
    /// the binding IS that text, not a value computed from it.
    pub plain: bool,
    /// Every string literal in the initialiser (a `concat!` adds its joined text).
    pub init_strings: Vec<String>,
    /// Every ident in the initialiser (`const A: &str = B;` → `B`).
    pub init_idents: Vec<String>,
    /// The name the initialiser passes through: `f` in `f(a)`, `x` in
    /// `x.clone()` (see `value_root`).
    pub init_root: Option<String>,
    /// The macros that ARE the initialiser's value (see `value_macros`).
    pub init_value_macros: Vec<(String, String)>,
    /// Every path call in the initialiser (`HeaderMap::new`).
    pub init_calls: Vec<String>,
    /// Position of the call at the root of `init`'s method chain.
    pub init_root_pos: Option<Pos>,
}

/// Every path that names something: an expression path, a type path, a
/// pattern path, a `use` leaf (`alias` = the `as` name).
pub(crate) struct PathUse {
    pub line: usize,
    pub func: usize,
    pub segs: Vec<String>,
    pub alias: Option<String>,
    /// An expression path (`IamState::Disabled` as a value), not a pattern,
    /// a type or a `use`.
    pub expr: bool,
}

/// What a fn evaluates to: its tail expression and every `return expr`.
pub(crate) struct FnValue {
    pub name: String,
    pub macros: Vec<(String, String)>,
    /// The name the value passes through (see `value_root`).
    pub root: Option<String>,
}

/// A struct pattern `Path { field, .. }` (a destructuring read of `field`).
pub(crate) struct PatField {
    pub line: usize,
    pub path_last: String,
    pub field: String,
}

#[derive(Default)]
pub(crate) struct Facts {
    pub calls: Vec<Call>,
    pub fields: Vec<Field>,
    pub cmps: Vec<Cmp>,
    pub bindings: Vec<Binding>,
    pub paths: Vec<PathUse>,
    pub pat_fields: Vec<PatField>,
    pub struct_lits: Vec<StructLit>,
    pub fn_values: Vec<FnValue>,
    pub indexes: Vec<Index>,
    pub tuples: Vec<Tuple>,
    /// `&mut x.field`, `x.field = ..`, `x.field += ..`.
    pub field_writes: Vec<Field>,
}

impl Facts {
    /// Every call whose method name, or last path segment, is `name`.
    pub(crate) fn calls_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Call> {
        self.calls.iter().filter(move |c| {
            if c.method {
                c.name == name
            } else {
                c.segs.last().is_some_and(|s| s == name)
            }
        })
    }

    /// Does any path start at `ident` (`use aws_sdk_s3::..`, `aws_sdk_s3::Client`)?
    pub(crate) fn mentions(&self, ident: &str) -> bool {
        self.paths
            .iter()
            .any(|p| p.segs.first().is_some_and(|s| s == ident))
    }
}

/// `base[start..end]`: the idents of the range's ends.
pub(crate) struct Index {
    pub line: usize,
    /// Every ident (path segment, field, method name) in the range's bounds.
    pub bound_idents: Vec<String>,
    pub ranged: bool,
}

/// A tuple expression `(a, b(..), c)`: per element, the leaf of its path or
/// of the path it calls (`StatusCode::NO_CONTENT` → `NO_CONTENT`, `Json(x)` → `Json`).
pub(crate) struct Tuple {
    pub line: usize,
    pub elems: Vec<String>,
}

/// A struct literal `Path { a: 1, b, ..rest }`.
pub(crate) struct StructLit {
    pub line: usize,
    pub path_last: String,
    pub rest: bool,
}

/// One parsed file. The facts of production code and of test code (a
/// `#[cfg(test)]` item, a `#[test]` fn, or a whole test file) are apart, so a
/// guard names the code it is about without counting braces.
pub(crate) struct Source {
    /// Path relative to the crate root, `/` separators.
    pub rel: String,
    pub prod: Facts,
    pub test: Facts,
    /// `fn_names[id - 1]` is the name of fn `id`.
    fn_names: Vec<String>,
}

impl Source {
    pub(crate) fn fn_name(&self, id: usize) -> &str {
        if id == 0 {
            ""
        } else {
            &self.fn_names[id - 1]
        }
    }
}

/// Parse `text` as the production-or-test file `rel`. Fixtures of the
/// guards use this.
pub(crate) fn parse(rel: &str, text: &str) -> Source {
    parse_as(rel, text, false)
}

/// Parse `text` as a file that is test code throughout (`tests/*.rs`, an
/// out-of-line `#[cfg(test)] mod x;`): everything lands in `Source::test`.
pub(crate) fn parse_test_file(rel: &str, text: &str) -> Source {
    parse_as(rel, text, true)
}

fn parse_as(rel: &str, text: &str, test_file: bool) -> Source {
    let file = syn::parse_file(text).unwrap_or_else(|e| {
        let at = e.span().start();
        panic!(
            "{rel}:{}:{}: syn cannot parse the file: {e}",
            at.line, at.column
        )
    });
    let mut c = Collector {
        test_depth: usize::from(test_file),
        func: vec![0],
        assert_depth: 0,
        in_expr_path: 0,
        in_pat_path: false,
        prod: Facts::default(),
        test: Facts::default(),
        fn_names: Vec::new(),
    };
    c.visit_file(&file);
    Source {
        rel: rel.to_string(),
        prod: c.prod,
        test: c.test,
        fn_names: c.fn_names,
    }
}

type Cache = Mutex<HashMap<String, &'static [Source]>>;

fn cached(key: String, build: impl FnOnce() -> Vec<Source>) -> &'static [Source] {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let mut map = CACHE.get_or_init(Default::default).lock().unwrap();
    // Parsed once per test binary; the guards share the result.
    map.entry(key)
        .or_insert_with(|| Box::leak(build().into_boxed_slice()))
}

/// Every `.rs` file of `src/`, parsed once per test binary. A file that an
/// out-of-line `#[cfg(test)] mod x;` names is test code throughout.
pub(crate) fn src() -> &'static [Source] {
    cached("src".into(), || {
        let files = super::rust_files("src");
        let tests = super::out_of_line_test_modules(&files);
        files
            .into_iter()
            .map(|f| {
                let text = std::fs::read_to_string(&f).unwrap();
                parse_as(&super::rel(&f), &text, tests.contains(&f))
            })
            .collect()
    })
}

/// Every `.rs` file under `tests/` (test code throughout), parsed once.
pub(crate) fn tests() -> &'static [Source] {
    cached("tests".into(), || {
        super::rust_files("tests")
            .into_iter()
            .map(|f| {
                let text = std::fs::read_to_string(&f).unwrap();
                parse_test_file(&super::rel(&f), &text)
            })
            .collect()
    })
}

/// Methods that test a string against a literal.
pub(crate) const TEXT_METHODS: [&str; 12] = [
    "contains",
    "starts_with",
    "ends_with",
    "eq_ignore_ascii_case",
    "eq",
    "ne",
    "find",
    "rfind",
    "matches",
    "strip_prefix",
    "strip_suffix",
    "split",
];

// ----- expression helpers ---------------------------------------------------

/// The root of a receiver chain: `a.b().c?.d` → `a`.
pub(crate) fn chain_root(e: &Expr) -> &Expr {
    match e {
        Expr::MethodCall(m) => chain_root(&m.receiver),
        Expr::Field(f) => chain_root(&f.base),
        Expr::Paren(p) => chain_root(&p.expr),
        Expr::Group(g) => chain_root(&g.expr),
        Expr::Reference(r) => chain_root(&r.expr),
        Expr::Try(t) => chain_root(&t.expr),
        Expr::Await(a) => chain_root(&a.base),
        _ => e,
    }
}

/// Methods and wrappers that pass a path or string through unchanged.
const CONVERSIONS: [&str; 8] = [
    "clone",
    "to_owned",
    "to_string",
    "into",
    "as_str",
    "as_ref",
    "to_path_buf",
    "as_path",
];

/// The macros that ARE the value of `e`: `e` is the macro (`env!(..)`),
/// `String::from(..)`/`PathBuf::from(..)` of it, or a conversion of those
/// (`env!(..).into()`). A macro deep in a block, a struct literal or the
/// argument of another call (`Command::new(env!(..))`) is not the value.
pub(crate) fn value_macros(e: &Expr) -> Vec<(String, String)> {
    match e {
        Expr::Macro(_) => macros_in(e),
        Expr::MethodCall(m) if CONVERSIONS.contains(&m.method.to_string().as_str()) => {
            value_macros(&m.receiver)
        }
        Expr::Call(c) if c.args.len() == 1 && call_leaf(c).as_deref() == Some("from") => {
            value_macros(&c.args[0])
        }
        Expr::Reference(r) => value_macros(&r.expr),
        Expr::Paren(p) => value_macros(&p.expr),
        Expr::Group(g) => value_macros(&g.expr),
        _ => Vec::new(),
    }
}

fn call_leaf(c: &syn::ExprCall) -> Option<String> {
    match &*c.func {
        Expr::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

/// The name that `e` passes through: `x`, `&x`, `x.clone()`, `f()` (a call
/// of the path `f`). `None` after any other method: `x.len()` is no `x`.
pub(crate) fn value_root(e: &Expr) -> Option<String> {
    match e {
        Expr::Path(p) if p.qself.is_none() => p.path.segments.last().map(|s| s.ident.to_string()),
        Expr::Call(c) => call_leaf(c),
        Expr::MethodCall(m) if CONVERSIONS.contains(&m.method.to_string().as_str()) => {
            value_root(&m.receiver)
        }
        Expr::Reference(r) => value_root(&r.expr),
        Expr::Paren(p) => value_root(&p.expr),
        Expr::Group(g) => value_root(&g.expr),
        _ => None,
    }
}

/// `x` when `e` is the single-ident path `x` (after `&`, `(..)`).
pub(crate) fn as_ident(e: &Expr) -> Option<String> {
    match e {
        Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
            Some(p.path.segments[0].ident.to_string())
        }
        Expr::Reference(r) => as_ident(&r.expr),
        Expr::Paren(p) => as_ident(&p.expr),
        Expr::Group(g) => as_ident(&g.expr),
        _ => None,
    }
}

/// Is `e` a string literal, after `&` and `(..)`?
fn is_str_lit(e: &Expr) -> bool {
    match e {
        Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(_),
            ..
        }) => true,
        Expr::Reference(r) => is_str_lit(&r.expr),
        Expr::Paren(p) => is_str_lit(&p.expr),
        Expr::Group(g) => is_str_lit(&g.expr),
        _ => false,
    }
}

/// Is `e` a literal, a path or a `concat!`, after `&` and `(..)`?
fn is_plain(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) | Expr::Path(_) => true,
        Expr::Macro(m) => m.mac.path.is_ident("concat"),
        Expr::Reference(r) => is_plain(&r.expr),
        Expr::Paren(p) => is_plain(&p.expr),
        Expr::Group(g) => is_plain(&g.expr),
        _ => false,
    }
}

/// `Some(b)` when `e` is the literal `true` / `false`.
fn as_bool(e: &Expr) -> Option<bool> {
    match e {
        Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Bool(b),
            ..
        }) => Some(b.value),
        _ => None,
    }
}

/// Every ident in `e`: path segments, field names, method names.
pub(crate) fn idents_in(e: &Expr) -> Vec<String> {
    struct V(Vec<String>);
    impl<'ast> Visit<'ast> for V {
        fn visit_ident(&mut self, i: &'ast syn::Ident) {
            self.0.push(i.to_string());
        }
        fn visit_macro(&mut self, m: &'ast syn::Macro) {
            for e in macro_exprs(m) {
                self.visit_expr(&e);
            }
        }
    }
    let mut v = V(Vec::new());
    v.visit_expr(e);
    v.0
}

/// The last segment of `e` when it is a path (`crate::m::X` → `X`).
fn path_leaf(e: &Expr) -> Option<String> {
    match e {
        Expr::Path(p) if p.qself.is_none() => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

/// Every string literal inside `e` (any depth, macro bodies included).
pub(crate) fn string_literals(e: &Expr) -> Vec<String> {
    struct V(Vec<String>);
    impl<'ast> Visit<'ast> for V {
        fn visit_lit_str(&mut self, l: &'ast syn::LitStr) {
            self.0.push(l.value());
        }
        fn visit_macro(&mut self, m: &'ast syn::Macro) {
            for e in macro_exprs(m) {
                self.visit_expr(&e);
            }
        }
    }
    let mut v = V(Vec::new());
    v.visit_expr(e);
    v.0
}

/// Every macro inside `e` (any depth, macro bodies too) as `(name, tokens
/// text)`: `env!("CARGO_BIN_EXE_x")` → `("env", "\"CARGO_BIN_EXE_x\"")`.
pub(crate) fn macros_in(e: &Expr) -> Vec<(String, String)> {
    struct V(Vec<(String, String)>);
    impl<'ast> Visit<'ast> for V {
        fn visit_macro(&mut self, m: &'ast syn::Macro) {
            let name = m.path.segments.last().map(|s| s.ident.to_string());
            self.0
                .push((name.unwrap_or_default(), m.tokens.to_string()));
            for e in macro_exprs(m) {
                self.visit_expr(&e);
            }
        }
    }
    let mut v = V(Vec::new());
    v.visit_expr(e);
    v.0
}

/// Every path call inside `e` (`HeaderMap::new()` → `HeaderMap::new`).
pub(crate) fn calls_in(e: &Expr) -> Vec<String> {
    struct V(Vec<String>);
    impl<'ast> Visit<'ast> for V {
        fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
            if let Expr::Path(p) = &*c.func {
                self.0.push(expr_path_segs(p).join("::"));
            }
            visit::visit_expr_call(self, c);
        }
        fn visit_macro(&mut self, m: &'ast syn::Macro) {
            for e in macro_exprs(m) {
                self.visit_expr(&e);
            }
        }
    }
    let mut v = V(Vec::new());
    v.visit_expr(e);
    v.0
}

/// A macro body, split into what parses as expressions and what does not.
#[derive(Default)]
pub(crate) struct MacroBody {
    pub exprs: Vec<Expr>,
    /// Parts that are no expression (a `target` key, a `field = %x` pair, `else`):
    /// the collector scans their tokens.
    pub raw: Vec<TokenStream>,
}

/// Is `t` the punct `c`?
fn is_punct(t: &TokenTree, c: char) -> bool {
    matches!(t, TokenTree::Punct(p) if p.as_char() == c)
}

/// Cut `tokens` at every top-level `,`, `;` and `=>`. A group is one token,
/// so a separator inside `(..)`, `[..]` or `{..}` never cuts.
fn split_top(tokens: &TokenStream) -> Vec<TokenStream> {
    let toks: Vec<TokenTree> = tokens.clone().into_iter().collect();
    let mut parts = Vec::new();
    let mut cur = TokenStream::new();
    let mut i = 0;
    while i < toks.len() {
        let arrow = is_punct(&toks[i], '=') && toks.get(i + 1).is_some_and(|t| is_punct(t, '>'));
        if arrow {
            parts.push(std::mem::take(&mut cur));
            i += 2;
        } else if is_punct(&toks[i], ',') || is_punct(&toks[i], ';') {
            parts.push(std::mem::take(&mut cur));
            i += 1;
        } else {
            cur.extend([toks[i].clone()]);
            i += 1;
        }
    }
    parts.push(cur);
    parts.into_iter().filter(|p| !p.is_empty()).collect()
}

/// `x; n` (the inside of `vec![x; n]` and `[x; n]`).
fn parse_repeat(tokens: &TokenStream) -> Option<Vec<Expr>> {
    (|input: syn::parse::ParseStream| {
        let x: Expr = input.parse()?;
        input.parse::<syn::Token![;]>()?;
        let n: Expr = input.parse()?;
        Ok(vec![x, n])
    })
    .parse2(tokens.clone())
    .ok()
}

/// Read a macro body: see the module docs.
pub(crate) fn macro_body(tokens: &TokenStream) -> MacroBody {
    if let Ok(p) = Punctuated::<Expr, syn::Token![,]>::parse_terminated.parse2(tokens.clone()) {
        return MacroBody {
            exprs: p.into_iter().collect(),
            raw: Vec::new(),
        };
    }
    if let Some(exprs) = parse_repeat(tokens) {
        return MacroBody {
            exprs,
            raw: Vec::new(),
        };
    }
    let mut body = MacroBody::default();
    for part in split_top(tokens) {
        match syn::parse2::<Expr>(part.clone()) {
            Ok(e) => body.exprs.push(e),
            Err(_) => body.raw.push(part),
        }
    }
    body
}

/// The expressions of a macro body (`assert!(a, "m")`, `format!(..)`,
/// `vec![x; n]`, the arms of a `select!`); what is no expression is not here.
pub(crate) fn macro_exprs(m: &syn::Macro) -> Vec<Expr> {
    macro_body(&m.tokens).exprs
}

fn is_assert_macro(m: &syn::Macro) -> bool {
    m.path.segments.last().is_some_and(|s| {
        let n = s.ident.to_string();
        n.starts_with("assert") || n.starts_with("debug_assert")
    })
}

// ----- the collector ----------------------------------------------------------

struct Collector {
    /// > 0 inside a `#[cfg(test)]` item or a `#[test]` fn.
    test_depth: usize,
    /// Stack of fn ids; the last one is the enclosing fn (0 = none).
    func: Vec<usize>,
    assert_depth: usize,
    /// > 0 while visiting the path of an `ExprPath` (recorded whole there).
    in_expr_path: usize,
    in_pat_path: bool,
    prod: Facts,
    test: Facts,
    fn_names: Vec<String>,
}

/// Does the cfg predicate `m` hold only when `test` is on? `test`, `all(..)`
/// with such an operand, `any(..)` of only such operands. `not(..)`, a
/// `feature = ".."` and any other name do not require `test`.
fn requires_test(m: &syn::Meta) -> bool {
    match m {
        syn::Meta::Path(p) => p.is_ident("test"),
        syn::Meta::NameValue(_) => false,
        syn::Meta::List(l) => {
            let ops = l
                .parse_args_with(Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
                .map(|p| p.into_iter().collect::<Vec<_>>())
                .unwrap_or_default();
            if l.path.is_ident("all") {
                ops.iter().any(requires_test)
            } else if l.path.is_ident("any") {
                !ops.is_empty() && ops.iter().all(requires_test)
            } else {
                false
            }
        }
    }
}

fn cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg") && a.parse_args::<syn::Meta>().is_ok_and(|m| requires_test(&m))
    })
}

fn test_attr(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| a.path().segments.last().is_some_and(|s| s.ident == "test"))
}

fn item_attrs(i: &Item) -> &[Attribute] {
    match i {
        Item::Const(x) => &x.attrs,
        Item::Enum(x) => &x.attrs,
        Item::Fn(x) => &x.attrs,
        Item::Impl(x) => &x.attrs,
        Item::Macro(x) => &x.attrs,
        Item::Mod(x) => &x.attrs,
        Item::Static(x) => &x.attrs,
        Item::Struct(x) => &x.attrs,
        Item::Trait(x) => &x.attrs,
        Item::Type(x) => &x.attrs,
        Item::Use(x) => &x.attrs,
        _ => &[],
    }
}

fn segs_of(path: &syn::Path) -> Vec<String> {
    path.segments.iter().map(|s| s.ident.to_string()).collect()
}

/// Idents of the generic arguments of every segment (`f::<A, B>`).
fn generic_idents(path: &syn::Path) -> Vec<String> {
    struct V(Vec<String>);
    impl<'ast> Visit<'ast> for V {
        fn visit_path_segment(&mut self, s: &'ast syn::PathSegment) {
            self.0.push(s.ident.to_string());
            visit::visit_path_segment(self, s);
        }
    }
    let mut out = Vec::new();
    for seg in &path.segments {
        if let syn::PathArguments::AngleBracketed(a) = &seg.arguments {
            let mut v = V(Vec::new());
            v.visit_angle_bracketed_generic_arguments(a);
            out.extend(v.0);
        }
    }
    out
}

/// The segments of an expression path, `<T as Tr>::f` included (`T`'s own
/// segments, then what follows the trait).
fn expr_path_segs(p: &syn::ExprPath) -> Vec<String> {
    match &p.qself {
        None => segs_of(&p.path),
        Some(q) => {
            let mut out = match &*q.ty {
                syn::Type::Path(t) => segs_of(&t.path),
                _ => Vec::new(),
            };
            out.extend(
                p.path
                    .segments
                    .iter()
                    .skip(q.position)
                    .map(|s| s.ident.to_string()),
            );
            out
        }
    }
}

impl Collector {
    fn is_test(attrs: &[Attribute]) -> bool {
        cfg_test(attrs) || test_attr(attrs)
    }

    /// The facts of the code being visited.
    fn cur(&mut self) -> &mut Facts {
        if self.test_depth > 0 {
            &mut self.test
        } else {
            &mut self.prod
        }
    }

    fn func(&self) -> usize {
        *self.func.last().unwrap()
    }

    fn enter(&mut self, name: &syn::Ident) {
        self.fn_names.push(name.to_string());
        self.func.push(self.fn_names.len());
    }

    fn leave(&mut self) {
        self.func.pop();
    }

    /// Record `e` as a value of the enclosing fn (its tail or a `return`).
    fn fn_value(&mut self, e: &Expr) {
        let func = self.func();
        if func == 0 {
            return;
        }
        let v = FnValue {
            name: self.fn_names[func - 1].clone(),
            macros: value_macros(e),
            root: value_root(e),
        };
        self.cur().fn_values.push(v);
    }

    /// The tail expression of a fn body, as a value.
    fn tail_value(&mut self, b: &syn::Block) {
        if let Some(syn::Stmt::Expr(e, None)) = b.stmts.last() {
            self.fn_value(e);
        }
    }

    fn root_pos(e: &Expr) -> Option<Pos> {
        match chain_root(e) {
            Expr::Call(c) => Some(pos(c.span())),
            _ => None,
        }
    }

    /// Visit `body` as test code when `attrs` say so.
    fn scoped(&mut self, attrs: &[Attribute], body: impl FnOnce(&mut Self)) {
        let test = Self::is_test(attrs);
        if test {
            self.test_depth += 1;
        }
        body(self);
        if test {
            self.test_depth -= 1;
        }
    }

    fn bind(&mut self, name: String, init: &Expr, item: bool) {
        let func = self.func();
        let init_macros = macros_in(init);
        let mut init_strings = string_literals(init);
        if init_macros.iter().any(|(n, _)| n == "concat") {
            init_strings.push(init_strings.concat());
        }
        self.cur().bindings.push(Binding {
            line: pos(init.span()).0,
            func,
            name,
            item,
            plain: is_plain(init),
            init_strings,
            init_idents: idents_in(init),
            init_root: value_root(init),
            init_value_macros: value_macros(init),
            init_calls: calls_in(init),
            init_root_pos: Self::root_pos(init),
        });
    }
}

/// What a call site knows; `Collector::make_call` fills the rest of `Call`.
struct CallShape<'a> {
    line: usize,
    pos: Pos,
    name: String,
    method: bool,
    segs: Vec<String>,
    generics: Vec<String>,
    receiver_idents: Vec<String>,
    root_pos: Option<Pos>,
    root_ident: Option<String>,
    args: &'a [&'a Expr],
}

/// Idents that start no path or call: `if (a)` is no call of `if`.
const KEYWORDS: [&str; 22] = [
    "if", "else", "match", "while", "for", "in", "loop", "return", "let", "move", "async", "await",
    "break", "continue", "fn", "mut", "ref", "as", "where", "unsafe", "dyn", "impl",
];

impl Collector {
    fn make_call(&self, s: CallShape) -> Call {
        let a = s.args;
        Call {
            line: s.line,
            pos: s.pos,
            func: self.func(),
            name: s.name,
            method: s.method,
            segs: s.segs,
            generics: s.generics,
            argc: a.len(),
            arg_bools: a.iter().map(|e| as_bool(e)).collect(),
            receiver_idents: s.receiver_idents,
            arg_strings: a.iter().flat_map(|e| string_literals(e)).collect(),
            arg_idents: a.iter().map(|e| as_ident(e)).collect(),
            arg_idents_all: a.iter().flat_map(|e| idents_in(e)).collect(),
            arg_macros: a.iter().flat_map(|e| macros_in(e)).collect(),
            arg_calls: a.iter().flat_map(|e| calls_in(e)).collect(),
            root_pos: s.root_pos,
            root_ident: s.root_ident,
            in_assert: self.assert_depth > 0,
        }
    }

    /// Visit a macro body: its expressions as code, the rest by token scan.
    fn visit_body(&mut self, tokens: &TokenStream) {
        let body = macro_body(tokens);
        for e in &body.exprs {
            self.visit_expr(e);
        }
        for raw in body.raw {
            self.scan_tokens(raw);
        }
    }

    /// Record a call found by token scan; its arguments are what the group
    /// parses to, and are visited as code.
    fn scan_call(&mut self, shape: CallShape<'static>, group: &TokenStream) {
        let body = macro_body(group);
        let args: Vec<&Expr> = body.exprs.iter().collect();
        let mut shape: CallShape<'_> = shape;
        shape.args = &args;
        let call = self.make_call(shape);
        self.cur().calls.push(call);
        for e in &body.exprs {
            self.visit_expr(e);
        }
        for raw in body.raw {
            self.scan_tokens(raw);
        }
    }

    /// Token-level scan of a macro part that is no expression
    /// (a `target` key, a `field = %x.len()` pair, `else`): records every path
    /// (`a::b::c`), path call (`a::b(..)`), method call (`.name(..)`) and
    /// field read (`.name`), and goes into every group. What is lost against
    /// the AST: the receiver of a scanned method call is its previous ident.
    fn scan_tokens(&mut self, ts: TokenStream) {
        let toks: Vec<TokenTree> = ts.into_iter().collect();
        let mut i = 0;
        while i < toks.len() {
            match &toks[i] {
                TokenTree::Group(g) => {
                    self.visit_body(&g.stream());
                    i += 1;
                }
                TokenTree::Punct(p)
                    if p.as_char() == '.'
                        && !(i > 0 && is_punct(&toks[i - 1], '.'))
                        && matches!(toks.get(i + 1), Some(TokenTree::Ident(_))) =>
                {
                    let TokenTree::Ident(name) = &toks[i + 1] else {
                        unreachable!()
                    };
                    let recv = match i.checked_sub(1).map(|j| &toks[j]) {
                        Some(TokenTree::Ident(r)) => Some(r.to_string()),
                        _ => None,
                    };
                    let group = match toks.get(i + 2) {
                        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis => {
                            Some(g.stream())
                        }
                        _ => None,
                    };
                    match group {
                        Some(g) => {
                            let shape = CallShape {
                                line: name.span().start().line,
                                pos: pos(name.span()),
                                name: name.to_string(),
                                method: true,
                                segs: Vec::new(),
                                generics: Vec::new(),
                                receiver_idents: recv.iter().cloned().collect(),
                                root_pos: None,
                                root_ident: recv.clone(),
                                args: &[],
                            };
                            self.scan_call(shape, &g);
                            i += 3;
                        }
                        None => {
                            let f = Field {
                                line: name.span().start().line,
                                func: self.func(),
                                name: name.to_string(),
                            };
                            self.cur().fields.push(f);
                            i += 2;
                        }
                    }
                }
                TokenTree::Ident(first) if !KEYWORDS.contains(&first.to_string().as_str()) => {
                    // `a::b::c`, `a::<T>::b`: idents joined by `::`.
                    let start = first.span();
                    let mut segs = vec![first.to_string()];
                    let mut j = i + 1;
                    while j + 1 < toks.len()
                        && is_punct(&toks[j], ':')
                        && is_punct(&toks[j + 1], ':')
                    {
                        match toks.get(j + 2) {
                            Some(TokenTree::Ident(n)) => {
                                segs.push(n.to_string());
                                j += 3;
                            }
                            Some(t) if is_punct(t, '<') => {
                                // Skip the turbofish: balanced `<` `>`.
                                let mut depth = 0usize;
                                j += 2;
                                while j < toks.len() {
                                    if is_punct(&toks[j], '<') {
                                        depth += 1;
                                    } else if is_punct(&toks[j], '>') {
                                        depth -= 1;
                                        if depth == 0 {
                                            j += 1;
                                            break;
                                        }
                                    }
                                    j += 1;
                                }
                            }
                            _ => break,
                        }
                    }
                    let next = toks.get(j);
                    let call_group = match next {
                        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis => {
                            Some(g.stream())
                        }
                        _ => None,
                    };
                    let macro_group = match (next, toks.get(j + 1)) {
                        (Some(bang), Some(TokenTree::Group(g))) if is_punct(bang, '!') => {
                            Some(g.stream())
                        }
                        _ => None,
                    };
                    if segs.len() > 1 || call_group.is_some() {
                        let u = PathUse {
                            line: start.start().line,
                            func: self.func(),
                            segs: segs.clone(),
                            alias: None,
                            expr: true,
                        };
                        self.cur().paths.push(u);
                    }
                    if let Some(g) = call_group {
                        let shape = CallShape {
                            line: start.start().line,
                            pos: pos(start),
                            name: segs.join("::"),
                            method: false,
                            segs: segs.clone(),
                            generics: Vec::new(),
                            receiver_idents: Vec::new(),
                            root_pos: None,
                            root_ident: None,
                            args: &[],
                        };
                        self.scan_call(shape, &g);
                        i = j + 1;
                    } else if let Some(g) = macro_group {
                        self.visit_body(&g);
                        i = j + 2;
                    } else {
                        i = j.max(i + 1);
                    }
                }
                _ => i += 1,
            }
        }
    }
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_item(&mut self, i: &'ast Item) {
        self.scoped(item_attrs(i), |c| visit::visit_item(c, i));
    }

    fn visit_impl_item(&mut self, i: &'ast ImplItem) {
        let attrs = match i {
            ImplItem::Fn(f) => &f.attrs,
            ImplItem::Const(c) => &c.attrs,
            ImplItem::Macro(m) => &m.attrs,
            _ => &[][..],
        };
        self.scoped(attrs, |c| visit::visit_impl_item(c, i));
    }

    fn visit_trait_item(&mut self, i: &'ast TraitItem) {
        let attrs = match i {
            TraitItem::Fn(f) => &f.attrs,
            TraitItem::Const(c) => &c.attrs,
            TraitItem::Macro(m) => &m.attrs,
            _ => &[][..],
        };
        self.scoped(attrs, |c| visit::visit_trait_item(c, i));
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.enter(&i.sig.ident);
        self.tail_value(&i.block);
        visit::visit_item_fn(self, i);
        self.leave();
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.enter(&i.sig.ident);
        self.tail_value(&i.block);
        visit::visit_impl_item_fn(self, i);
        self.leave();
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.enter(&i.sig.ident);
        visit::visit_trait_item_fn(self, i);
        self.leave();
    }

    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.bind(i.ident.to_string(), &i.expr, true);
        visit::visit_item_const(self, i);
    }

    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.bind(i.ident.to_string(), &i.expr, true);
        visit::visit_item_static(self, i);
    }

    fn visit_local(&mut self, l: &'ast syn::Local) {
        if let Some(init) = &l.init {
            let mut pat = &l.pat;
            while let Pat::Type(t) = pat {
                pat = &t.pat;
            }
            if let Pat::Ident(id) = pat {
                self.bind(id.ident.to_string(), &init.expr, false);
            }
        }
        visit::visit_local(self, l);
    }

    fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
        let root = chain_root(&m.receiver);
        let generics = m
            .turbofish
            .as_ref()
            .map(|t| {
                let mut p = syn::Path::from(m.method.clone());
                p.segments[0].arguments = syn::PathArguments::AngleBracketed(t.clone());
                generic_idents(&p)
            })
            .unwrap_or_default();
        let args: Vec<&Expr> = m.args.iter().collect();
        let call = self.make_call(CallShape {
            line: m.method.span().start().line,
            pos: pos(m.method.span()),
            name: m.method.to_string(),
            method: true,
            segs: Vec::new(),
            generics,
            receiver_idents: idents_in(&m.receiver),
            root_pos: Self::root_pos(&m.receiver),
            root_ident: path_leaf(root),
            args: &args,
        });
        self.cur().calls.push(call);
        visit::visit_expr_method_call(self, m);
    }

    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        if let Expr::Path(p) = &*c.func {
            let segs = expr_path_segs(p);
            let args: Vec<&Expr> = c.args.iter().collect();
            let call = self.make_call(CallShape {
                line: p.span().start().line,
                pos: pos(c.span()),
                name: segs.join("::"),
                method: false,
                generics: generic_idents(&p.path),
                segs,
                receiver_idents: Vec::new(),
                root_pos: None,
                root_ident: None,
                args: &args,
            });
            self.cur().calls.push(call);
        }
        visit::visit_expr_call(self, c);
    }

    fn visit_expr_return(&mut self, r: &'ast syn::ExprReturn) {
        if let Some(e) = &r.expr {
            self.fn_value(e);
        }
        visit::visit_expr_return(self, r);
    }

    fn visit_expr_field(&mut self, f: &'ast syn::ExprField) {
        if let syn::Member::Named(n) = &f.member {
            let field = Field {
                line: n.span().start().line,
                func: self.func(),
                name: n.to_string(),
            };
            self.cur().fields.push(field);
        }
        visit::visit_expr_field(self, f);
    }

    fn visit_expr_assign(&mut self, a: &'ast syn::ExprAssign) {
        self.field_write(&a.left);
        visit::visit_expr_assign(self, a);
    }

    fn visit_expr_reference(&mut self, r: &'ast syn::ExprReference) {
        if r.mutability.is_some() {
            self.field_write(&r.expr);
        }
        visit::visit_expr_reference(self, r);
    }

    fn visit_expr_struct(&mut self, s: &'ast syn::ExprStruct) {
        let fields: Vec<String> = s
            .fields
            .iter()
            .filter_map(|f| match &f.member {
                syn::Member::Named(n) => Some(n.to_string()),
                _ => None,
            })
            .collect();
        let func = self.func();
        for name in &fields {
            let line = s.path.span().start().line;
            self.cur().field_writes.push(Field {
                line,
                func,
                name: name.clone(),
            });
        }
        let lit = StructLit {
            line: s.path.span().start().line,
            path_last: s
                .path
                .segments
                .last()
                .map(|x| x.ident.to_string())
                .unwrap_or_default(),
            rest: s.rest.is_some(),
        };
        self.cur().struct_lits.push(lit);
        visit::visit_expr_struct(self, s);
    }

    fn visit_expr_index(&mut self, i: &'ast syn::ExprIndex) {
        let (ranged, bound_idents) = match &*i.index {
            Expr::Range(r) => (
                true,
                r.start
                    .iter()
                    .chain(r.end.iter())
                    .flat_map(|b| idents_in(b))
                    .collect(),
            ),
            other => (false, idents_in(other)),
        };
        let ix = Index {
            line: i.bracket_token.span.open().start().line,
            bound_idents,
            ranged,
        };
        self.cur().indexes.push(ix);
        visit::visit_expr_index(self, i);
    }

    fn visit_expr_tuple(&mut self, t: &'ast syn::ExprTuple) {
        let elems = t
            .elems
            .iter()
            .map(|e| match e {
                Expr::Call(c) => match &*c.func {
                    Expr::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
                    _ => None,
                },
                other => path_leaf(other),
            })
            .map(|x| x.unwrap_or_default())
            .collect();
        let tu = Tuple {
            line: t.paren_token.span.open().start().line,
            elems,
        };
        self.cur().tuples.push(tu);
        visit::visit_expr_tuple(self, t);
    }

    fn visit_expr_binary(&mut self, b: &'ast syn::ExprBinary) {
        let compound = matches!(
            b.op,
            syn::BinOp::AddAssign(_)
                | syn::BinOp::SubAssign(_)
                | syn::BinOp::MulAssign(_)
                | syn::BinOp::DivAssign(_)
                | syn::BinOp::BitOrAssign(_)
                | syn::BinOp::BitAndAssign(_)
                | syn::BinOp::ShlAssign(_)
                | syn::BinOp::ShrAssign(_)
                | syn::BinOp::RemAssign(_)
                | syn::BinOp::BitXorAssign(_)
        );
        if compound {
            self.field_write(&b.left);
        }
        if matches!(b.op, syn::BinOp::Eq(_) | syn::BinOp::Ne(_)) {
            let cmp = Cmp {
                line: b.op.span().start().line,
                func: self.func(),
                lhs: Side::of(&b.left),
                rhs: Side::of(&b.right),
                in_assert: self.assert_depth > 0,
            };
            self.cur().cmps.push(cmp);
        }
        visit::visit_expr_binary(self, b);
    }

    fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
        // A pattern path is recorded by `visit_pat`, as no value.
        if !self.in_pat_path {
            let u = PathUse {
                line: p.span().start().line,
                func: self.func(),
                segs: expr_path_segs(p),
                alias: None,
                expr: true,
            };
            self.cur().paths.push(u);
        }
        self.in_expr_path += 1;
        visit::visit_expr_path(self, p);
        self.in_expr_path -= 1;
    }

    fn visit_pat(&mut self, p: &'ast Pat) {
        if let Pat::Path(ep) = p {
            // `X::Unit` in a pattern is no value.
            let u = PathUse {
                line: ep.span().start().line,
                func: self.func(),
                segs: expr_path_segs(ep),
                alias: None,
                expr: false,
            };
            self.cur().paths.push(u);
            self.in_expr_path += 1;
            self.in_pat_path = true;
            visit::visit_pat(self, p);
            self.in_pat_path = false;
            self.in_expr_path -= 1;
        } else {
            visit::visit_pat(self, p);
        }
    }

    fn visit_path(&mut self, p: &'ast syn::Path) {
        // The path of an `ExprPath` is recorded whole by `visit_expr_path`.
        if self.in_expr_path == 0 {
            let u = PathUse {
                line: p.span().start().line,
                func: self.func(),
                segs: segs_of(p),
                alias: None,
                expr: false,
            };
            self.cur().paths.push(u);
        }
        let saved = std::mem::take(&mut self.in_expr_path);
        visit::visit_path(self, p);
        self.in_expr_path = saved;
    }

    fn visit_use_tree(&mut self, t: &'ast syn::UseTree) {
        fn leaves(
            t: &syn::UseTree,
            prefix: &mut Vec<String>,
            out: &mut Vec<(Vec<String>, Option<String>, usize)>,
        ) {
            match t {
                syn::UseTree::Path(p) => {
                    prefix.push(p.ident.to_string());
                    leaves(&p.tree, prefix, out);
                    prefix.pop();
                }
                syn::UseTree::Name(n) => {
                    let mut s = prefix.clone();
                    s.push(n.ident.to_string());
                    out.push((s, None, n.ident.span().start().line));
                }
                syn::UseTree::Rename(r) => {
                    let mut s = prefix.clone();
                    s.push(r.ident.to_string());
                    out.push((s, Some(r.rename.to_string()), r.ident.span().start().line));
                }
                syn::UseTree::Glob(g) => {
                    let mut s = prefix.clone();
                    s.push("*".into());
                    out.push((s, None, g.star_token.span().start().line));
                }
                syn::UseTree::Group(g) => {
                    for t in &g.items {
                        leaves(t, prefix, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        leaves(t, &mut Vec::new(), &mut out);
        let func = self.func();
        for (segs, alias, line) in out {
            self.cur().paths.push(PathUse {
                line,
                func,
                segs,
                alias,
                expr: false,
            });
        }
        // Not `visit::visit_use_tree`: the leaves are recorded whole above.
    }

    fn visit_pat_struct(&mut self, p: &'ast syn::PatStruct) {
        if let Some(last) = p.path.segments.last() {
            for f in &p.fields {
                if let syn::Member::Named(n) = &f.member {
                    let pf = PatField {
                        line: n.span().start().line,
                        path_last: last.ident.to_string(),
                        field: n.to_string(),
                    };
                    self.cur().pat_fields.push(pf);
                }
            }
        }
        visit::visit_pat_struct(self, p);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        let assert = is_assert_macro(m);
        if assert {
            self.assert_depth += 1;
        }
        let name = m.path.segments.last().map(|s| s.ident.to_string());
        if name.as_deref() == Some("matches") {
            // `matches!(value, Pattern)`: only the value is an expression.
            let body = macro_body(&m.tokens);
            if let Some(e) = body.exprs.first() {
                self.visit_expr(e);
            }
        } else {
            self.visit_body(&m.tokens);
        }
        if assert {
            self.assert_depth -= 1;
        }
        visit::visit_macro(self, m);
    }
}

impl Collector {
    /// Record `e` as a field written (`x.field = ..`, `&mut x.field`).
    fn field_write(&mut self, e: &Expr) {
        if let Expr::Field(f) = e {
            if let syn::Member::Named(n) = &f.member {
                let w = Field {
                    line: n.span().start().line,
                    func: self.func(),
                    name: n.to_string(),
                };
                self.cur().field_writes.push(w);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfg_test_items_and_test_fns_are_test_facts() {
        let s = parse(
            "fixture.rs",
            "fn prod() { a.one(); }\n\
             #[cfg(test)] fn only_in_tests() { a.two(); }\n\
             fn also_prod() { a.three(); }\n\
             #[cfg(test)] mod tests { fn t() { a.four(); } }\n\
             #[test] fn unit() { a.five(); }\n\
             #[cfg(not(test))] fn real() { a.six(); }\n\
             impl X { #[cfg(test)] fn h(&self) { a.seven(); } fn p(&self) { a.eight(); } }\n",
        );
        let names = |f: &Facts| f.calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&s.prod), ["one", "three", "six", "eight"]);
        assert_eq!(names(&s.test), ["two", "four", "five", "seven"]);
        let t = parse_test_file("f.rs", "fn helper() { a.x(); }");
        assert!(
            t.prod.calls.is_empty() && t.test.calls.len() == 1,
            "a test file"
        );
    }

    #[test]
    fn calls_carry_line_fn_and_chain_root_even_when_split() {
        let s = parse(
            "fixture.rs",
            "fn f() {\n    let c = Command::new(B)\n        .arg(1)\n        .current_dir(d);\n    c.arg(2)\n        .current_dir(e);\n}\n",
        );
        let cwd: Vec<&Call> = s
            .prod
            .calls
            .iter()
            .filter(|c| c.name == "current_dir")
            .collect();
        assert_eq!(cwd.len(), 2);
        assert_eq!(cwd[0].line, 4, "the line of the method name");
        assert!(cwd[0].root_pos.is_some(), "rooted at Command::new");
        assert_eq!(cwd[1].root_ident.as_deref(), Some("c"));
        assert_eq!(s.fn_name(cwd[0].func), "f");
    }

    #[test]
    fn calls_inside_macros_and_asserts_are_seen() {
        let s = parse(
            "fixture.rs",
            "fn f() { assert!(x.contains(\"A\")); println!(\"{}\", y.contains(\"B\")); }",
        );
        let c: Vec<(&str, bool)> = s
            .prod
            .calls
            .iter()
            .map(|c| (c.name.as_str(), c.in_assert))
            .collect();
        assert_eq!(c, [("contains", true), ("contains", false)]);
    }

    /// Every macro body shape shows its calls: a `select!` arm, a tracing
    /// `target:` and `field = %x` form, a `vec![x; n]` repeat, a nested group.
    #[test]
    fn calls_inside_every_macro_shape_are_seen() {
        let found = |body: &str, name: &str| {
            let s = parse("f.rs", &format!("fn f() {{ {body} }}"));
            s.prod.calls_named(name).count()
        };
        let plain = "let _ = std::env::var(\"X\");";
        assert_eq!(found(plain, "var"), 1, "baseline");
        for (what, body) in [
            (
                "select! arm body",
                "tokio::select! { _ = tick.tick() => { let _ = std::env::var(\"X\"); } }",
            ),
            (
                "select! future",
                "tokio::select! { v = std::env::var(\"X\") => {} else => {} }",
            ),
            (
                "select! guard",
                "tokio::select! { biased; _ = f(), if std::env::var(\"X\").is_ok() => {} }",
            ),
            (
                "tracing target",
                "tracing::info!(target: \"t\", \"m {}\", std::env::var(\"X\").is_ok());",
            ),
            (
                "tracing field",
                "tracing::info!(v = %std::env::var(\"X\").unwrap(), \"m\");",
            ),
            ("repeat", "let _ = vec![std::env::var(\"X\"); 2];"),
            ("array repeat", "let _ = [std::env::var(\"X\"); 2];"),
            (
                "json group",
                "let _ = json!({ \"k\": std::env::var(\"X\") });",
            ),
        ] {
            assert_eq!(found(body, "var"), 1, "{what}");
        }
        // A scanned method call and a field read are seen too.
        let s = parse("f.rs", "fn f() { info!(n = %o.size, m = %o.name.len()); }");
        assert_eq!(s.prod.calls_named("len").count(), 1);
        assert!(s.prod.fields.iter().any(|f| f.name == "size"));
        // A scanned call keeps its arguments.
        let s = parse("f.rs", "fn f() { info!(v = %m.contains(\"A\")); }");
        let c = s.prod.calls_named("contains").next().unwrap();
        assert_eq!(c.arg_strings, ["A"]);
    }

    #[test]
    fn cfg_predicates_require_test_only_when_they_must() {
        let facts = |attr: &str| {
            let s = parse("f.rs", &format!("{attr} fn g() {{ a.x(); }}"));
            (s.prod.calls.len(), s.test.calls.len())
        };
        for (attr, test_only) in [
            ("#[cfg(test)]", true),
            ("#[cfg(all(test, unix))]", true),
            ("#[cfg(all(unix, test))]", true),
            ("#[cfg(all(unix, any(test, test)))]", true),
            ("#[cfg(any(test, test))]", true),
            ("#[cfg(any(test, feature = \"x\"))]", false),
            ("#[cfg(any(test, unix))]", false),
            ("#[cfg(not(test))]", false),
            ("#[cfg(all(not(test), unix))]", false),
            ("#[cfg(feature = \"test\")]", false),
            ("#[cfg(feature = \"testing\")]", false),
            ("#[cfg(all(feature = \"testing\", unix))]", false),
            ("#[cfg(all(feature = \"integration-test\", unix))]", false),
            ("#[cfg(unix)]", false),
            ("", false),
        ] {
            let (prod, test) = facts(attr);
            assert_eq!(
                (prod, test),
                if test_only { (0, 1) } else { (1, 0) },
                "{attr}"
            );
        }
    }

    #[test]
    fn paths_see_aliases_qualified_forms_and_patterns() {
        let s = parse(
            "fixture.rs",
            "use a::b::Cmd as C;\nfn f() { C::new(1); <T as From<u8>>::from(1); let _ = X::Unit; if let X::Unit = v {} matches!(v, X::Unit); }",
        );
        let p = &s.prod;
        assert!(p
            .paths
            .iter()
            .any(|p| p.segs == ["a", "b", "Cmd"] && p.alias.as_deref() == Some("C")));
        assert!(p.calls.iter().any(|c| c.segs == ["T", "from"]));
        assert!(p.mentions("a"));
        // `X::Unit` is a value once; the pattern and the `matches!` arm are not.
        let values = p
            .paths
            .iter()
            .filter(|p| p.expr && p.segs == ["X", "Unit"])
            .count();
        assert_eq!(values, 1);
    }

    #[test]
    fn struct_patterns_field_reads_and_writes_are_facts() {
        let s = parse(
            "fixture.rs",
            "fn f(o: O, n: u8) { let O { body, .. } = o; let _ = o.body; let _ = o.body(); \
             o.t = 1; o.u += 1; g(&mut o.v); let _ = O { w: 1, n, ..d }; }",
        );
        let p = &s.prod;
        assert_eq!(p.pat_fields.len(), 1);
        assert_eq!(
            p.fields.iter().filter(|f| f.name == "body").count(),
            1,
            "o.body() is a call"
        );
        let mut w: Vec<&str> = p.field_writes.iter().map(|f| f.name.as_str()).collect();
        w.sort();
        assert_eq!(
            w,
            ["n", "t", "u", "v", "w"],
            "assign, compound, &mut, literal, shorthand"
        );
        assert_eq!(p.struct_lits.len(), 1);
        assert!(p.struct_lits[0].rest);
    }

    #[test]
    fn the_whole_tree_parses() {
        assert!(src().len() > 100, "syn parsed the sources");
        assert!(tests().len() > 50, "syn parsed tests/");
        assert!(
            src().iter().any(|s| !s.test.calls.is_empty()),
            "test code is split out"
        );
    }
}
