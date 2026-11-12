//! Locate `sqlx::query*!` macros and `sqlx::query*(...)` function calls, and the
//! byte span of their SQL string-literal argument, using `syn` + `proc-macro2`
//! span locations.

use std::collections::{HashMap, HashSet};

use proc_macro2::{Delimiter, Group, Literal, Spacing, TokenStream, TokenTree};
use syn::{parse::Parser, punctuated::Punctuated, visit::Visit};

use crate::line_index::LineIndex;

/// One inline SQL literal located in a source file.
#[derive(Debug, Clone)]
pub struct QueryLiteral {
    /// Byte range of the literal token (including delimiters) in the source.
    pub start_byte: usize,
    pub end_byte: usize,
    /// 1-based line where the literal token starts (for diagnostics).
    pub line: usize,
    /// Exact source text of the literal, e.g. `r#"SELECT 1"#`.
    pub text: String,
}

const QUERY_MACROS: &[&str] = &[
    "query",
    "query_as",
    "query_scalar",
    "query_unchecked",
    "query_as_unchecked",
    "query_scalar_unchecked",
];

/// The runtime query functions sqlx exports at its crate root. Each takes the
/// SQL as its first argument.
const QUERY_FUNCTIONS: &[&str] = &[
    "query",
    "query_with",
    "query_as",
    "query_as_with",
    "query_scalar",
    "query_scalar_with",
];

/// The query macros whose first argument is the record type, so that the SQL is
/// their second argument.
const RECORD_TYPE_MACROS: &[&str] = &["query_as", "query_as_unchecked"];

/// Extract every inline query literal. Returns empty (not an error) if the file
/// does not parse; use [`extract_checked`] to distinguish "no queries" from
/// "file didn't parse" so callers can report it instead of silently skipping.
pub fn extract(src: &str) -> Vec<QueryLiteral> {
    extract_checked(src).unwrap_or_default()
}

/// Why a file is not valid Rust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line in the file that `syn`'s error points at.
    pub line: usize,
    /// 1-based column in that line, counted in chars.
    pub col: usize,
    /// `syn`'s message.
    pub message: String,
}

/// Like [`extract`], but returns the `syn` parse error so callers can report
/// that a whole file was skipped (rather than hiding its queries).
pub fn extract_checked(src: &str) -> Result<Vec<QueryLiteral>, ParseError> {
    // Cheap byte-substring gate before the expensive `syn::parse_file`: every
    // supported macro and function name (`query`, `query_as`, ...) contains
    // "query", so a file without that substring has no inline SQL and need not
    // be parsed at all. In a typical codebase most `.rs` files hold no query, so
    // this skips the bulk of the parsing work. A file that fails to parse but
    // contains no "query" is reported as having no queries (Ok-empty), never as
    // skipped, which is the correct outcome.
    if !src.contains("query") {
        return Ok(Vec::new());
    }
    let file = syn::parse_file(src).map_err(|e| {
        // An error at the end of the file has no token to point at, so its span
        // covers no source text and starts at line 1. Point after the last token.
        let (line, col) = if e.span().source_text().is_none() {
            let text = src.trim_end();
            let last_line = text.rsplit('\n').next().unwrap_or_default();
            (text.matches('\n').count() + 1, last_line.chars().count() + 1)
        } else {
            let start = e.span().start();
            (start.line, start.column + 1)
        };
        ParseError { line, col, message: e.to_string() }
    })?;
    let mut v = Visitor::default();
    v.visit_file(&file);

    let index = LineIndex::new(src);
    Ok(v.found
        .into_iter()
        .filter_map(|lit| {
            let span = lit.span();
            let start_byte = index.offset(src, span.start())?;
            let end_byte = index.offset(src, span.end())?;
            let text = src.get(start_byte..end_byte)?.to_string();
            Some(QueryLiteral { start_byte, end_byte, line: span.start().line, text })
        })
        .collect())
}

#[derive(Default)]
struct Visitor {
    /// The scopes around the node being visited, innermost last.
    scopes: Vec<Scope>,
    /// Names that `extern crate sqlx as NAME;` at the top of the file gives the
    /// sqlx crate; like the crate name, they are visible in every module.
    crate_aliases: HashSet<String>,
    /// Whether the file has `#[macro_use] extern crate sqlx;`.
    macro_use: bool,
    /// Whether the visited code is inside a `macro_rules!` body. Its paths
    /// resolve where the macro is used, so there only a path ending in
    /// `sqlx::NAME` counts, not a bare name or a crate alias.
    in_macro_rules: bool,
    found: Vec<Literal>,
}

