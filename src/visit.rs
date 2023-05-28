// Copyright 2021-2023 Martin Pool

//! Visit the abstract syntax tree and discover things to mutate.
//!
//! Knowledge of the `syn` API is localized here.
//!
//! Walking the tree starts with some root files known to the build tool:
//! e.g. for cargo they are identified from the targets. The tree walker then
//! follows `mod` statements to recursively visit other referenced files.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::Context;
use itertools::Itertools;
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::{quote, ToTokens};
use syn::ext::IdentExt;
use syn::visit::Visit;
use syn::{Attribute, Expr, ItemFn, ReturnType};
use tracing::{debug, debug_span, trace, trace_span, warn};

use crate::path::TreeRelativePathBuf;
use crate::source::SourceFile;
use crate::*;

/// Mutants and files discovered in a source tree.
///
/// Files are listed separately so that we can represent files that
/// were visited but that produced no mutants.
pub struct Discovered {
    pub mutants: Vec<Mutant>,
    pub files: Vec<Arc<SourceFile>>,
}

/// Discover all mutants and all source files.
///
/// The list of source files includes even those with no mutants.
pub fn walk_tree(tool: &dyn Tool, root: &Utf8Path, options: &Options) -> Result<Discovered> {
    let mut mutants = Vec::new();
    let mut files: Vec<Arc<SourceFile>> = Vec::new();

    let mut file_queue: VecDeque<Arc<SourceFile>> = tool.root_files(root)?.into();
    while let Some(source_file) = file_queue.pop_front() {
        check_interrupted()?;
        let (mut file_mutants, more_files) = walk_file(root, Arc::clone(&source_file), options)?;
        // We'll still walk down through files that don't match globs, so that
        // we have a chance to find modules underneath them. However, we won't
        // collect any mutants from them, and they don't count as "seen" for
        // `--list-files`.
        for path in more_files {
            file_queue.push_back(Arc::new(SourceFile::new(root, path, &source_file.package)?));
        }
        let path = &source_file.tree_relative_path;
        if let Some(examine_globset) = &options.examine_globset {
            if !examine_globset.is_match(path.as_ref()) {
                trace!("{path:?} does not match examine globset");
                continue;
            }
        }
        if let Some(exclude_globset) = &options.exclude_globset {
            if exclude_globset.is_match(path.as_ref()) {
                trace!("{path:?} excluded by globset");
                continue;
            }
        }
        if let Some(examine_names) = &options.examine_names {
            if !examine_names.is_empty() {
                file_mutants.retain(|m| examine_names.is_match(&m.to_string()));
            }
        }
        if let Some(exclude_names) = &options.exclude_names {
            if !exclude_names.is_empty() {
                file_mutants.retain(|m| !exclude_names.is_match(&m.to_string()));
            }
        }
        mutants.append(&mut file_mutants);
        files.push(Arc::clone(&source_file));
    }
    Ok(Discovered { mutants, files })
}

/// Find all possible mutants in a source file.
///
/// Returns the mutants found, and more files discovered by `mod` statements to visit.
fn walk_file(
    root: &Utf8Path,
    source_file: Arc<SourceFile>,
    options: &Options,
) -> Result<(Vec<Mutant>, Vec<TreeRelativePathBuf>)> {
    let _span = debug_span!("source_file", path = source_file.tree_relative_slashes()).entered();
    debug!("visit source file");
    let syn_file = syn::parse_str::<syn::File>(&source_file.code)
        .with_context(|| format!("failed to parse {}", source_file.tree_relative_slashes()))?;
    let error_exprs = options
        .error_values
        .iter()
        .map(|e| syn::parse_str(e).with_context(|| "Failed to parse error value {e:?}"))
        .collect::<Result<Vec<Expr>>>()?;
    let mut visitor = DiscoveryVisitor {
        error_exprs,
        more_files: Vec::new(),
        mutants: Vec::new(),
        namespace_stack: Vec::new(),
        options,
        root: root.to_owned(),
        source_file,
    };
    visitor.visit_file(&syn_file);
    Ok((visitor.mutants, visitor.more_files))
}

/// `syn` visitor that recursively traverses the syntax tree, accumulating places
/// that could be mutated.
struct DiscoveryVisitor<'o> {
    /// All the mutants generated by visiting the file.
    mutants: Vec<Mutant>,

    /// The file being visited.
    source_file: Arc<SourceFile>,

    /// The root of the source tree.
    root: Utf8PathBuf,

    /// The stack of namespaces we're currently inside.
    namespace_stack: Vec<String>,

    /// Files discovered by `mod` statements.
    more_files: Vec<TreeRelativePathBuf>,

    /// Global options.
    #[allow(unused)]
    options: &'o Options,

    /// Parsed error expressions, from the config file or command line.
    error_exprs: Vec<Expr>,
}

