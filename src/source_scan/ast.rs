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
//! Macro bodies are parsed as a comma list of expressions (`assert!`,
//! `format!`, `matches!`, `tracing::info!` ...), so a call inside one is seen.
//! A macro body that is not an expression list (`json!({..})`) is skipped.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use proc_macro2::Span;
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
    /// Every string literal in the arguments (any depth, macro bodies too).
    pub arg_strings: Vec<String>,
    /// Per argument: the ident when it is a plain path (`&x`, `x`).
    pub arg_idents: Vec<Option<String>>,
    /// Every macro in the arguments, as `(name, tokens text)`.
    pub arg_macros: Vec<(String, String)>,
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
}

impl Side {
    fn of(e: &Expr) -> Side {
        Side {
            strings: string_literals(e),
            method: match e {
                Expr::MethodCall(m) => Some(m.method.to_string()),
                _ => None,
            },
        }
    }
}

/// A `let x = init;`, `const X: T = init;` or `static`.
pub(crate) struct Binding {
    pub func: usize,
    pub name: String,
    /// Every macro in the initialiser, as `(name, tokens text)`.
    pub init_macros: Vec<(String, String)>,
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
}

/// A struct pattern `Path { field, .. }` (a destructuring read of `field`).
pub(crate) struct PatField {
    pub line: usize,
    pub func: usize,
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
    /// `fn_names[id - 1]` is the name of fn `id`.
    pub fn_names: Vec<String>,
}

impl Facts {
    pub(crate) fn fn_name(&self, id: usize) -> &str {
        if id == 0 {
            ""
        } else {
            &self.fn_names[id - 1]
        }
    }
}

/// One parsed file.
pub(crate) struct Source {
    /// Path relative to the crate root, `/` separators.
    pub rel: String,
    pub facts: Facts,
}

impl Source {
    /// Does any path in the file start at `ident` (`use aws_sdk_s3::..`,
    /// `aws_sdk_s3::Client`)?
    pub(crate) fn mentions(&self, ident: &str) -> bool {
        self.facts
            .paths
            .iter()
            .any(|p| p.segs.first().is_some_and(|s| s == ident))
    }
}

/// Which items a scan sees.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Production code: `#[cfg(test)]` items and `#[test]` fns are left out.
    Prod,
    /// Everything (the files of `tests/`, where every fn is a test).
    All,
}

/// Parse `text` as the file `rel`. Fixtures of the guards use this.
pub(crate) fn parse(rel: &str, text: &str, scope: Scope) -> Source {
    let file = syn::parse_file(text).unwrap_or_else(|e| {
        let at = e.span().start();
        panic!(
            "{rel}:{}:{}: syn cannot parse the file: {e}",
            at.line, at.column
        )
    });
    let mut c = Collector {
        skip_tests: scope == Scope::Prod,
        func: vec![0],
        assert_depth: 0,
        facts: Facts::default(),
    };
    c.visit_file(&file);
    Source {
        rel: rel.to_string(),
        facts: c.facts,
    }
}

type Cache = Mutex<HashMap<String, &'static [Source]>>;

fn cached(key: String, build: impl FnOnce() -> Vec<Source>) -> &'static [Source] {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let mut map = CACHE.get_or_init(Default::default).lock().unwrap();
    // Parsed once per test binary; the guards share the result.
    *map.entry(key)
        .or_insert_with(|| Box::leak(build().into_boxed_slice()))
}

/// Every production source file of `src/` (the out-of-line test modules are
/// left out), parsed once, `#[cfg(test)]` items removed.
pub(crate) fn prod() -> &'static [Source] {
    cached("prod:src".into(), || {
        super::prod_sources("src")
            .into_iter()
            .map(|(rel, text)| parse(&rel, &text, Scope::Prod))
            .collect()
    })
}