impl<'ast> Visit<'ast> for Visitor {
    fn visit_file(&mut self, file: &'ast syn::File) {
        for item in &file.items {
            if let syn::Item::ExternCrate(krate) = item
                && krate.ident == "sqlx"
            {
                if let Some((_, alias)) = &krate.rename {
                    self.crate_aliases.insert(alias.to_string());
                }
                if krate.attrs.iter().any(|a| a.path().is_ident("macro_use")) {
                    self.macro_use = true;
                }
            }
        }
        self.scopes.push(Scope::new(true, &file.items));
        syn::visit::visit_file(self, file);
        self.scopes.pop();
    }

    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        let Some((_, items)) = &module.content else {
            return syn::visit::visit_item_mod(self, module);
        };
        self.scopes.push(Scope::new(true, items));
        syn::visit::visit_item_mod(self, module);
        self.scopes.pop();
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        let items = block.stmts.iter().filter_map(|stmt| match stmt {
            syn::Stmt::Item(item) => Some(item),
            _ => None,
        });
        self.scopes.push(Scope::new(false, items));
        for stmt in &block.stmts {
            self.visit_stmt(stmt);
            // A `let` binding is visible from the next statement on.
            if let syn::Stmt::Local(local) = stmt
                && let Some(scope) = self.scopes.last_mut()
            {
                scope.bind(&local.pat);
            }
        }
        self.scopes.pop();
    }

    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        self.with_scope(Scope::binding(parameters(&f.sig)), |v| syn::visit::visit_item_fn(v, f));
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        self.with_scope(Scope::binding(parameters(&f.sig)), |v| syn::visit::visit_impl_item_fn(v, f));
    }

    fn visit_trait_item_fn(&mut self, f: &'ast syn::TraitItemFn) {
        self.with_scope(Scope::binding(parameters(&f.sig)), |v| syn::visit::visit_trait_item_fn(v, f));
    }

    fn visit_expr_closure(&mut self, closure: &'ast syn::ExprClosure) {
        self.with_scope(Scope::binding(&closure.inputs), |v| syn::visit::visit_expr_closure(v, closure));
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        self.with_scope(Scope::binding([&arm.pat]), |v| syn::visit::visit_arm(v, arm));
    }

    fn visit_expr_if(&mut self, expr: &'ast syn::ExprIf) {
        self.with_scope(Scope::default(), |v| {
            v.visit_condition(&expr.cond);
            v.visit_block(&expr.then_branch);
        });
        if let Some((_, else_branch)) = &expr.else_branch {
            self.visit_expr(else_branch);
        }
    }

    fn visit_expr_while(&mut self, expr: &'ast syn::ExprWhile) {
        self.with_scope(Scope::default(), |v| {
            v.visit_condition(&expr.cond);
            v.visit_block(&expr.body);
        });
    }

    fn visit_expr_for_loop(&mut self, expr: &'ast syn::ExprForLoop) {
        self.visit_expr(&expr.expr);
        self.with_scope(Scope::binding([&*expr.pat]), |v| v.visit_block(&expr.body));
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        let segments = path_segments(&m.path);
        self.visit_macro_call(m.path.leading_colon.is_some(), &segments, m.tokens.clone());
        syn::visit::visit_macro(self, m);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(func) = &*call.func
            && func.qself.is_none()
            && self
                .query_name(func.path.leading_colon.is_some(), &path_segments(&func.path), Namespace::Function)
                .is_some()
            && let Some(syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(sql), .. })) =
                call.args.first()
        {
            self.found.push(sql.token());
        }
        syn::visit::visit_expr_call(self, call);
    }
}

impl Visitor {
    fn with_scope(&mut self, scope: Scope, visit: impl FnOnce(&mut Self)) {
        self.scopes.push(scope);
        visit(self);
        self.scopes.pop();
    }

    /// Visit the condition of an `if` or `while` inside the scope of its body,
    /// which sees the bindings of its `let`s, as do the conditions after them.
    fn visit_condition(&mut self, cond: &syn::Expr) {
        match cond {
            syn::Expr::Binary(b) if matches!(b.op, syn::BinOp::And(_)) => {
                self.visit_condition(&b.left);
                self.visit_condition(&b.right);
            }
            syn::Expr::Let(l) => {
                self.visit_expr(&l.expr);
                if let Some(scope) = self.scopes.last_mut() {
                    scope.bind(&l.pat);
                }
            }
            _ => self.visit_expr(cond),
        }
    }

    /// Visit a macro invocation `PATH!(tokens)`, found in the syntax tree or in
    /// the tokens of another macro.
    fn visit_macro_call(&mut self, leading_colon: bool, segments: &[String], tokens: TokenStream) {
        if let Some(name) = self.query_name(leading_colon, segments, Namespace::Macro) {
            if let Some(lit) = sql_literal(tokens, name) {
                self.found.push(lit);
            }
        } else if !leading_colon && segments == ["macro_rules"] {
            if self.may_name_a_query(&tokens) {
                let outer = std::mem::replace(&mut self.in_macro_rules, true);
                self.scan(tokens);
                self.in_macro_rules = outer;
            }
        } else if let Ok(args) = Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated.parse2(tokens.clone()) {
            // Most macros (`assert!`, `vec!`, `tokio::try_join!`, ...) take
            // expressions, which can hold queries.
            for arg in &args {
                self.visit_expr(arg);
            }
        } else if self.may_name_a_query(&tokens) {
            self.scan(tokens);
        }
    }

    /// Find queries in tokens that are not Rust syntax as a whole, such as the
    /// body of a `tokio::select!` or of a `macro_rules!`: every `PATH!(...)`
    /// and `PATH(...)` in them, and every group that parses as Rust.
    fn scan(&mut self, tokens: TokenStream) {
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        let mut at = 0;
        while at < tokens.len() {
            // A name after a `.` is a method or a field, not a path.
            let after_dot = at > 0 && is_punct(&tokens[at - 1], '.');
            if !after_dot && let Some((leading_colon, segments, end)) = path_at(&tokens, at) {
                at = end;
                if tokens.get(at).is_some_and(|tt| is_punct(tt, '!'))
                    && let Some(TokenTree::Group(args)) = tokens.get(at + 1)
                {
                    self.visit_macro_call(leading_colon, &segments, args.stream());
                    at += 2;
                    continue;
                }
                let args_at = skip_turbofish(&tokens, at);
                if let Some(TokenTree::Group(args)) = tokens.get(args_at)
                    && args.delimiter() == Delimiter::Parenthesis
                {
                    if self.query_name(leading_colon, &segments, Namespace::Function).is_some()
                        && let Some(lit) = argument_literal(&args.stream().into_iter().collect::<Vec<_>>())
                    {
                        self.found.push(lit);
                    }
                    self.visit_group(args);
                    at = args_at + 1;
                }
                continue;
            }
            if let TokenTree::Group(group) = &tokens[at] {
                self.visit_group(group);
            }
            at += 1;
        }
    }

    /// Visit a group of tokens as the Rust syntax it holds: a block, or a list
    /// of expressions; search the tokens of any other group.
    fn visit_group(&mut self, group: &Group) {
        match group.delimiter() {
            Delimiter::Brace => {
                let tokens = TokenStream::from(TokenTree::Group(group.clone()));
                if let Ok(block) = syn::parse2::<syn::Block>(tokens) {
                    self.visit_block(&block);
                    return;
                }
            }
            Delimiter::Parenthesis | Delimiter::Bracket => {
                let parse = Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated;
                if let Ok(args) = parse.parse2(group.stream()) {
                    for arg in &args {
                        self.visit_expr(arg);
                    }
                    return;
                }
            }
            Delimiter::None => {}
        }
        self.scan(group.stream());
    }