impl<'o> DiscoveryVisitor<'o> {
    fn collect_fn_mutants(&mut self, return_type: &ReturnType, span: &proc_macro2::Span) {
        let full_function_name = Arc::new(self.namespace_stack.join("::"));
        let return_type_str = Arc::new(return_type_to_string(return_type));
        let mut new_mutants = self
            .return_value_replacements(return_type)
            .into_iter()
            .map(|rep| Mutant {
                source_file: Arc::clone(&self.source_file),
                function_name: Arc::clone(&full_function_name),
                return_type: Arc::clone(&return_type_str),
                replacement: tokens_to_pretty_string(&rep),
                span: span.into(),
                genre: Genre::FnValue,
            })
            .collect_vec();
        if new_mutants.is_empty() {
            debug!(
                ?full_function_name,
                ?return_type_str,
                "No mutants generated for this return type"
            );
        } else {
            self.mutants.append(&mut new_mutants);
        }
    }

    /// Call a function with a namespace pushed onto the stack.
    ///
    /// This is used when recursively descending into a namespace.
    fn in_namespace<F, T>(&mut self, name: &str, f: F) -> T
    where
        F: FnOnce(&mut Self) -> T,
    {
        self.namespace_stack.push(name.to_owned());
        let r = f(self);
        assert_eq!(self.namespace_stack.pop().unwrap(), name);
        r
    }

    /// Generate replacement text for a function based on its return type.
    fn return_value_replacements(&self, return_type: &ReturnType) -> Vec<TokenStream> {
        let mut reps = Vec::new();
        match return_type {
            ReturnType::Default => reps.push(quote! { () }),
            ReturnType::Type(_rarrow, box_typ) => match &**box_typ {
                syn::Type::Never(_) => {
                    // In theory we could mutate this to a function that just
                    // loops or sleeps, but it seems unlikely to be useful,
                    // so generate nothing.
                }
                syn::Type::Path(syn::TypePath { path, .. }) => {
                    // dbg!(&path);
                    if path.is_ident("bool") {
                        reps.push(quote! { true });
                        reps.push(quote! { false });
                    } else if path.is_ident("String") {
                        reps.push(quote! { String::new() });
                        reps.push(quote! { "xyzzy".into() });
                    } else if path_is_result(path) {
                        // TODO: Recursively generate for types inside the Ok side of the Result.
                        reps.push(quote! { Ok(Default::default()) });
                        reps.extend(self.error_exprs.iter().map(|error_expr| {
                            quote! { Err(#error_expr) }
                        }));
                    } else {
                        reps.push(quote! { Default::default() });
                    }
                }
                syn::Type::Reference(syn::TypeReference {
                    mutability: None,
                    elem,
                    ..
                }) => match &**elem {
                    // needs a separate `match` because of the box.
                    syn::Type::Path(path) if path.path.is_ident("str") => {
                        reps.push(quote! { "" });
                        reps.push(quote! { "xyzzy" });
                    }
                    _ => {
                        trace!(?box_typ, "Return type is not recognized, trying Default");
                        reps.push(quote! { Default::default() });
                    }
                },
                syn::Type::Reference(syn::TypeReference {
                    mutability: Some(_),
                    ..
                }) => {
                    reps.push(quote! { Box::leak(Box::new(Default::default())) });
                }
                _ => {
                    trace!(?box_typ, "Return type is not recognized, trying Default");
                    reps.push(quote! { Default::default() });
                }
            },
        }
        reps
    }
}

impl<'ast> Visit<'ast> for DiscoveryVisitor<'_> {
    /// Visit top-level `fn foo()`.
    fn visit_item_fn(&mut self, i: &'ast ItemFn) {
        let function_name = tokens_to_pretty_string(&i.sig.ident);
        let _span = trace_span!(
            "fn",
            line = i.sig.fn_token.span.start().line,
            name = function_name
        )
        .entered();
        if fn_sig_excluded(&i.sig) || attrs_excluded(&i.attrs) || block_is_empty(&i.block) {
            return;
        }
        self.in_namespace(&function_name, |self_| {
            self_.collect_fn_mutants(&i.sig.output, &i.block.brace_token.span.join());
            syn::visit::visit_item_fn(self_, i);
        });
    }

    /// Visit `fn foo()` within an `impl`.
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        // Don't look inside constructors (called "new") because there's often no good
        // alternative.
        let function_name = tokens_to_pretty_string(&i.sig.ident);
        let _span = trace_span!(
            "fn",
            line = i.sig.fn_token.span.start().line,
            name = function_name
        )
        .entered();
        if fn_sig_excluded(&i.sig)
            || attrs_excluded(&i.attrs)
            || i.sig.ident == "new"
            || block_is_empty(&i.block)
        {
            return;
        }
        self.in_namespace(&function_name, |self_| {
            self_.collect_fn_mutants(&i.sig.output, &i.block.brace_token.span.join());
            syn::visit::visit_impl_item_fn(self_, i)
        });
    }

