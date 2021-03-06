// Copyright 2014-2018 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.


use crate::rustc::hir::def_id::DefId;
use crate::rustc::hir::*;
use crate::rustc::lint::{LateContext, LateLintPass, LintArray, LintPass};
use crate::rustc::{declare_tool_lint, lint_array};
use crate::rustc::ty;
use crate::rustc_data_structures::fx::FxHashSet;
use crate::syntax::ast::{Lit, LitKind, Name};
use crate::syntax::source_map::{Span, Spanned};
use crate::utils::{get_item_name, in_macro, snippet, span_lint, span_lint_and_sugg, walk_ptrs_ty};

/// **What it does:** Checks for getting the length of something via `.len()`
/// just to compare to zero, and suggests using `.is_empty()` where applicable.
///
/// **Why is this bad?** Some structures can answer `.is_empty()` much faster
/// than calculating their length. Notably, for slices, getting the length
/// requires a subtraction whereas `.is_empty()` is just a comparison. So it is
/// good to get into the habit of using `.is_empty()`, and having it is cheap.
/// Besides, it makes the intent clearer than a manual comparison.
///
/// **Known problems:** None.
///
/// **Example:**
/// ```rust
/// if x.len() == 0 { .. }
/// if y.len() != 0 { .. }
/// ```
/// instead use
/// ```rust
/// if x.len().is_empty() { .. }
/// if !y.len().is_empty() { .. }
/// ```
declare_clippy_lint! {
    pub LEN_ZERO,
    style,
    "checking `.len() == 0` or `.len() > 0` (or similar) when `.is_empty()` \
     could be used instead"
}

/// **What it does:** Checks for items that implement `.len()` but not
/// `.is_empty()`.
///
/// **Why is this bad?** It is good custom to have both methods, because for
/// some data structures, asking about the length will be a costly operation,
/// whereas `.is_empty()` can usually answer in constant time. Also it used to
/// lead to false positives on the [`len_zero`](#len_zero) lint – currently that
/// lint will ignore such entities.
///
/// **Known problems:** None.
///
/// **Example:**
/// ```rust
/// impl X {
///     pub fn len(&self) -> usize { .. }
/// }
/// ```
declare_clippy_lint! {
    pub LEN_WITHOUT_IS_EMPTY,
    style,
    "traits or impls with a public `len` method but no corresponding `is_empty` method"
}

#[derive(Copy, Clone)]
pub struct LenZero;

impl LintPass for LenZero {
    fn get_lints(&self) -> LintArray {
        lint_array!(LEN_ZERO, LEN_WITHOUT_IS_EMPTY)
    }
}

impl<'a, 'tcx> LateLintPass<'a, 'tcx> for LenZero {
    fn check_item(&mut self, cx: &LateContext<'a, 'tcx>, item: &'tcx Item) {
        if in_macro(item.span) {
            return;
        }

        match item.node {
            ItemKind::Trait(_, _, _, _, ref trait_items) => check_trait_items(cx, item, trait_items),
            ItemKind::Impl(_, _, _, _, None, _, ref impl_items) => check_impl_items(cx, item, impl_items),
            _ => (),
        }
    }

    fn check_expr(&mut self, cx: &LateContext<'a, 'tcx>, expr: &'tcx Expr) {
        if in_macro(expr.span) {
            return;
        }

        if let ExprKind::Binary(Spanned { node: cmp, .. }, ref left, ref right) = expr.node {
            match cmp {
                BinOpKind::Eq => {
                    check_cmp(cx, expr.span, left, right, "", 0); // len == 0
                    check_cmp(cx, expr.span, right, left, "", 0); // 0 == len
                },
                BinOpKind::Ne => {
                    check_cmp(cx, expr.span, left, right, "!", 0); // len != 0
                    check_cmp(cx, expr.span, right, left, "!", 0); // 0 != len
                },
                BinOpKind::Gt => {
                    check_cmp(cx, expr.span, left, right, "!", 0); // len > 0
                    check_cmp(cx, expr.span, right, left, "", 1); // 1 > len
                },
                BinOpKind::Lt => {
                    check_cmp(cx, expr.span, left, right, "", 1); // len < 1
                    check_cmp(cx, expr.span, right, left, "!", 0); // 0 < len
                },
                BinOpKind::Ge => check_cmp(cx, expr.span, left, right, "!", 1), // len <= 1
                BinOpKind::Le => check_cmp(cx, expr.span, right, left, "!", 1), // 1 >= len
                _ => (),
            }
        }
    }
}