    /// Whether the tokens hold a name that can call a query function or macro
    /// here. Only then are the tokens of a macro searched one by one.
    fn may_name_a_query(&self, tokens: &TokenStream) -> bool {
        tokens.clone().into_iter().any(|tt| match tt {
            TokenTree::Ident(ident) => {
                let name = ident.to_string();
                QUERY_FUNCTIONS.contains(&name.as_str())
                    || QUERY_MACROS.contains(&name.as_str())
                    || self
                        .enclosing_scopes()
                        .any(|scope| scope.functions.contains_key(&name) || scope.macros.contains_key(&name))
            }
            TokenTree::Group(group) => self.may_name_a_query(&group.stream()),
            _ => false,
        })
    }

    /// The name of the sqlx query function or macro that a path names:
    /// `sqlx::NAME` or any other path ending in `sqlx::NAME` (sqlx re-exported
    /// by another crate or module), `NAME` under a crate alias, or a bare `NAME`
    /// that resolves to an import from sqlx. A bare `query` without that import
    /// is rejected because other crates export functions and macros with the
    /// same names.
    fn query_name(&self, leading_colon: bool, segments: &[String], namespace: Namespace) -> Option<&'static str> {
        let (name, prefix) = segments.split_last()?;
        match prefix {
            [] if !leading_colon && !self.in_macro_rules => self.resolve(name, namespace),
            [.., krate] if krate == "sqlx" => namespace.query_name(name),
            [krate] if !self.in_macro_rules && self.names_a_crate_alias(krate, leading_colon) => {
                namespace.query_name(name)
            }
            _ => None,
        }
    }

    fn names_a_crate_alias(&self, name: &str, leading_colon: bool) -> bool {
        self.crate_aliases.contains(name)
            || !leading_colon && self.enclosing_scopes().any(|scope| scope.crates.contains(name))
    }

    fn resolve(&self, name: &str, namespace: Namespace) -> Option<&'static str> {
        match self.enclosing_scopes().find_map(|scope| scope.lookup(name, namespace)) {
            Some(Meaning::Sqlx(name)) => Some(name),
            Some(Meaning::Other) => None,
            None if matches!(namespace, Namespace::Macro) && self.macro_use => namespace.query_name(name),
            None => None,
        }
    }

    /// The scopes whose names the visited node can see: the innermost ones up to
    /// and including the nearest module.
    fn enclosing_scopes(&self) -> impl Iterator<Item = &Scope> {
        let module = self.scopes.iter().rposition(|scope| scope.module).unwrap_or(0);
        self.scopes[module..].iter().rev()
    }
}

#[derive(Clone, Copy)]
enum Namespace {
    Function,
    Macro,
}

impl Namespace {
    /// `name` as one of sqlx's query functions or macros in this namespace.
    fn query_name(self, name: &str) -> Option<&'static str> {
        let names = match self {
            Self::Function => QUERY_FUNCTIONS,
            Self::Macro => QUERY_MACROS,
        };
        names.iter().find(|known| **known == name).copied()
    }
}

/// What a scope says a name means.
enum Meaning {
    /// The sqlx query function or macro with this name.
    Sqlx(&'static str),
    Other,
}

/// The names that one module, block, function body or pattern brings into
/// scope.
#[derive(Default)]
struct Scope {
    /// Whether this is a module, which does not see the names around it.
    module: bool,
    /// Local names of sqlx query functions imported by a `use` item, with the
    /// sqlx name of each.
    functions: HashMap<String, &'static str>,
    /// Local names of sqlx query macros imported by a `use` item, as for
    /// `functions`.
    macros: HashMap<String, &'static str>,
    /// Whether a `use sqlx::*;` is in this scope.
    glob: bool,
    /// Local names of the sqlx crate (`use sqlx as db;`).
    crates: HashSet<String>,
    /// Other function names defined or imported here; they shadow a glob import
    /// and the names of the enclosing scopes.
    other_functions: HashSet<String>,
    /// Other macro names defined or imported here, as for `other_functions`.
    other_macros: HashSet<String>,
    /// Variables bound here by a `let`, a parameter or a pattern. They hide
    /// every function of the same name, but no macro.
    locals: HashSet<String>,
}

impl Scope {
    /// The scope of the variables that `pats` bind.
    fn binding<'a>(pats: impl IntoIterator<Item = &'a syn::Pat>) -> Self {
        let mut scope = Self::default();
        for pat in pats {
            scope.bind(pat);
        }
        scope
    }

    fn bind(&mut self, pat: &syn::Pat) {
        Bindings(&mut self.locals).visit_pat(pat);
    }