    /// Visit `impl Foo { ...}` or `impl Debug for Foo { ... }`.
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if attrs_excluded(&i.attrs) {
            return;
        }
        let type_name = tokens_to_pretty_string(&i.self_ty);
        let name = if let Some((_, trait_path, _)) = &i.trait_ {
            let trait_name = &trait_path.segments.last().unwrap().ident;
            if trait_name == "Default" {
                // Can't think of how to generate a viable different default.
                return;
            }
            format!("<impl {trait_name} for {type_name}>")
        } else {
            type_name
        };
        self.in_namespace(&name, |v| syn::visit::visit_item_impl(v, i));
    }

    /// Visit `mod foo { ... }` or `mod foo;`.
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        let mod_name = &node.ident.unraw().to_string();
        let _span = trace_span!(
            "mod",
            line = node.mod_token.span.start().line,
            name = mod_name
        )
        .entered();
        if attrs_excluded(&node.attrs) {
            trace!("mod {:?} excluded by attrs", node.ident,);
            return;
        }
        // If there's no content in braces, then this is a `mod foo;`
        // statement referring to an external file. We find the file name
        // then remember to visit it later.
        //
        // Both the current module and the included sub-module can be in
        // either style: `.../foo.rs` or `.../foo/mod.rs`.
        //
        // If the current file ends with `/mod.rs`, then sub-modules
        // will be in the same directory as this file. Otherwise, this is
        // `/foo.rs` and sub-modules will be in `foo/`.
        //
        // Having determined the directory then we can look for either
        // `foo.rs` or `foo/mod.rs`.
        if node.content.is_none() {
            let my_path: &Utf8Path = self.source_file.tree_relative_path().as_ref();
            // Maybe matching on the name here is no the right approach and
            // we should instead remember how this file was found?
            let dir = if my_path.ends_with("mod.rs")
                || my_path.ends_with("lib.rs")
                || my_path.ends_with("main.rs")
            {
                my_path.parent().expect("mod path has no parent").to_owned()
            } else {
                my_path.with_extension("")
            };
            let mut found = false;
            let mut tried_paths = Vec::new();
            for &ext in &[".rs", "/mod.rs"] {
                let relative_path = TreeRelativePathBuf::new(dir.join(format!("{mod_name}{ext}")));
                let full_path = relative_path.within(&self.root);
                if full_path.is_file() {
                    trace!("found submodule in {full_path}");
                    self.more_files.push(relative_path);
                    found = true;
                    break;
                } else {
                    tried_paths.push(full_path);
                }
            }
            if !found {
                warn!(
                    "{path}:{line}: referent of mod {mod_name:#?} not found: tried {tried_paths:?}",
                    path = self.source_file.tree_relative_path,
                    line = node.mod_token.span.start().line,
                );
            }
        }
        self.in_namespace(mod_name, |v| syn::visit::visit_item_mod(v, node));
    }
}

fn return_type_to_string(return_type: &ReturnType) -> String {
    match return_type {
        ReturnType::Default => String::new(),
        ReturnType::Type(arrow, typ) => {
            format!(
                "{} {}",
                arrow.to_token_stream(),
                tokens_to_pretty_string(typ)
            )
        }
    }
}

