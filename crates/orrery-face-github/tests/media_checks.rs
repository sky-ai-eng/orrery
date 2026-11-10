//! Every REST route refuses the GitHub media types it doesn't render
//! (spec/grammar.md, "Requests"). A layer on the router can't do it in
//! GitHub's order: the check has to run after the handler finds what its path
//! names, so a missing subject is GitHub's 404, and before it reads its body
//! or writes. So each handler makes the check itself, and this test reads the
//! face's source to hold every handler the route table names to three rules:
//!
//! - it calls `media::refuse_unrendered`, `media::refuse_unrendered_except`,
//!   or `media::refuse_unrendered_unless_diff` exactly once, as `if let Some(refused) = media::… { return refused; }`,
//!   a statement of its own in the handler's body, so no branch skips it;
//! - the call names its resource with a string literal that spec/grammar.md
//!   lists; and
//! - no statement before the call reads the handler's request body.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use syn::visit::{self, Visit};
use syn::{Expr, ExprCall, ExprMethodCall, ExprPath, FnArg, Item, ItemFn, Lit, Pat, Stmt, Type};

/// The axum method-router constructors and builders that take a handler.
const METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];
const CHECKS: &[&str] = &[
    "refuse_unrendered",
    "refuse_unrendered_except",
    "refuse_unrendered_unless_diff",
];

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn parse(path: &Path) -> syn::File {
    let source =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    syn::parse_file(&source).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn path_string(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

fn is_check(call: &ExprCall) -> bool {
    matches!(&*call.func, Expr::Path(func)
        if func.path.segments.last().is_some_and(|s| CHECKS.contains(&s.ident.to_string().as_str())))
}

/// The handlers `router` names, as the route table spells them
/// (`reads::get_pull`), with any argument that isn't a named function.
#[derive(Default)]
struct Routed {
    handlers: Vec<syn::Path>,
    unnamed: Vec<String>,
}

impl Routed {
    fn take(&mut self, method: &str, handler: Option<&Expr>) {
        match handler {
            Some(Expr::Path(handler)) => self.handlers.push(handler.path.clone()),
            _ => self.unnamed.push(format!(
                "a `{method}` route's handler is not a named function"
            )),
        }
    }
}

impl<'ast> Visit<'ast> for Routed {
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if let Expr::Path(func) = &*call.func
            && let [segment] = func.path.segments.iter().collect::<Vec<_>>()[..]
            && METHODS.contains(&segment.ident.to_string().as_str())
        {
            self.take(&segment.ident.to_string(), call.args.first());
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        let method = call.method.to_string();
        if METHODS.contains(&method.as_str()) {
            self.take(&method, call.args.first());
        }
        visit::visit_expr_method_call(self, call);
    }
}

#[derive(Default)]
struct Checks(usize);

impl<'ast> Visit<'ast> for Checks {
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if is_check(call) {
            self.0 += 1;
        }
        visit::visit_expr_call(self, call);
    }
}

/// Whether an expression reads the variable `name`.
struct Reads<'a> {
    name: &'a str,
    found: bool,
}

impl<'ast> Visit<'ast> for Reads<'_> {
    fn visit_expr_path(&mut self, path: &'ast ExprPath) {
        if path.path.is_ident(self.name) {
            self.found = true;
        }
        visit::visit_expr_path(self, path);
    }
}

/// The handler's request-body argument (`body: Bytes`), if it takes one.
fn body_arg(handler: &ItemFn) -> Option<String> {
    handler.sig.inputs.iter().find_map(|input| match input {
        FnArg::Typed(arg) => match (&*arg.pat, &*arg.ty) {
            (Pat::Ident(name), Type::Path(ty))
                if ty.path.segments.last().is_some_and(|s| s.ident == "Bytes") =>
            {
                Some(name.ident.to_string())
            }
            _ => None,
        },
        FnArg::Receiver(_) => None,
    })
}