    fn new<'a>(module: bool, items: impl IntoIterator<Item = &'a syn::Item>) -> Self {
        let mut scope = Self { module, ..Self::default() };
        for item in items {
            match item {
                syn::Item::Use(item) => scope.add_use(&item.tree),
                syn::Item::ExternCrate(krate) if krate.ident == "sqlx" => {
                    if let Some((_, alias)) = &krate.rename {
                        scope.crates.insert(alias.to_string());
                    }
                }
                syn::Item::Fn(f) => {
                    scope.other_functions.insert(f.sig.ident.to_string());
                }
                syn::Item::Const(c) => {
                    scope.other_functions.insert(c.ident.to_string());
                }
                syn::Item::Static(s) => {
                    scope.other_functions.insert(s.ident.to_string());
                }
                syn::Item::Macro(syn::ItemMacro { ident: Some(name), .. }) => {
                    scope.other_macros.insert(name.to_string());
                }
                _ => {}
            }
        }
        scope
    }

    /// What `name` means here, or `None` when this scope says nothing about it.
    fn lookup(&self, name: &str, namespace: Namespace) -> Option<Meaning> {
        let (imported, other) = match namespace {
            Namespace::Function if self.locals.contains(name) => return Some(Meaning::Other),
            Namespace::Function => (&self.functions, &self.other_functions),
            Namespace::Macro => (&self.macros, &self.other_macros),
        };
        if let Some(sqlx_name) = imported.get(name) {
            Some(Meaning::Sqlx(sqlx_name))
        } else if other.contains(name) {
            Some(Meaning::Other)
        } else if self.glob {
            namespace.query_name(name).map(Meaning::Sqlx)
        } else {
            None
        }
    }

    /// Walk the start of a use tree, before a `sqlx` segment has been seen. A
    /// `sqlx` segment after other segments (`use crate::db::sqlx::query;`) is
    /// taken as sqlx re-exported there.
    fn add_use(&mut self, tree: &syn::UseTree) {
        match tree {
            syn::UseTree::Path(p) if p.ident == "sqlx" => self.add_sqlx_use(&p.tree),
            syn::UseTree::Path(p) => self.add_use(&p.tree),
            syn::UseTree::Rename(r) if r.ident == "sqlx" => {
                self.crates.insert(r.rename.to_string());
            }
            syn::UseTree::Group(g) => g.items.iter().for_each(|t| self.add_use(t)),
            _ => self.add_other_use(tree),
        }
    }

    /// Walk the part of a use tree that follows `sqlx::`.
    fn add_sqlx_use(&mut self, tree: &syn::UseTree) {
        match tree {
            syn::UseTree::Name(n) => self.add_sqlx_name(&n.ident, &n.ident),
            syn::UseTree::Rename(r) if r.ident == "self" => {
                self.crates.insert(r.rename.to_string());
            }
            syn::UseTree::Rename(r) => self.add_sqlx_name(&r.ident, &r.rename),
            syn::UseTree::Glob(_) => self.glob = true,
            syn::UseTree::Group(g) => g.items.iter().for_each(|t| self.add_sqlx_use(t)),
            // Deeper paths such as `sqlx::query_builder::...` hold no query
            // functions or macros.
            syn::UseTree::Path(_) => self.add_other_use(tree),
        }
    }

    fn add_sqlx_name(&mut self, name: &syn::Ident, local: &syn::Ident) {
        let name = name.to_string();
        if let Some(function) = Namespace::Function.query_name(&name) {
            self.functions.insert(local.to_string(), function);
        }
        if let Some(mac) = Namespace::Macro.query_name(&name) {
            self.macros.insert(local.to_string(), mac);
        }
    }

    /// Record the names a use tree that does not import from sqlx brings in.
    fn add_other_use(&mut self, tree: &syn::UseTree) {
        match tree {
            syn::UseTree::Path(p) => self.add_other_use(&p.tree),
            syn::UseTree::Group(g) => g.items.iter().for_each(|t| self.add_other_use(t)),
            syn::UseTree::Name(n) if n.ident != "self" => self.add_other_name(&n.ident),
            syn::UseTree::Rename(r) => self.add_other_name(&r.rename),
            _ => {}
        }
    }

    fn add_other_name(&mut self, local: &syn::Ident) {
        self.other_functions.insert(local.to_string());
        self.other_macros.insert(local.to_string());
    }
}

/// Collects the names of the variables that a pattern binds.
struct Bindings<'a>(&'a mut HashSet<String>);

impl<'ast> Visit<'ast> for Bindings<'_> {
    fn visit_pat_ident(&mut self, pat: &'ast syn::PatIdent) {
        self.0.insert(pat.ident.to_string());
        syn::visit::visit_pat_ident(self, pat);
    }

    // A match guard or a constant inside a pattern binds nothing.
    fn visit_expr(&mut self, _: &'ast syn::Expr) {}
}

fn parameters(sig: &syn::Signature) -> impl Iterator<Item = &syn::Pat> {
    sig.inputs.iter().filter_map(|arg| match arg {
        syn::FnArg::Typed(typed) => Some(&*typed.pat),
        _ => None,
    })
}

fn path_segments(path: &syn::Path) -> Vec<String> {
    path.segments.iter().map(|s| s.ident.to_string()).collect()
}

/// The path that starts at `tokens[start]`, as its leading `::`, its segment
/// names, and the index of the token after it. A `$crate` path in a
/// `macro_rules!` body is read from its `crate`.
fn path_at(tokens: &[TokenTree], start: usize) -> Option<(bool, Vec<String>, usize)> {
    let leading_colon = is_path_separator(tokens, start);
    let mut at = if leading_colon { start + 2 } else { start };
    let TokenTree::Ident(first) = tokens.get(at)? else {
        return None;
    };
    let mut segments = vec![first.to_string()];
    at += 1;
    while is_path_separator(tokens, at)
        && let Some(TokenTree::Ident(next)) = tokens.get(at + 2)
    {
        segments.push(next.to_string());
        at += 3;
    }
    Some((leading_colon, segments, at))
}

/// The index after a turbofish (`::<_, Row>`) at `at`, or `at` when there is none.
fn skip_turbofish(tokens: &[TokenTree], at: usize) -> usize {
    if !is_path_separator(tokens, at) || !tokens.get(at + 2).is_some_and(|tt| is_punct(tt, '<')) {
        return at;
    }
    let mut depth = 0usize;
    for (i, tt) in tokens.iter().enumerate().skip(at + 2) {
        let arrow = i > 0
            && matches!(&tokens[i - 1], TokenTree::Punct(p) if p.as_char() == '-' && p.spacing() == Spacing::Joint);
        if is_punct(tt, '<') {
            depth += 1;
        } else if is_punct(tt, '>') && !arrow {
            depth -= 1;
            if depth == 0 {
                return i + 1;
            }
        }
    }
    tokens.len()
}

fn is_path_separator(tokens: &[TokenTree], at: usize) -> bool {
    matches!(tokens.get(at), Some(TokenTree::Punct(p)) if p.as_char() == ':' && p.spacing() == Spacing::Joint)
        && tokens.get(at + 1).is_some_and(|tt| is_punct(tt, ':'))
}

