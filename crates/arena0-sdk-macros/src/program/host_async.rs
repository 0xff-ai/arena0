//! Static rejection of host async and I/O in deterministic module-shell handlers.
//!
//! Owns the validation pass that walks module-shell handler bodies and rejects
//! task spawning, host sleeps, network I/O, and `async` blocks (including
//! aliased and glob-imported paths), so nothing nondeterministic escapes into a
//! traced program. This is the host-async validation seam.

use std::collections::HashMap;
use syn::visit::Visit;
use syn::{Error, Expr, Item, Result, spanned::Spanned};

pub(super) fn reject_disallowed_host_async_in_module(items: &[Item]) -> Result<()> {
    let host_async_aliases = collect_use_aliases(items);
    for item in items {
        let Item::Fn(function) = item else {
            continue;
        };
        let mut visitor = DisallowedHostAsyncVisitor {
            error: None,
            host_async_aliases: host_async_aliases.clone(),
        };
        visitor.visit_item_fn(function);
        if let Some(error) = visitor.error {
            return Err(error);
        }
    }
    Ok(())
}

fn collect_use_aliases(items: &[Item]) -> HashMap<String, String> {
    let mut aliases = HashMap::new();
    for item in items {
        if let Item::Use(item_use) = item {
            collect_use_aliases_from_tree(&item_use.tree, "", &mut aliases);
        }
    }
    aliases
}

fn collect_use_aliases_from_tree(
    tree: &syn::UseTree,
    prefix: &str,
    aliases: &mut HashMap<String, String>,
) {
    match tree {
        syn::UseTree::Path(path) => {
            let next = join_path(prefix, &path.ident.to_string());
            collect_use_aliases_from_tree(&path.tree, &next, aliases);
        }
        syn::UseTree::Name(name) => {
            aliases.insert(
                name.ident.to_string(),
                join_path(prefix, &name.ident.to_string()),
            );
        }
        syn::UseTree::Rename(rename) => {
            aliases.insert(
                rename.rename.to_string(),
                join_path(prefix, &rename.ident.to_string()),
            );
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_use_aliases_from_tree(item, prefix, aliases);
            }
        }
        syn::UseTree::Glob(_) => {
            for name in glob_alias_names(prefix).iter().copied() {
                aliases.insert(name.to_owned(), join_path(prefix, name));
            }
        }
    }
}

fn join_path(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_owned()
    } else {
        format!("{prefix}::{segment}")
    }
}

fn glob_alias_names(prefix: &str) -> &'static [&'static str] {
    match prefix {
        "async_std" => &["net", "task"],
        "async_std::net" => &["TcpListener", "TcpStream", "UdpSocket"],
        "async_std::task" => &["sleep", "spawn"],
        "reqwest" => &["get"],
        "std" => &["net", "thread"],
        "std::net" => &["TcpListener", "TcpStream", "UdpSocket"],
        "std::thread" => &["sleep", "spawn"],
        "tokio" => &["net", "spawn", "task", "time"],
        "tokio::net" => &["TcpListener", "TcpStream", "UdpSocket"],
        "tokio::task" => &["spawn", "spawn_blocking"],
        "tokio::time" => &["sleep"],
        _ => &[],
    }
}

struct DisallowedHostAsyncVisitor {
    error: Option<Error>,
    host_async_aliases: HashMap<String, String>,
}

impl DisallowedHostAsyncVisitor {
    fn reject(&mut self, span: proc_macro2::Span, message: &'static str) {
        if self.error.is_none() {
            self.error = Some(Error::new(span, message));
        }
    }

