//! Locate `sqlx::query*!` macros and `sqlx::query*(...)` function calls, and the
//! byte span of their SQL string-literal argument, using `syn` + `proc-macro2`
//! span locations.

use std::collections::HashSet;

use proc_macro2::{Literal, TokenTree};
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

/// Extract every inline query literal. Returns empty (not an error) if the file
/// does not parse; use [`extract_checked`] to distinguish "no queries" from
/// "file didn't parse" so callers can warn instead of silently skipping.
pub fn extract(src: &str) -> Vec<QueryLiteral> {
    extract_checked(src).unwrap_or_default()
}

/// Like [`extract`], but returns the `syn` parse error so callers can warn that
/// a whole file was skipped (rather than hiding its queries).
pub fn extract_checked(src: &str) -> Result<Vec<QueryLiteral>, String> {
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
    let file = syn::parse_file(src).map_err(|e| e.to_string())?;
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
        syn::visit::visit_block(self, block);
        self.scopes.pop();
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if self.is_query_path(&m.path, Namespace::Macro) {
            if let Some(lit) = sql_literal(m.tokens.clone()) {
                self.found.push(lit);
            }
        } else if let Ok(args) =
            Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated.parse2(m.tokens.clone())
        {
            // Most macros (`assert!`, `vec!`, `tokio::try_join!`, ...) take
            // expressions, which can hold queries; skip any that do not parse.
            for arg in &args {
                Visit::visit_expr(self, arg);
            }
        }
        syn::visit::visit_macro(self, m);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(func) = &*call.func
            && func.qself.is_none()
            && self.is_query_path(&func.path, Namespace::Function)
            && let Some(syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(sql), .. })) =
                call.args.first()
        {
            self.found.push(sql.token());
        }
        syn::visit::visit_expr_call(self, call);
    }
}

impl Visitor {
    /// `sqlx::NAME` (or the crate under an alias), or a bare `NAME` that resolves
    /// to an import from sqlx. A bare `query` without that import is rejected
    /// because other crates export functions and macros with the same names.
    fn is_query_path(&self, path: &syn::Path, namespace: Namespace) -> bool {
        let segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        match segments.as_slice() {
            [krate, name] => {
                namespace.query_names().contains(&name.as_str())
                    && self.names_sqlx_crate(krate, path.leading_colon.is_some())
            }
            [name] if path.leading_colon.is_none() => self.resolves_to_sqlx(name, namespace),
            _ => false,
        }
    }

    fn names_sqlx_crate(&self, name: &str, leading_colon: bool) -> bool {
        if name == "sqlx" || self.crate_aliases.contains(name) {
            return true;
        }
        !leading_colon && self.enclosing_scopes().any(|scope| scope.crates.contains(name))
    }

    fn resolves_to_sqlx(&self, name: &str, namespace: Namespace) -> bool {
        match self.enclosing_scopes().find_map(|scope| scope.lookup(name, namespace)) {
            Some(found) => found,
            None => {
                matches!(namespace, Namespace::Macro)
                    && self.macro_use
                    && QUERY_MACROS.contains(&name)
            }
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
    fn query_names(self) -> &'static [&'static str] {
        match self {
            Self::Function => QUERY_FUNCTIONS,
            Self::Macro => QUERY_MACROS,
        }
    }
}

/// The names that the items of one module or block bring into scope.
#[derive(Default)]
struct Scope {
    /// Whether this is a module, which does not see the names around it.
    module: bool,
    /// Local names of sqlx query functions imported by a `use` item.
    functions: HashSet<String>,
    /// Local names of sqlx query macros imported by a `use` item.
    macros: HashSet<String>,
    /// Whether a `use sqlx::*;` is in this scope.
    glob: bool,
    /// Local names of the sqlx crate (`use sqlx as db;`).
    crates: HashSet<String>,
    /// Other function names defined or imported here; they shadow a glob import
    /// and the names of the enclosing scopes.
    other_functions: HashSet<String>,
    /// Other macro names defined or imported here, as for `other_functions`.
    other_macros: HashSet<String>,
}

impl Scope {
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

    /// `Some(true)` when `name` is a sqlx query function or macro here,
    /// `Some(false)` when this scope gives `name` another meaning, `None` when
    /// it says nothing about `name`.
    fn lookup(&self, name: &str, namespace: Namespace) -> Option<bool> {
        let (imported, other) = match namespace {
            Namespace::Function => (&self.functions, &self.other_functions),
            Namespace::Macro => (&self.macros, &self.other_macros),
        };
        if imported.contains(name) {
            Some(true)
        } else if other.contains(name) {
            Some(false)
        } else if self.glob && namespace.query_names().contains(&name) {
            Some(true)
        } else {
            None
        }
    }

    /// Walk the start of a use tree, before the `sqlx` segment has been seen.
    fn add_use(&mut self, tree: &syn::UseTree) {
        match tree {
            syn::UseTree::Path(p) if p.ident == "sqlx" => self.add_sqlx_use(&p.tree),
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
        if QUERY_FUNCTIONS.contains(&name.as_str()) {
            self.functions.insert(local.to_string());
        }
        if QUERY_MACROS.contains(&name.as_str()) {
            self.macros.insert(local.to_string());
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

/// The first string literal in the token stream, which skips a leading struct
/// ident + comma (`query_as!(Row, r#"..."#)`) and stops before bind-arg literals.
/// `None` when that literal is not a whole argument on its own: sqlx joins
/// literals written with `+`, and rewriting one of them alone can break the query.
fn sql_literal(tokens: proc_macro2::TokenStream) -> Option<Literal> {
    let is_comma = |tt: &TokenTree| matches!(tt, TokenTree::Punct(p) if p.as_char() == ',');
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    let (at, lit) = tokens.iter().enumerate().find_map(|(i, tt)| match tt {
        TokenTree::Literal(l) => {
            let s = l.to_string();
            (s.starts_with('"') || s.starts_with("r\"") || s.starts_with("r#")).then_some((i, l))
        }
        _ => None,
    })?;
    let whole = (at == 0 || is_comma(&tokens[at - 1])) && tokens.get(at + 1).is_none_or(is_comma);
    whole.then(|| lit.clone())
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
        assert_eq!(texts(src), ["\"SELECT 1\"", "\"SELECT 2\""]);
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
        assert_eq!(texts, ["\"SELECT 1\"", "\"SELECT 2\"", "\"SELECT 3\"", "r#\"SELECT 4\"#"]);
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
        // surfaced (the CLI turns this into a skip warning).
        assert!(extract_checked("query! this is not rust {{{").is_err());
    }
}