fn is_punct(tt: &TokenTree, c: char) -> bool {
    matches!(tt, TokenTree::Punct(p) if p.as_char() == c)
}

fn is_str_literal(tt: &TokenTree) -> bool {
    let TokenTree::Literal(lit) = tt else {
        return false;
    };
    let s = lit.to_string();
    s.starts_with('"') || s.starts_with("r\"") || s.starts_with("r#")
}

/// The SQL literal of a call to the sqlx query macro `name`: its first
/// argument, or for `query_as!` its second, after the record type. `None` when
/// that argument is not a single string literal: sqlx joins literals written
/// with `+`, and rewriting one of them alone can break the query.
fn sql_literal(tokens: TokenStream, name: &str) -> Option<Literal> {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    if !RECORD_TYPE_MACROS.contains(&name) {
        return argument_literal(&tokens);
    }
    // The record type can hold commas (`Pair<A, B>`), so take the first string
    // literal, and check that all before its comma is one type.
    let at = tokens.iter().position(is_str_literal)?;
    let comma = at.checked_sub(1)?;
    if !is_punct(&tokens[comma], ',') || !is_type(&tokens[..comma]) {
        return None;
    }
    argument_literal(&tokens[at..])
}

/// The string literal that is the whole first argument in `tokens`.
fn argument_literal(tokens: &[TokenTree]) -> Option<Literal> {
    match tokens {
        [first @ TokenTree::Literal(lit), rest @ ..]
            if is_str_literal(first) && rest.first().is_none_or(|tt| is_punct(tt, ',')) =>
        {
            Some(lit.clone())
        }
        _ => None,
    }
}