    fn path_call_kind(&self, path: &syn::Path) -> Option<&'static str> {
        let segments = path_segments(path);
        if let Some(message) = Self::path_segments_kind(&segments) {
            return Some(message);
        }
        if path.leading_colon.is_none()
            && let Some(first) = path.segments.first()
            && let Some(alias) = self.host_async_aliases.get(&first.ident.to_string())
        {
            let mut resolved = alias.split("::").map(str::to_owned).collect::<Vec<_>>();
            resolved.extend(
                path.segments
                    .iter()
                    .skip(1)
                    .map(|segment| segment.ident.to_string()),
            );
            return Self::path_segments_kind(&resolved);
        }
        None
    }

    fn path_segments_kind(segments: &[String]) -> Option<&'static str> {
        if path_is(segments, &["tokio", "spawn"])
            || path_is(segments, &["tokio", "task", "spawn"])
            || path_is(segments, &["tokio", "task", "spawn_blocking"])
            || path_is(segments, &["async_std", "task", "spawn"])
            || path_is(segments, &["std", "thread", "spawn"])
        {
            return Some(
                "task spawning is not supported in deterministic module-shell handlers; model concurrency as arena events",
            );
        }
        if path_is(segments, &["tokio", "time", "sleep"])
            || path_is(segments, &["async_std", "task", "sleep"])
            || path_is(segments, &["std", "thread", "sleep"])
        {
            return Some(
                "host sleeps are not supported in deterministic module-shell handlers; use a typed timer effect and on_timer handler",
            );
        }
        if path_is(segments, &["std", "net", "TcpStream", "connect"])
            || path_is(segments, &["std", "net", "TcpListener", "bind"])
            || path_is(segments, &["std", "net", "UdpSocket", "bind"])
            || path_is(segments, &["std", "net", "UdpSocket", "connect"])
            || path_is(segments, &["tokio", "net", "TcpStream", "connect"])
            || path_is(segments, &["tokio", "net", "TcpListener", "bind"])
            || path_is(segments, &["tokio", "net", "UdpSocket", "bind"])
            || path_is(segments, &["tokio", "net", "UdpSocket", "connect"])
            || path_is(segments, &["async_std", "net", "TcpStream", "connect"])
            || path_is(segments, &["async_std", "net", "TcpListener", "bind"])
            || path_is(segments, &["async_std", "net", "UdpSocket", "bind"])
            || path_is(segments, &["async_std", "net", "UdpSocket", "connect"])
            || path_is(segments, &["reqwest", "get"])
        {
            return Some(
                "network I/O is not supported in deterministic module-shell handlers; use arena peer messages or host effects",
            );
        }
        None
    }
}

fn path_segments(path: &syn::Path) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect()
}

fn path_is(segments: &[String], expected: &[&str]) -> bool {
    segments.len() == expected.len()
        && segments
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.as_str() == *expected)
}

impl<'ast> Visit<'ast> for DisallowedHostAsyncVisitor {
    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        collect_use_aliases_from_tree(&node.tree, "", &mut self.host_async_aliases);
        syn::visit::visit_item_use(self, node);
    }

    fn visit_expr_async(&mut self, node: &'ast syn::ExprAsync) {
        self.reject(
            node.async_token.span(),
            "async blocks are not supported in deterministic module-shell handlers; await arena effects directly",
        );
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if self.error.is_some() {
            return;
        }
        if let Expr::Path(path) = node.func.as_ref()
            && let Some(message) = self.path_call_kind(&path.path)
        {
            self.reject(path.path.span(), message);
            return;
        }
        syn::visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if self.error.is_some() {
            return;
        }
        if node.method == "spawn" {
            self.reject(
                node.method.span(),
                "task spawning is not supported in deterministic module-shell handlers; model concurrency as arena events",
            );
            return;
        }
        if node.method == "recv" {
            self.reject(
                node.method.span(),
                "awaitable receive is not supported in module-shell handlers; handle peer input in on_message",
            );
            return;
        }
        if node.method == "timer" {
            self.reject(
                node.method.span(),
                "awaitable timers are not supported in module-shell handlers; use typed timer effects and on_timer",
            );
            return;
        }
        syn::visit::visit_expr_method_call(self, node);
    }
}