/// Every `.rs` file under `dir` (relative to the crate root), all items.
pub(crate) fn all(dir: &str) -> &'static [Source] {
    cached(format!("all:{dir}"), || {
        super::rust_files(dir)
            .into_iter()
            .map(|f| {
                parse(
                    &super::rel(&f),
                    &std::fs::read_to_string(&f).unwrap(),
                    Scope::All,
                )
            })
            .collect()
    })
}

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

/// A macro body read as a comma list of expressions (`assert!(a, "m")`,
/// `format!(..)`, `matches!(x, Pat)`); empty when it is not one.
pub(crate) fn macro_exprs(m: &syn::Macro) -> Vec<Expr> {
    m.parse_body_with(Punctuated::<Expr, syn::Token![,]>::parse_terminated)
        .map(|p| p.into_iter().collect())
        .unwrap_or_default()
}

fn is_assert_macro(m: &syn::Macro) -> bool {
    m.path.segments.last().is_some_and(|s| {
        let n = s.ident.to_string();
        n.starts_with("assert") || n.starts_with("debug_assert")
    })
}

// ----- the collector ----------------------------------------------------------

struct Collector {
    skip_tests: bool,
    /// Stack of fn ids; the last one is the enclosing fn (0 = none).
    func: Vec<usize>,
    assert_depth: usize,
    facts: Facts,
}