/// Whether `tokens` are one type. In a `macro_rules!` body, a `$` metavariable
/// (`$t`, `$crate::Row`) is read without its `$`.
fn is_type(tokens: &[TokenTree]) -> bool {
    let tokens: TokenStream = tokens.iter().filter(|tt| !is_punct(tt, '$')).cloned().collect();
    syn::parse2::<syn::Type>(tokens).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_query_as_with_leading_ident() {
        let src = r###"
fn f() {
    let _ = sqlx::query_as!(Row, r#"SELECT 1"#, x);
}
"###;
        let q = extract(src);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].text, "r#\"SELECT 1\"#");
    }

    #[test]
    fn finds_bare_query_and_scalar() {
        let src = r###"
use sqlx::query;
fn f() {
    let _ = query!("SELECT 1");
    let _ = sqlx::query_scalar!(r#"SELECT 2"#);
}
"###;
        let q = extract(src);
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].text, "\"SELECT 1\"");
        assert_eq!(q[1].text, "r#\"SELECT 2\"#");
    }

    #[test]
    fn byte_span_is_exact() {
        let src = "fn f(){let _=sqlx::query!(r#\"SELECT 1\"#);}";
        let q = extract(src);
        assert_eq!(q.len(), 1);
        assert_eq!(&src[q[0].start_byte..q[0].end_byte], "r#\"SELECT 1\"#");
    }

    fn texts(src: &str) -> Vec<String> {
        extract(src).into_iter().map(|q| q.text).collect()
    }

    #[test]
    fn finds_sqlx_function_calls_with_literal_sql() {
        let src = r###"
fn f() {
    let _ = sqlx::query("SELECT 1").bind(x);
    let _ = sqlx::query_as::<_, Row>(r#"SELECT 2"#);
    let _ = ::sqlx::query_scalar("SELECT 3");
    let _ = sqlx::query_with("SELECT 4", args);
    let _ = sqlx::query_as_with::<_, Row, _>("SELECT 5", args);
    let _ = sqlx::query_scalar_with("SELECT 6", args);
}
"###;
        assert_eq!(
            texts(src),
            [
                "\"SELECT 1\"",
                "r#\"SELECT 2\"#",
                "\"SELECT 3\"",
                "\"SELECT 4\"",
                "\"SELECT 5\"",
                "\"SELECT 6\"",
            ]
        );
    }

    #[test]
    fn function_call_byte_span_is_exact() {
        let src = "fn f(){let _=sqlx::query(r#\"SELECT 1\"#).bind(\"x\");}";
        let q = extract(src);
        assert_eq!(q.len(), 1);
        assert_eq!(&src[q[0].start_byte..q[0].end_byte], "r#\"SELECT 1\"#");
    }

    #[test]
    fn ignores_function_calls_without_literal_sql() {
        // Only a string literal written in the call is complete, rewritable SQL.
        let src = r###"
const Q: &str = "SELECT 1";
fn f() {
    let _ = sqlx::query(Q);
    let _ = sqlx::query(&sql);
    let _ = sqlx::query(AssertSqlSafe(format!("SELECT {}", c)));
    let _ = sqlx::query_statement(&stmt);
}
"###;
        assert!(texts(src).is_empty());
    }

    #[test]
    fn ignores_macros_whose_sql_is_not_a_single_string_literal() {
        // sqlx joins literals written with `+`; one of them alone is not the query.
        let src = r###"
fn f() {
    let _ = sqlx::query!("SELECT  id " + "FROM users WHERE id = $1", 1);
    let _ = sqlx::query_as!(Row, "SELECT a " + r#"FROM t"# + "", x);
    let _ = sqlx::query_scalar!(r#"SELECT 1"#
        + "", "bind");
    let _ = sqlx::query!("SELECT 3", "a" + "b");
}
"###;
        assert_eq!(texts(src), ["\"SELECT 3\""]);
    }

    #[test]
    fn ignores_calls_that_are_not_sqlx() {
        let src = r###"
fn f() {
    let _ = client.query("SELECT 1", &[]);
    let _ = other::query("SELECT 2");
    let _ = sqlx::raw_sql("SELECT 3");
    let _ = query("SELECT 4");
    let _ = sqlx::QueryBuilder::new("SELECT * FROM t WHERE ");
}
"###;
        assert!(texts(src).is_empty());
    }

    #[test]
    fn bare_name_needs_an_import_from_sqlx() {
        let src = r###"
use other::query_as;
use sqlx::{Row, query, query_scalar as scalar};
fn f() {
    let _ = query("SELECT 1");
    let _ = scalar("SELECT 2");
    let _ = query_as("SELECT 3");
    let _ = query_scalar("SELECT 4");
}
"###;
        assert_eq!(texts(src), ["\"SELECT 1\"", "\"SELECT 2\""]);
    }

    #[test]
    fn an_import_inside_a_function_counts_only_in_that_function() {
        let src = r###"
fn f() { let _ = query("SELECT 1"); }
fn g() { let _ = query("SELECT 2"); use sqlx::query; }
"###;
        assert_eq!(texts(src), ["\"SELECT 2\""]);
    }

    #[test]
    fn an_import_reaches_the_blocks_nested_in_its_scope() {
        let src = r###"
use sqlx::query;
fn f() {
    let _ = query("SELECT 1");
    let _ = || { let _ = query("SELECT 2"); };
}
fn g() {
    if x { let _ = query_as("SELECT 3"); }
    use sqlx::query_as;
}
fn h() { let _ = query_as("SELECT 4"); }
"###;
        assert_eq!(texts(src), ["\"SELECT 1\"", "\"SELECT 2\"", "\"SELECT 3\""]);
    }

    #[test]
    fn an_import_does_not_reach_other_modules() {
        let src = r###"
mod a { use sqlx::query; }
mod b {
    fn query(s: &str) -> &str { s }
    fn g() { let _ = query("select  a   from  t"); }
}
use sqlx::query_as;
mod c {
    fn h() { let _ = query_as("SELECT 1"); }
}
fn f() {
    let _ = other_crate::query!("select  b   from  u");
}
"###;
        assert!(texts(src).is_empty(), "{:?}", texts(src));
    }

    #[test]
    fn a_local_item_shadows_a_glob_import() {
        let src = r###"
use sqlx::*;
fn query(s: &str) -> &str { s }
fn f() {
    let _ = query("SELECT 1");
    let _ = query_as("SELECT 2");
}
mod m {
    use sqlx::*;
    fn g() {
        fn query_scalar(s: &str) -> &str { s }
        let _ = query_scalar("SELECT 3");
        let _ = query("SELECT 4");
    }
}
"###;
        assert_eq!(texts(src), ["\"SELECT 2\"", "\"SELECT 4\""]);
    }

    #[test]
    fn a_local_closure_named_like_an_imported_function_is_not_sqlx() {
        let src = r###"
use sqlx::query;
pub fn search(index: &Index) -> Vec<String> {
    let query = |text: &str| index.lookup(text);
    query("select   name from   people")
}
"###;
        assert!(texts(src).is_empty(), "{:?}", texts(src));
    }

    #[test]
    fn a_let_binding_hides_the_import_from_the_next_statement_to_the_end_of_its_block() {
        let src = r###"
use sqlx::query;
fn f() {
    let _ = query("SELECT 1");
    let query = query("SELECT 2");
    let _ = query("not sql 3");
    {
        let _ = query("not sql 4");
    }
    let _ = sqlx::query("SELECT 5");
    let _ = query!("SELECT 6");
}
fn g() {
    {
        let query = |s: &str| s;
        let _ = query("not sql 7");
    }
    let _ = query("SELECT 8");
    let Some(query) = query("SELECT 9") else {
        let _ = query("SELECT 10");
        return;
    };
    let _ = query("not sql 11");
}
fn h() {
    let _ = query("SELECT 12");
    {
        use sqlx::query;
        let _ = query("SELECT 13");
    }
}
"###;
        assert_eq!(
            texts(src),
            [
                "\"SELECT 1\"",
                "\"SELECT 2\"",
                "\"SELECT 5\"",
                "\"SELECT 6\"",
                "\"SELECT 8\"",
                "\"SELECT 9\"",
                "\"SELECT 10\"",
                "\"SELECT 12\"",
                "\"SELECT 13\"",
            ]
        );
    }

    #[test]
    fn parameters_and_pattern_bindings_hide_an_imported_function_in_their_scope() {
        let src = r###"
use sqlx::{query, query_as, query_scalar as scalar};
fn params(query: impl Fn(&str)) { query("not sql 1"); }
fn closure() {
    let _ = |query_as: fn(&str)| query_as("not sql 2");
    let _ = query_as("SELECT 3");
}
fn arms(x: Option<F>) {
    match x {
        Some(query) if query("not sql 4") => query("not sql 5"),
        _ => query("SELECT 6"),
    }
    if let Some(scalar) = x { scalar("not sql 7"); } else { scalar("SELECT 8"); }
    while let Some((_, ref mut query)) = it.next() { query("not sql 9"); }
    for Pair { query, .. } in pairs { query("not sql 10"); }
    for query in query("SELECT 11") { query("not sql 12"); }
    let _ = scalar("SELECT 13");
}
struct T;
impl T { fn m(&self, query: F) { query("not sql 14"); } }
trait U { fn d(query @ _: F) { query("not sql 15"); } }
fn after() { let _ = query("SELECT 16"); }
"###;
        assert_eq!(
            texts(src),
            [
                "\"SELECT 3\"",
                "\"SELECT 6\"",
                "\"SELECT 8\"",
                "\"SELECT 11\"",
                "\"SELECT 13\"",
                "\"SELECT 16\"",
            ]
        );
    }

    #[test]
    fn finds_queries_in_macros_whose_arguments_are_not_expressions() {
        let src = r###"
use sqlx::query;
fn f() {
    tokio::select! {
        r = sqlx::query!("SELECT 1").fetch_one(pool) => r,
        r = sqlx::query_scalar::<_, i64>("SELECT 2").fetch_one(pool) => {
            let _ = sqlx::query!("SELECT 3");
            r
        }
        r = query!("SELECT 4").fetch_one(pool) => r,
    }
    custom! { key => sqlx::query_as!(Row, r#"SELECT 5"#), other: [sqlx::query("SELECT 6")] }
}
"###;
        let found = extract(src);
        let texts: Vec<&str> = found.iter().map(|q| q.text.as_str()).collect();
        assert_eq!(
            texts,
            ["\"SELECT 1\"", "\"SELECT 2\"", "\"SELECT 3\"", "\"SELECT 4\"", "r#\"SELECT 5\"#", "\"SELECT 6\""]
        );
        for q in &found {
            assert_eq!(&src[q.start_byte..q.end_byte], q.text);
            assert_eq!(src[..q.start_byte].matches('\n').count() + 1, q.line);
        }
    }

    #[test]
    fn ignores_what_is_not_a_sqlx_query_in_macros_whose_arguments_are_not_expressions() {
        let src = r###"
mod m {
    fn f() {
        tokio::select! {
            r = client.query("not sql 1") => r,
            r = other::query!("not sql 2") => r,
            r = sqlx::raw_sql("not sql 3") => r,
            r = query!("not sql 4") => r,
            r = query("not sql 5") => r,
            _ = sqlx::query!(SQL, "not sql 6") => {}
            _ = sqlx::query("SELECT " + "x") => {}
        }
    }
}
"###;
        assert!(texts(src).is_empty(), "{:?}", texts(src));
    }

    #[test]
    fn finds_full_sqlx_paths_in_macro_rules_bodies() {
        let src = r###"
use sqlx::query;
use sqlx as db;
macro_rules! m {
    () => { sqlx::query!("SELECT 1 AS c") };
    ($pool:expr) => { sqlx::query_as!(Row, "SELECT 2").fetch_one($pool) };
    ($sql:literal) => { sqlx::query!($sql) };
    ($sql:literal, $x:expr) => { sqlx::query!($sql, "not sql 3") };
    ($t:ty) => { ::sqlx::query_as!($t, "SELECT 4") };
    ($t:ty, $sql:literal) => { sqlx::query_as!($t, $sql, "not sql 5") };
    (x) => { $crate::db::sqlx::query("SELECT 6").bind($x) };
    (y) => { query!("not sql 7"); db::query("not sql 8"); query("not sql 9") };
}
fn f() {
    macro_rules! inner { () => { sqlx::query_scalar!("SELECT 10") }; }
}
"###;
        let found = extract(src);
        let texts: Vec<&str> = found.iter().map(|q| q.text.as_str()).collect();
        assert_eq!(
            texts,
            ["\"SELECT 1 AS c\"", "\"SELECT 2\"", "\"SELECT 4\"", "\"SELECT 6\"", "\"SELECT 10\""]
        );
        for q in &found {
            assert_eq!(&src[q.start_byte..q.end_byte], q.text);
            assert_eq!(src[..q.start_byte].matches('\n').count() + 1, q.line);
        }
    }

    #[test]
    fn a_path_ending_in_sqlx_and_a_query_name_counts_whatever_its_prefix() {
        let src = r###"
use crate::db::sqlx::{query_scalar, query as q};
fn f() {
    let _ = crate::db::sqlx::query!("SELECT 1");
    let _ = ::my_crate::sqlx::query("SELECT 2");
    let _ = self::sqlx::query_as::<_, Row>("SELECT 3");
    let _ = a::b::sqlx::query_scalar!("SELECT 4");
    let _ = q("SELECT 5");
    let _ = query_scalar!("SELECT 6");
    let _ = crate::sqlx::db::query!("not sql 7");
    let _ = sqlx_reexport::query("not sql 8");
    let _ = crate::db::sqlx::raw_sql("not sql 9");
    let _ = crate::sqlx::Other::query("not sql 10");
    let _ = crate::db::sqlx::query_with!("not sql 11");
}
"###;
        assert_eq!(
            texts(src),
            [
                "\"SELECT 1\"",
                "\"SELECT 2\"",
                "\"SELECT 3\"",
                "\"SELECT 4\"",
                "\"SELECT 5\"",
                "\"SELECT 6\"",
            ]
        );
    }

    #[test]
    fn a_macro_takes_its_sql_only_from_the_argument_that_holds_it() {
        let src = r###"
fn f() {
    let _ = sqlx::query!("SELECT 1", "bind");
    let _ = sqlx::query_as!(Row, "SELECT 2", "bind");
    let _ = sqlx::query_as!(Pair<A, B>, "SELECT 3");
    let _ = sqlx::query_scalar!(x, "not sql 4");
    let _ = sqlx::query_as!(Row, x, "not sql 5");
}
"###;
        assert_eq!(texts(src), ["\"SELECT 1\"", "\"SELECT 2\"", "\"SELECT 3\""]);
    }

    #[test]
    fn macros_need_the_sqlx_path_or_an_import_from_sqlx() {
        let src = r###"
fn f() {
    let _ = sqlx::query!("SELECT 1");
    let _ = ::sqlx::query_as!(Row, "SELECT 2");
    let _ = other_crate::query!("SELECT 3");
    let _ = graphql::query!("SELECT 4");
    let _ = query!("SELECT 5");
    let _ = a::sqlx::query!("SELECT 6");
    let _ = sqlx::query_with!("SELECT 7");
}
"###;
        assert_eq!(texts(src), ["\"SELECT 1\"", "\"SELECT 2\"", "\"SELECT 6\""]);
    }

    #[test]
    fn bare_macros_imported_from_sqlx_count() {
        let src = r###"
use sqlx::{query, query_scalar as scalar};
mod m {
    use sqlx::*;
    fn g() { let _ = query_as!(Row, "SELECT 1"); }
}
fn f() {
    let _ = query!("SELECT 2");
    let _ = scalar!("SELECT 3");
    let _ = query_as!(Row, "SELECT 4");
}
"###;
        assert_eq!(texts(src), ["\"SELECT 1\"", "\"SELECT 2\"", "\"SELECT 3\""]);
    }

    #[test]
    fn a_macro_use_extern_crate_makes_bare_macros_count() {
        let src = "#[macro_use]\nextern crate sqlx;\nmod m { fn f() { let _ = query!(\"SELECT 1\"); } }\n";
        assert_eq!(texts(src), ["\"SELECT 1\""]);
        let without = "extern crate sqlx;\nfn f() { let _ = query!(\"SELECT 1\"); }\n";
        assert!(texts(without).is_empty());
    }

    #[test]
    fn a_crate_alias_counts_as_sqlx() {
        let src = r###"
use sqlx as db;
extern crate sqlx as pg;
fn f() {
    let _ = db::query("SELECT 1");
    let _ = db::query!("SELECT 2");
    let _ = pg::query_scalar("SELECT 3");
}
mod m {
    fn g() { let _ = db::query("SELECT 4"); }
}
fn h() { let _ = dbx::query("SELECT 5"); }
"###;
        assert_eq!(texts(src), ["\"SELECT 1\"", "\"SELECT 2\"", "\"SELECT 3\""]);
    }

    #[test]
    fn finds_queries_inside_other_macros_with_exact_spans() {
        let src = r###"
fn f() {
    let (_x, _y) = tokio::try_join!(sqlx::query!("SELECT 1").fetch_one(&p), other());
    assert!(sqlx::query("SELECT 2").sql().len() > 0, "msg {}", sqlx::query_scalar("SELECT 3").sql());
    let _ = vec![sqlx::query(r#"SELECT 4"#)];
    json!({ "a": sqlx::query("SELECT 5") });
}
"###;
        let found = extract(src);
        let texts: Vec<&str> = found.iter().map(|q| q.text.as_str()).collect();
        assert_eq!(texts, ["\"SELECT 1\"", "\"SELECT 2\"", "\"SELECT 3\"", "r#\"SELECT 4\"#", "\"SELECT 5\""]);
        for q in &found {
            assert_eq!(&src[q.start_byte..q.end_byte], q.text);
            assert_eq!(src[..q.start_byte].matches('\n').count() + 1, q.line);
        }
    }

    #[test]
    fn extracts_only_sqlx_queries_from_a_mixed_file() {
        let src = r###"use sqlx as db;
mod a { use sqlx::query; }
mod b {
    fn query(s: &str) -> &str { s }
    fn g() { let _ = query("keep   THIS = NULL text"); }
}
fn f() {
    let (_x, _y) = tokio::try_join!(sqlx::query!("SELECT 1 FROM t WHERE x = NULL").fetch_one(&p), other());
    assert!(sqlx::query("SELECT 2 FROM t WHERE x = NULL").sql().len() > 0);
    let _ = db::query("SELECT 3 FROM t WHERE x = NULL");
    let _ = graphql::query!("query  { user = NULL }");
}
"###;
        let found = extract(src);
        let lines: Vec<usize> = found.iter().map(|q| q.line).collect();
        assert_eq!(lines, [8, 9, 10]);
        for q in &found {
            assert!(q.text.starts_with("\"SELECT "), "{}", q.text);
            assert_eq!(&src[q.start_byte..q.end_byte], q.text);
        }
    }

    #[test]
    fn a_literal_on_the_first_line_after_a_byte_order_mark_has_its_exact_span() {
        let src = "\u{feff}fn f(){let _=sqlx::query!(\"SELECT 1\");}\nfn g(){let _=sqlx::query!(\"SELECT 2\");}\n";
        let found = extract(src);
        assert_eq!(found.len(), 2);
        for (q, text) in found.iter().zip(["\"SELECT 1\"", "\"SELECT 2\""]) {
            assert_eq!(q.text, text);
            assert_eq!(&src[q.start_byte..q.end_byte], text);
        }
    }

    #[test]
    fn glob_import_from_sqlx_counts() {
        let src = "use sqlx::*;\nfn f(){let _=query_as::<_, Row>(\"SELECT 1\");}";
        assert_eq!(texts(src), ["\"SELECT 1\""]);
    }

    #[test]
    fn finds_macros_and_function_calls_in_the_same_file() {
        let src = r###"
fn f() {
    let _ = sqlx::query!("SELECT 1");
    let _ = sqlx::query("SELECT 2");
}
"###;
        assert_eq!(texts(src), ["\"SELECT 1\"", "\"SELECT 2\""]);
    }

    #[test]
    fn ignores_unparsable_file() {
        assert!(extract("this is not rust {{{").is_empty());
    }

    #[test]
    fn pre_filter_skips_query_free_files_without_parsing() {
        // No "query" substring -> Ok-empty without invoking syn, even if the file
        // would not parse. Such files are never reported as skipped/unparsable.
        assert!(extract_checked("this is not rust {{{").unwrap().is_empty());
        assert!(extract_checked("fn f() { let x = 1; }").unwrap().is_empty());
    }

    #[test]
    fn pre_filter_still_errors_on_unparsable_file_with_query() {
        // Contains "query" -> syn must still run, so a genuine parse failure is
        // surfaced (the CLI reports it as an error naming the file).
        assert!(extract_checked("query! this is not rust {{{").is_err());
    }

    fn error_position(src: &str) -> (usize, usize) {
        let err = extract_checked(src).unwrap_err();
        (err.line, err.col)
    }

    #[test]
    fn a_parse_error_gives_the_line_and_column_that_syn_points_at() {
        assert_eq!(error_position("fn f() {\n    let x = ;\n    sqlx::query(\"SELECT 1\");\n}\n"), (2, 13));
        // Columns count chars, not bytes.
        assert_eq!(error_position("fn f() {\n    let é = ;\n    sqlx::query(\"SELECT 1\");\n}\n"), (2, 13));
        // A delimiter that is never closed.
        assert_eq!(error_position("fn f() {\n    sqlx::query(\"SELECT 1\");\n"), (1, 8));
        assert_eq!(error_position(")\nfn f() {\n    sqlx::query(\"SELECT 1\");\n}\n"), (1, 1));
        // The end of the file.
        assert_eq!(error_position("fn f() {\n    sqlx::query(\"SELECT 1\")\n}\nfn g() -> query\n\n"), (4, 16));
    }
}