fn check_trait_items(cx: &LateContext<'_, '_>, visited_trait: &Item, trait_items: &[TraitItemRef]) {
    fn is_named_self(cx: &LateContext<'_, '_>, item: &TraitItemRef, name: &str) -> bool {
        item.ident.name == name && if let AssociatedItemKind::Method { has_self } = item.kind {
            has_self && {
                let did = cx.tcx.hir.local_def_id(item.id.node_id);
                cx.tcx.fn_sig(did).inputs().skip_binder().len() == 1
            }
        } else {
            false
        }
    }

    // fill the set with current and super traits
    fn fill_trait_set(traitt: DefId, set: &mut FxHashSet<DefId>, cx: &LateContext<'_, '_>) {
        if set.insert(traitt) {
            for supertrait in crate::rustc::traits::supertrait_def_ids(cx.tcx, traitt) {
                fill_trait_set(supertrait, set, cx);
            }
        }
    }

    if cx.access_levels.is_exported(visited_trait.id) && trait_items.iter().any(|i| is_named_self(cx, i, "len")) {
        let mut current_and_super_traits = FxHashSet::default();
        let visited_trait_def_id = cx.tcx.hir.local_def_id(visited_trait.id);
        fill_trait_set(visited_trait_def_id, &mut current_and_super_traits, cx);

        let is_empty_method_found = current_and_super_traits
            .iter()
            .flat_map(|&i| cx.tcx.associated_items(i))
            .any(|i| {
                i.kind == ty::AssociatedKind::Method && i.method_has_self_argument && i.ident.name == "is_empty"
                    && cx.tcx.fn_sig(i.def_id).inputs().skip_binder().len() == 1
            });

        if !is_empty_method_found {
            span_lint(
                cx,
                LEN_WITHOUT_IS_EMPTY,
                visited_trait.span,
                &format!(
                    "trait `{}` has a `len` method but no (possibly inherited) `is_empty` method",
                    visited_trait.name
                ),
            );
        }
    }
}

fn check_impl_items(cx: &LateContext<'_, '_>, item: &Item, impl_items: &[ImplItemRef]) {
    fn is_named_self(cx: &LateContext<'_, '_>, item: &ImplItemRef, name: &str) -> bool {
        item.ident.name == name && if let AssociatedItemKind::Method { has_self } = item.kind {
            has_self && {
                let did = cx.tcx.hir.local_def_id(item.id.node_id);
                cx.tcx.fn_sig(did).inputs().skip_binder().len() == 1
            }
        } else {
            false
        }
    }

    let is_empty = if let Some(is_empty) = impl_items.iter().find(|i| is_named_self(cx, i, "is_empty")) {
        if cx.access_levels.is_exported(is_empty.id.node_id) {
            return;
        } else {
            "a private"
        }
    } else {
        "no corresponding"
    };

    if let Some(i) = impl_items.iter().find(|i| is_named_self(cx, i, "len")) {
        if cx.access_levels.is_exported(i.id.node_id) {
            let def_id = cx.tcx.hir.local_def_id(item.id);
            let ty = cx.tcx.type_of(def_id);

            span_lint(
                cx,
                LEN_WITHOUT_IS_EMPTY,
                item.span,
                &format!(
                    "item `{}` has a public `len` method but {} `is_empty` method",
                    ty, is_empty
                ),
            );
        }
    }
}

fn check_cmp(cx: &LateContext<'_, '_>, span: Span, method: &Expr, lit: &Expr, op: &str, compare_to: u32) {
    if let (&ExprKind::MethodCall(ref method_path, _, ref args), &ExprKind::Lit(ref lit)) = (&method.node, &lit.node) {
        // check if we are in an is_empty() method
        if let Some(name) = get_item_name(cx, method) {
            if name == "is_empty" {
                return;
            }
        }

        check_len(cx, span, method_path.ident.name, args, lit, op, compare_to)
    }
}

fn check_len(cx: &LateContext<'_, '_>, span: Span, method_name: Name, args: &[Expr], lit: &Lit, op: &str, compare_to: u32) {
    if let Spanned {
        node: LitKind::Int(lit, _),
        ..
    } = *lit
    {
        // check if length is compared to the specified number
        if lit != u128::from(compare_to) {
            return;
        }

        if method_name == "len" && args.len() == 1 && has_is_empty(cx, &args[0]) {
            span_lint_and_sugg(
                cx,
                LEN_ZERO,
                span,
                &format!("length comparison to {}", if compare_to == 0 { "zero" } else { "one" }),
                "using `is_empty` is clearer and more explicit",
                format!("{}{}.is_empty()", op, snippet(cx, args[0].span, "_")),
            );
        }
    }
}

/// Check if this type has an `is_empty` method.
fn has_is_empty(cx: &LateContext<'_, '_>, expr: &Expr) -> bool {
    /// Get an `AssociatedItem` and return true if it matches `is_empty(self)`.
    fn is_is_empty(cx: &LateContext<'_, '_>, item: &ty::AssociatedItem) -> bool {
        if let ty::AssociatedKind::Method = item.kind {
            if item.ident.name == "is_empty" {
                let sig = cx.tcx.fn_sig(item.def_id);
                let ty = sig.skip_binder();
                ty.inputs().len() == 1
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Check the inherent impl's items for an `is_empty(self)` method.
    fn has_is_empty_impl(cx: &LateContext<'_, '_>, id: DefId) -> bool {
        cx.tcx.inherent_impls(id).iter().any(|imp| {
            cx.tcx
                .associated_items(*imp)
                .any(|item| is_is_empty(cx, &item))
        })
    }

    let ty = &walk_ptrs_ty(cx.tables.expr_ty(expr));
    match ty.sty {
        ty::Dynamic(ref tt, ..) => cx.tcx
            .associated_items(tt.principal().def_id())
            .any(|item| is_is_empty(cx, &item)),
        ty::Projection(ref proj) => has_is_empty_impl(cx, proj.item_def_id),
        ty::Adt(id, _) => has_is_empty_impl(cx, id.did),
        ty::Array(..) | ty::Slice(..) | ty::Str => true,
        _ => false,
    }
}