fn cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        if !a.path().is_ident("cfg") {
            return false;
        }
        let t = match &a.meta {
            syn::Meta::List(l) => l.tokens.to_string(),
            _ => String::new(),
        };
        // `cfg(test)` and `cfg(all(test, ..))`; `cfg(not(test))` is production.
        t == "test" || (t.starts_with("all (") && t.contains("test") && !t.contains("not"))
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
    fn hidden(&self, attrs: &[Attribute]) -> bool {
        self.skip_tests && (cfg_test(attrs) || test_attr(attrs))
    }

    fn func(&self) -> usize {
        *self.func.last().unwrap()
    }

    fn enter(&mut self, name: &syn::Ident) {
        self.facts.fn_names.push(name.to_string());
        self.func.push(self.facts.fn_names.len());
    }

    fn leave(&mut self) {
        self.func.pop();
    }

    fn root_pos(e: &Expr) -> Option<Pos> {
        match chain_root(e) {
            Expr::Call(c) => Some(pos(c.span())),
            _ => None,
        }
    }
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_item(&mut self, i: &'ast Item) {
        if !self.hidden(item_attrs(i)) {
            visit::visit_item(self, i);
        }
    }

    fn visit_impl_item(&mut self, i: &'ast ImplItem) {
        let attrs = match i {
            ImplItem::Fn(f) => &f.attrs,
            ImplItem::Const(c) => &c.attrs,
            ImplItem::Macro(m) => &m.attrs,
            _ => &[][..],
        };
        if !self.hidden(attrs) {
            visit::visit_impl_item(self, i);
        }
    }

    fn visit_trait_item(&mut self, i: &'ast TraitItem) {
        let attrs = match i {
            TraitItem::Fn(f) => &f.attrs,
            TraitItem::Const(c) => &c.attrs,
            TraitItem::Macro(m) => &m.attrs,
            _ => &[][..],
        };
        if !self.hidden(attrs) {
            visit::visit_trait_item(self, i);
        }
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.enter(&i.sig.ident);
        visit::visit_item_fn(self, i);
        self.leave();
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.enter(&i.sig.ident);
        visit::visit_impl_item_fn(self, i);
        self.leave();
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.enter(&i.sig.ident);
        visit::visit_trait_item_fn(self, i);
        self.leave();
    }

    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.facts.bindings.push(Binding {
            func: self.func(),
            name: i.ident.to_string(),
            init_macros: macros_in(&i.expr),
            init_root_pos: Self::root_pos(&i.expr),
        });
        visit::visit_item_const(self, i);
    }

    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.facts.bindings.push(Binding {
            func: self.func(),
            name: i.ident.to_string(),
            init_macros: macros_in(&i.expr),
            init_root_pos: Self::root_pos(&i.expr),
        });
        visit::visit_item_static(self, i);
    }

    fn visit_local(&mut self, l: &'ast syn::Local) {
        if let Some(init) = &l.init {
            let mut pat = &l.pat;
            while let Pat::Type(t) = pat {
                pat = &t.pat;
            }
            if let Pat::Ident(id) = pat {
                self.facts.bindings.push(Binding {
                    func: self.func(),
                    name: id.ident.to_string(),
                    init_macros: macros_in(&init.expr),
                    init_root_pos: Self::root_pos(&init.expr),
                });
            }
        }
        visit::visit_local(self, l);
    }

    fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
        let root = chain_root(&m.receiver);
        self.facts.calls.push(Call {
            line: m.method.span().start().line,
            pos: pos(m.method.span()),
            func: self.func(),
            name: m.method.to_string(),
            method: true,
            segs: Vec::new(),
            generics: m
                .turbofish
                .as_ref()
                .map(|t| {
                    let mut p = syn::Path::from(m.method.clone());
                    p.segments[0].arguments = syn::PathArguments::AngleBracketed(t.clone());
                    generic_idents(&p)
                })
                .unwrap_or_default(),
            argc: m.args.len(),
            arg_strings: m.args.iter().flat_map(string_literals).collect(),
            arg_idents: m.args.iter().map(as_ident).collect(),
            arg_macros: m.args.iter().flat_map(macros_in).collect(),
            root_pos: Self::root_pos(&m.receiver),
            root_ident: path_leaf(root),
            in_assert: self.assert_depth > 0,
        });
        visit::visit_expr_method_call(self, m);
    }

    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        if let Expr::Path(p) = &*c.func {
            let segs = expr_path_segs(p);
            self.facts.calls.push(Call {
                line: p.span().start().line,
                pos: pos(c.span()),
                func: self.func(),
                name: segs.join("::"),
                method: false,
                segs,
                generics: generic_idents(&p.path),
                argc: c.args.len(),
                arg_strings: c.args.iter().flat_map(string_literals).collect(),
                arg_idents: c.args.iter().map(as_ident).collect(),
                arg_macros: c.args.iter().flat_map(macros_in).collect(),
                root_pos: None,
                root_ident: None,
                in_assert: self.assert_depth > 0,
            });
        }
        visit::visit_expr_call(self, c);
    }

    fn visit_expr_field(&mut self, f: &'ast syn::ExprField) {
        if let syn::Member::Named(n) = &f.member {
            self.facts.fields.push(Field {
                line: n.span().start().line,
                func: self.func(),
                name: n.to_string(),
            });
        }
        visit::visit_expr_field(self, f);
    }

    fn visit_expr_binary(&mut self, b: &'ast syn::ExprBinary) {
        if matches!(b.op, syn::BinOp::Eq(_) | syn::BinOp::Ne(_)) {
            self.facts.cmps.push(Cmp {
                line: b.op.span().start().line,
                func: self.func(),
                lhs: Side::of(&b.left),
                rhs: Side::of(&b.right),
                in_assert: self.assert_depth > 0,
            });
        }
        visit::visit_expr_binary(self, b);
    }

    fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
        self.facts.paths.push(PathUse {
            line: p.span().start().line,
            func: self.func(),
            segs: expr_path_segs(p),
            alias: None,
        });
        visit::visit_expr_path(self, p);
    }

    fn visit_path(&mut self, p: &'ast syn::Path) {
        self.facts.paths.push(PathUse {
            line: p.span().start().line,
            func: self.func(),
            segs: segs_of(p),
            alias: None,
        });
        visit::visit_path(self, p);
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
        for (segs, alias, line) in out {
            self.facts.paths.push(PathUse {
                line,
                func: self.func(),
                segs,
                alias,
            });
        }
        // Not `visit::visit_use_tree`: the leaves are recorded whole above.
    }

    fn visit_pat_struct(&mut self, p: &'ast syn::PatStruct) {
        if let Some(last) = p.path.segments.last() {
            for f in &p.fields {
                if let syn::Member::Named(n) = &f.member {
                    self.facts.pat_fields.push(PatField {
                        line: n.span().start().line,
                        func: self.func(),
                        path_last: last.ident.to_string(),
                        field: n.to_string(),
                    });
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
        for e in macro_exprs(m) {
            self.visit_expr(&e);
        }
        if assert {
            self.assert_depth -= 1;
        }
        visit::visit_macro(self, m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(text: &str) -> Source {
        parse("fixture.rs", text, Scope::Prod)
    }

    #[test]
    fn cfg_test_items_and_test_fns_are_left_out_of_a_prod_scan() {
        let s = src("fn prod() { a.one(); }\n\
             #[cfg(test)] fn only_in_tests() { a.two(); }\n\
             fn also_prod() { a.three(); }\n\
             #[cfg(test)] mod tests { fn t() { a.four(); } }\n\
             #[test] fn unit() { a.five(); }\n\
             #[cfg(not(test))] fn real() { a.six(); }\n\
             impl X { #[cfg(test)] fn h(&self) { a.seven(); } fn p(&self) { a.eight(); } }\n");
        let names: Vec<&str> = s.facts.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["one", "three", "six", "eight"]);
        let all = parse("f.rs", "#[test] fn unit() { a.five(); }", Scope::All);
        assert_eq!(all.facts.calls.len(), 1, "an All scan keeps test fns");
    }

    #[test]
    fn calls_carry_line_fn_and_chain_root_even_when_split() {
        let s = src("fn f() {\n    let c = Command::new(B)\n        .arg(1)\n        .current_dir(d);\n    c.arg(2)\n        .current_dir(e);\n}\n");
        let cwd: Vec<&Call> = s
            .facts
            .calls
            .iter()
            .filter(|c| c.name == "current_dir")
            .collect();
        assert_eq!(cwd.len(), 2);
        assert_eq!(cwd[0].line, 4, "the line of the method name");
        assert!(cwd[0].root_pos.is_some(), "rooted at Command::new");
        assert_eq!(cwd[1].root_ident.as_deref(), Some("c"));
        assert_eq!(s.facts.fn_name(cwd[0].func), "f");
    }

    #[test]
    fn calls_inside_macros_and_asserts_are_seen() {
        let s = src("fn f() { assert!(x.contains(\"A\")); println!(\"{}\", y.contains(\"B\")); }");
        let c: Vec<(&str, bool)> = s
            .facts
            .calls
            .iter()
            .map(|c| (c.name.as_str(), c.in_assert))
            .collect();
        assert_eq!(c, [("contains", true), ("contains", false)]);
    }

    #[test]
    fn paths_see_aliases_and_qualified_forms() {
        let s = src("use a::b::Cmd as C;\nfn f() { C::new(1); <T as From<u8>>::from(1); }");
        assert!(s
            .facts
            .paths
            .iter()
            .any(|p| p.segs == ["a", "b", "Cmd"] && p.alias.as_deref() == Some("C")));
        assert!(s.facts.calls.iter().any(|c| c.segs == ["T", "from"]));
        assert!(s.mentions("a"));
    }

    #[test]
    fn struct_patterns_and_field_reads_are_facts() {
        let s = src("fn f(o: O) { let O { body, .. } = o; let _ = o.body; let _ = o.body(); }");
        assert_eq!(s.facts.pat_fields.len(), 1);
        assert_eq!(s.facts.fields.len(), 1, "o.body() is a call, not a field");
    }

    #[test]
    fn the_whole_tree_parses() {
        assert!(prod().len() > 100, "syn parsed the production sources");
        assert!(all("tests").len() > 50, "syn parsed tests/");
    }
}