/// The statement index and resource of the handler's check, when it is the
/// `if let Some(refused) = media::…(&headers, "Resource", …) { … }` form.
fn check_statement(handler: &ItemFn) -> Option<(usize, Option<String>)> {
    handler
        .block
        .stmts
        .iter()
        .enumerate()
        .find_map(|(index, stmt)| {
            let Stmt::Expr(Expr::If(branch), _) = stmt else {
                return None;
            };
            let Expr::Let(binding) = &*branch.cond else {
                return None;
            };
            let Expr::Call(call) = &*binding.expr else {
                return None;
            };
            if !is_check(call) {
                return None;
            }
            let resource = match call.args.iter().nth(1) {
                Some(Expr::Lit(lit)) => match &lit.lit {
                    Lit::Str(s) => Some(s.value()),
                    _ => None,
                },
                _ => None,
            };
            Some((index, resource))
        })
}

/// The `resource` values spec/grammar.md lists.
fn spec_resources() -> Vec<String> {
    let grammar = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/grammar.md");
    let text =
        std::fs::read_to_string(&grammar).unwrap_or_else(|e| panic!("{}: {e}", grammar.display()));
    let line = text
        .lines()
        .find(|l| l.contains("`resource` names what the route is about:"))
        .expect("spec/grammar.md lists no `resource` values");
    let (_, list) = line.split_once("about:").expect("checked above");
    list.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

#[test]
fn every_routed_handler_refuses_unrendered_media() {
    let src = src_dir();
    let lib = parse(&src.join("lib.rs"));
    let router = lib
        .items
        .iter()
        .find_map(|item| match item {
            Item::Fn(f) if f.sig.ident == "router" => Some(f),
            _ => None,
        })
        .expect("lib.rs has no `fn router`");
    let mut routed = Routed::default();
    routed.visit_item_fn(router);
    assert!(!routed.handlers.is_empty(), "found no routes in `router`");

    let resources = spec_resources();
    let mut files = BTreeMap::new();
    let mut faults = routed.unnamed;
    for handler in &routed.handlers {
        let name = path_string(handler);
        let mut segments: Vec<String> = handler
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect();
        let ident = segments.pop().expect("a path has a segment");
        if segments.first().is_some_and(|s| s == "crate") {
            segments.remove(0);
        }
        let file = if segments.is_empty() {
            src.join("lib.rs")
        } else {
            let module = segments.join("/");
            let flat = src.join(format!("{module}.rs"));
            if flat.exists() {
                flat
            } else {
                src.join(module).join("mod.rs")
            }
        };
        let parsed = files.entry(file.clone()).or_insert_with(|| parse(&file));
        let Some(handler_fn) = parsed.items.iter().find_map(|item| match item {
            Item::Fn(f) if f.sig.ident == ident => Some(f),
            _ => None,
        }) else {
            faults.push(format!("`{name}`: no such function in {}", file.display()));
            continue;
        };

        let mut checks = Checks::default();
        checks.visit_item_fn(handler_fn);
        if checks.0 != 1 {
            faults.push(format!(
                "`{name}` calls the media check {} times; it calls it once",
                checks.0
            ));
            continue;
        }
        let Some((index, resource)) = check_statement(handler_fn) else {
            faults.push(format!(
                "`{name}`: the media check is not a statement of its own in the handler's body \
                 (`if let Some(refused) = media::refuse_unrendered(&headers, \"…\") {{ return refused; }}`)"
            ));
            continue;
        };
        match resource {
            Some(r) if resources.contains(&r) => {}
            Some(r) => faults.push(format!(
                "`{name}` names the resource `{r}`, which spec/grammar.md does not list"
            )),
            None => faults.push(format!(
                "`{name}`: the media check's resource is not a string literal"
            )),
        }
        if let Some(body) = body_arg(handler_fn) {
            let early = handler_fn.block.stmts[..index].iter().any(|stmt| {
                let mut reads = Reads {
                    name: &body,
                    found: false,
                };
                reads.visit_stmt(stmt);
                reads.found
            });
            if early {
                faults.push(format!(
                    "`{name}` reads `{body}` before the media check; the check comes first"
                ));
            }
        }
    }
    assert!(faults.is_empty(), "{}", faults.join("\n"));
}