/// Convert a TokenStream representing some code to a reasonably formatted
/// string of Rust code.
///
/// [TokenStream] has a `to_string`, but it adds spaces in places that don't
/// look idiomatic, so this reimplements it in a way that looks better.
///
/// This is probably not correctly formatted for all Rust syntax, and only tries
/// to cover cases that can emerge from the code we generate.
fn tokens_to_pretty_string<T: ToTokens>(t: T) -> String {
    use TokenTree::*;
    let mut b = String::with_capacity(200);
    let mut ts = t.to_token_stream().into_iter().peekable();
    while let Some(tt) = ts.next() {
        let next = ts.peek();
        match tt {
            Punct(p) => {
                let pc = p.as_char();
                b.push(pc);
                if b.ends_with(" ->") || pc == ',' {
                    b.push(' ');
                }
            }
            Ident(_) | Literal(_) => {
                match tt {
                    Literal(l) => b.push_str(&l.to_string()),
                    Ident(i) => b.push_str(&i.to_string()),
                    _ => unreachable!(),
                };
                if let Some(next) = next {
                    match next {
                        Ident(_) | Literal(_) => b.push(' '),
                        Punct(p) => match p.as_char() {
                            ',' | ';' | '<' | '>' | ':' | '.' | '!' => (),
                            _ => b.push(' '),
                        },
                        Group(_) => (),
                    }
                }
            }
            Group(g) => {
                //     let has_space = match g.delimiter() {
                //         Delimiter::Brace | Delimiter::Bracket => true,
                //         Delimiter::Parenthesis | Delimiter::None => false,
                //     };
                //     if soft_space && has_space {
                //         b.push(' ');
                //     }
                match g.delimiter() {
                    Delimiter::Brace => b.push('{'),
                    Delimiter::Bracket => b.push('['),
                    Delimiter::Parenthesis => b.push('('),
                    Delimiter::None => (),
                }
                b.push_str(&tokens_to_pretty_string(g.stream()));
                match g.delimiter() {
                    Delimiter::Brace => b.push('}'),
                    Delimiter::Bracket => b.push(']'),
                    Delimiter::Parenthesis => b.push(')'),
                    Delimiter::None => (),
                }
            }
        }
    }
    debug_assert!(
        !b.ends_with(' '),
        "generated a trailing space: ts={ts:?}, b={b:?}",
        ts = t.to_token_stream(),
    );
    b
}

fn path_is_result(path: &syn::Path) -> bool {
    path.segments
        .last()
        .map(|segment| segment.ident == "Result")
        .unwrap_or_default()
}

/// True if the signature of a function is such that it should be excluded.
fn fn_sig_excluded(sig: &syn::Signature) -> bool {
    if sig.unsafety.is_some() {
        trace!("Skip unsafe fn");
        true
    } else {
        false
    }
}

/// True if any of the attrs indicate that we should skip this node and everything inside it.
fn attrs_excluded(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr_is_cfg_test(attr) || attr_is_test(attr) || attr_is_mutants_skip(attr))
}

/// True if the block (e.g. the contents of a function) is empty.
fn block_is_empty(block: &syn::Block) -> bool {
    block.stmts.is_empty()
}

/// True if the attribute looks like `#[cfg(test)]`, or has "test"
/// anywhere in it.
fn attr_is_cfg_test(attr: &Attribute) -> bool {
    if !path_is(attr.path(), &["cfg"]) {
        return false;
    }
    let mut contains_test = false;
    if let Err(err) = attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("test") {
            contains_test = true;
        }
        Ok(())
    }) {
        debug!(
            ?err,
            ?attr,
            "Attribute is not in conventional form; skipped"
        );
        return false;
    }
    contains_test
}

/// True if the attribute is `#[test]`.
fn attr_is_test(attr: &Attribute) -> bool {
    attr.path().is_ident("test")
}

fn path_is(path: &syn::Path, idents: &[&str]) -> bool {
    path.segments.iter().map(|ps| &ps.ident).eq(idents.iter())
}

/// True if the attribute contains `mutants::skip`.
///
/// This for example returns true for `#[mutants::skip] or `#[cfg_attr(test, mutants::skip)]`.
fn attr_is_mutants_skip(attr: &Attribute) -> bool {
    if path_is(attr.path(), &["mutants", "skip"]) {
        return true;
    }
    if !path_is(attr.path(), &["cfg_attr"]) {
        return false;
    }
    let mut skip = false;
    if let Err(err) = attr.parse_nested_meta(|meta| {
        if path_is(&meta.path, &["mutants", "skip"]) {
            skip = true
        }
        Ok(())
    }) {
        debug!(
            ?attr,
            ?err,
            "Attribute is not a path with attributes; skipping"
        );
        return false;
    }
    skip
}

#[cfg(test)]
mod test {
    use quote::quote;

    #[test]
    fn path_is_result() {
        let path: syn::Path = syn::parse_quote! { Result<(), ()> };
        assert!(super::path_is_result(&path));
    }

    #[test]
    fn tokens_to_pretty_string() {
        use super::tokens_to_pretty_string;

        assert_eq!(
            tokens_to_pretty_string(quote! {
                <impl Iterator for MergeTrees < AE , BE , AIT , BIT > > :: next
                -> Option < Self ::  Item >
            }),
            "<impl Iterator for MergeTrees<AE, BE, AIT, BIT>>::next -> Option<Self::Item>"
        );
        assert_eq!(
            tokens_to_pretty_string(quote! { Lex < 'buf >::take }),
            "Lex<'buf>::take"
        );
    }
}
