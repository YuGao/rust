// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use check::{Inherited, FnCtxt};
use constrained_type_params::{identify_constrained_type_params, Parameter};

use hir::def_id::DefId;
use rustc::traits::{self, ObligationCauseCode};
use rustc::ty::{self, Lift, Ty, TyCtxt, GenericParamDefKind, TypeFoldable};
use rustc::ty::subst::{Subst, Substs};
use rustc::ty::util::ExplicitSelf;
use rustc::util::nodemap::{FxHashSet, FxHashMap};
use rustc::middle::lang_items;
use rustc::infer::anon_types::may_define_existential_type;

use syntax::ast;
use syntax::feature_gate::{self, GateIssue};
use syntax_pos::Span;
use errors::{DiagnosticBuilder, DiagnosticId};

use rustc::hir::intravisit::{self, Visitor, NestedVisitorMap};
use rustc::hir;

/// Helper type of a temporary returned by .for_item(...).
/// Necessary because we can't write the following bound:
/// F: for<'b, 'tcx> where 'gcx: 'tcx FnOnce(FnCtxt<'b, 'gcx, 'tcx>).
struct CheckWfFcxBuilder<'a, 'gcx: 'a+'tcx, 'tcx: 'a> {
    inherited: super::InheritedBuilder<'a, 'gcx, 'tcx>,
    id: ast::NodeId,
    span: Span,
    param_env: ty::ParamEnv<'tcx>,
}

impl<'a, 'gcx, 'tcx> CheckWfFcxBuilder<'a, 'gcx, 'tcx> {
    fn with_fcx<F>(&'tcx mut self, f: F) where
        F: for<'b> FnOnce(&FnCtxt<'b, 'gcx, 'tcx>,
                         TyCtxt<'b, 'gcx, 'gcx>) -> Vec<Ty<'tcx>>
    {
        let id = self.id;
        let span = self.span;
        let param_env = self.param_env;
        self.inherited.enter(|inh| {
            let fcx = FnCtxt::new(&inh, param_env, id);
            if !inh.tcx.features().trivial_bounds {
                // As predicates are cached rather than obligations, this
                // needsto be called first so that they are checked with an
                // empty param_env.
                check_false_global_bounds(&fcx, span, id);
            }
            let wf_tys = f(&fcx, fcx.tcx.global_tcx());
            fcx.select_all_obligations_or_error();
            fcx.regionck_item(id, span, &wf_tys);
        });
    }
}

/// Checks that the field types (in a struct def'n) or argument types (in an enum def'n) are
/// well-formed, meaning that they do not require any constraints not declared in the struct
/// definition itself. For example, this definition would be illegal:
///
///     struct Ref<'a, T> { x: &'a T }
///
/// because the type did not declare that `T:'a`.
///
/// We do this check as a pre-pass before checking fn bodies because if these constraints are
/// not included it frequently leads to confusing errors in fn bodies. So it's better to check
/// the types first.
pub fn check_item_well_formed<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, def_id: DefId) {
    let node_id = tcx.hir.as_local_node_id(def_id).unwrap();
    let item = tcx.hir.expect_item(node_id);

    debug!("check_item_well_formed(it.id={}, it.name={})",
            item.id,
            tcx.item_path_str(def_id));

    match item.node {
        // Right now we check that every default trait implementation
        // has an implementation of itself. Basically, a case like:
        //
        // `impl Trait for T {}`
        //
        // has a requirement of `T: Trait` which was required for default
        // method implementations. Although this could be improved now that
        // there's a better infrastructure in place for this, it's being left
        // for a follow-up work.
        //
        // Since there's such a requirement, we need to check *just* positive
        // implementations, otherwise things like:
        //
        // impl !Send for T {}
        //
        // won't be allowed unless there's an *explicit* implementation of `Send`
        // for `T`
        hir::ItemKind::Impl(_, polarity, defaultness, _, ref trait_ref, ref self_ty, _) => {
            let is_auto = tcx.impl_trait_ref(tcx.hir.local_def_id(item.id))
                                .map_or(false, |trait_ref| tcx.trait_is_auto(trait_ref.def_id));
            if let (hir::Defaultness::Default { .. }, true) = (defaultness, is_auto) {
                tcx.sess.span_err(item.span, "impls of auto traits cannot be default");
            }
            if polarity == hir::ImplPolarity::Positive {
                check_impl(tcx, item, self_ty, trait_ref);
            } else {
                // FIXME(#27579) what amount of WF checking do we need for neg impls?
                if trait_ref.is_some() && !is_auto {
                    span_err!(tcx.sess, item.span, E0192,
                                "negative impls are only allowed for \
                                auto traits (e.g., `Send` and `Sync`)")
                }
            }
        }
        hir::ItemKind::Fn(..) => {
            check_item_fn(tcx, item);
        }
        hir::ItemKind::Static(..) => {
            check_item_type(tcx, item);
        }
        hir::ItemKind::Const(..) => {
            check_item_type(tcx, item);
        }
        hir::ItemKind::Struct(ref struct_def, ref ast_generics) => {
            check_type_defn(tcx, item, false, |fcx| {
                vec![fcx.non_enum_variant(struct_def)]
            });

            check_variances_for_type_defn(tcx, item, ast_generics);
        }
        hir::ItemKind::Union(ref struct_def, ref ast_generics) => {
            check_type_defn(tcx, item, true, |fcx| {
                vec![fcx.non_enum_variant(struct_def)]
            });

            check_variances_for_type_defn(tcx, item, ast_generics);
        }
        hir::ItemKind::Enum(ref enum_def, ref ast_generics) => {
            check_type_defn(tcx, item, true, |fcx| {
                fcx.enum_variants(enum_def)
            });

            check_variances_for_type_defn(tcx, item, ast_generics);
        }
        hir::ItemKind::Trait(..) => {
            check_trait(tcx, item);
        }
        _ => {}
    }
}

pub fn check_trait_item<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, def_id: DefId) {
    let node_id = tcx.hir.as_local_node_id(def_id).unwrap();
    let trait_item = tcx.hir.expect_trait_item(node_id);

    let method_sig = match trait_item.node {
        hir::TraitItemKind::Method(ref sig, _) => Some(sig),
        _ => None
    };
    check_associated_item(tcx, trait_item.id, trait_item.span, method_sig);
}

pub fn check_impl_item<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, def_id: DefId) {
    let node_id = tcx.hir.as_local_node_id(def_id).unwrap();
    let impl_item = tcx.hir.expect_impl_item(node_id);

    let method_sig = match impl_item.node {
        hir::ImplItemKind::Method(ref sig, _) => Some(sig),
        _ => None
    };
    check_associated_item(tcx, impl_item.id, impl_item.span, method_sig);
}

fn check_associated_item<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                            item_id: ast::NodeId,
                            span: Span,
                            sig_if_method: Option<&hir::MethodSig>) {
    let code = ObligationCauseCode::MiscObligation;
    for_id(tcx, item_id, span).with_fcx(|fcx, tcx| {
        let item = fcx.tcx.associated_item(fcx.tcx.hir.local_def_id(item_id));

        let (mut implied_bounds, self_ty) = match item.container {
            ty::TraitContainer(_) => (vec![], fcx.tcx.mk_self_type()),
            ty::ImplContainer(def_id) => (fcx.impl_implied_bounds(def_id, span),
                                            fcx.tcx.type_of(def_id))
        };

        match item.kind {
            ty::AssociatedKind::Const => {
                let ty = fcx.tcx.type_of(item.def_id);
                let ty = fcx.normalize_associated_types_in(span, &ty);
                fcx.register_wf_obligation(ty, span, code.clone());
            }
            ty::AssociatedKind::Method => {
                reject_shadowing_parameters(fcx.tcx, item.def_id);
                let sig = fcx.tcx.fn_sig(item.def_id);
                let sig = fcx.normalize_associated_types_in(span, &sig);
                check_fn_or_method(tcx, fcx, span, sig,
                                        item.def_id, &mut implied_bounds);
                let sig_if_method = sig_if_method.expect("bad signature for method");
                check_method_receiver(fcx, sig_if_method, &item, self_ty);
            }
            ty::AssociatedKind::Type => {
                if item.defaultness.has_value() {
                    let ty = fcx.tcx.type_of(item.def_id);
                    let ty = fcx.normalize_associated_types_in(span, &ty);
                    fcx.register_wf_obligation(ty, span, code.clone());
                }
            }
            ty::AssociatedKind::Existential => {
                // do nothing, existential types check themselves
            }
        }

        implied_bounds
    })
}

fn for_item<'a, 'gcx, 'tcx>(tcx: TyCtxt<'a, 'gcx, 'gcx>, item: &hir::Item)
                    -> CheckWfFcxBuilder<'a, 'gcx, 'tcx> {
    for_id(tcx, item.id, item.span)
}

fn for_id<'a, 'gcx, 'tcx>(tcx: TyCtxt<'a, 'gcx, 'gcx>, id: ast::NodeId, span: Span)
                -> CheckWfFcxBuilder<'a, 'gcx, 'tcx> {
    let def_id = tcx.hir.local_def_id(id);
    CheckWfFcxBuilder {
        inherited: Inherited::build(tcx, def_id),
        id,
        span,
        param_env: tcx.param_env(def_id),
    }
}

/// In a type definition, we check that to ensure that the types of the fields are well-formed.
fn check_type_defn<'a, 'tcx, F>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                item: &hir::Item, all_sized: bool, mut lookup_fields: F)
    where F: for<'fcx, 'gcx, 'tcx2> FnMut(&FnCtxt<'fcx, 'gcx, 'tcx2>) -> Vec<AdtVariant<'tcx2>>
{
    for_item(tcx, item).with_fcx(|fcx, fcx_tcx| {
        let variants = lookup_fields(fcx);
        let def_id = fcx.tcx.hir.local_def_id(item.id);
        let packed = fcx.tcx.adt_def(def_id).repr.packed();

        for variant in &variants {
            // For DST, or when drop needs to copy things around, all
            // intermediate types must be sized.
            let needs_drop_copy = || {
                packed && {
                    let ty = variant.fields.last().unwrap().ty;
                    let ty = fcx.tcx.erase_regions(&ty).lift_to_tcx(fcx_tcx)
                        .unwrap_or_else(|| {
                            span_bug!(item.span, "inference variables in {:?}", ty)
                        });
                    ty.needs_drop(fcx_tcx, fcx_tcx.param_env(def_id))
                }
            };
            let all_sized =
                all_sized ||
                variant.fields.is_empty() ||
                needs_drop_copy();
            let unsized_len = if all_sized {
                0
            } else {
                1
            };
            for (idx, field) in variant.fields[..variant.fields.len() - unsized_len]
                .iter()
                .enumerate()
            {
                let last = idx == variant.fields.len() - 1;
                fcx.register_bound(
                    field.ty,
                    fcx.tcx.require_lang_item(lang_items::SizedTraitLangItem),
                    traits::ObligationCause::new(
                        field.span,
                        fcx.body_id,
                        traits::FieldSized {
                            adt_kind: match item.node.adt_kind() {
                                Some(i) => i,
                                None => bug!(),
                            },
                            last
                        }
                    )
                );
            }

            // All field types must be well-formed.
            for field in &variant.fields {
                fcx.register_wf_obligation(field.ty, field.span,
                    ObligationCauseCode::MiscObligation)
            }
        }

        check_where_clauses(tcx, fcx, item.span, def_id, None);

        vec![] // no implied bounds in a struct def'n
    });
}

fn check_trait<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, item: &hir::Item) {
    let trait_def_id = tcx.hir.local_def_id(item.id);
    for_item(tcx, item).with_fcx(|fcx, _| {
        check_where_clauses(tcx, fcx, item.span, trait_def_id, None);
        vec![]
    });
}

fn check_item_fn<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, item: &hir::Item) {
    for_item(tcx, item).with_fcx(|fcx, tcx| {
        let def_id = fcx.tcx.hir.local_def_id(item.id);
        let sig = fcx.tcx.fn_sig(def_id);
        let sig = fcx.normalize_associated_types_in(item.span, &sig);
        let mut implied_bounds = vec![];
        check_fn_or_method(tcx, fcx, item.span, sig,
                                def_id, &mut implied_bounds);
        implied_bounds
    })
}

fn check_item_type<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                    item: &hir::Item)
{
    debug!("check_item_type: {:?}", item);

    for_item(tcx, item).with_fcx(|fcx, _this| {
        let ty = fcx.tcx.type_of(fcx.tcx.hir.local_def_id(item.id));
        let item_ty = fcx.normalize_associated_types_in(item.span, &ty);

        fcx.register_wf_obligation(item_ty, item.span, ObligationCauseCode::MiscObligation);

        vec![] // no implied bounds in a const etc
    });
}

fn check_impl<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                item: &hir::Item,
                ast_self_ty: &hir::Ty,
                ast_trait_ref: &Option<hir::TraitRef>)
{
    debug!("check_impl: {:?}", item);

    for_item(tcx, item).with_fcx(|fcx, tcx| {
        let item_def_id = fcx.tcx.hir.local_def_id(item.id);

        match *ast_trait_ref {
            Some(ref ast_trait_ref) => {
                let trait_ref = fcx.tcx.impl_trait_ref(item_def_id).unwrap();
                let trait_ref =
                    fcx.normalize_associated_types_in(
                        ast_trait_ref.path.span, &trait_ref);
                let obligations =
                    ty::wf::trait_obligations(fcx,
                                                fcx.param_env,
                                                fcx.body_id,
                                                &trait_ref,
                                                ast_trait_ref.path.span);
                for obligation in obligations {
                    fcx.register_predicate(obligation);
                }
            }
            None => {
                let self_ty = fcx.tcx.type_of(item_def_id);
                let self_ty = fcx.normalize_associated_types_in(item.span, &self_ty);
                fcx.register_wf_obligation(self_ty, ast_self_ty.span,
                    ObligationCauseCode::MiscObligation);
            }
        }

        check_where_clauses(tcx, fcx, item.span, item_def_id, None);

        fcx.impl_implied_bounds(item_def_id, item.span)
    });
}

/// Checks where clauses and inline bounds that are declared on def_id.
fn check_where_clauses<'a, 'gcx, 'fcx, 'tcx>(
    tcx: TyCtxt<'a, 'gcx, 'gcx>,
    fcx: &FnCtxt<'fcx, 'gcx, 'tcx>,
    span: Span,
    def_id: DefId,
    return_ty: Option<Ty<'tcx>>,
) {
    use ty::subst::Subst;
    use rustc::ty::TypeFoldable;

    let predicates = fcx.tcx.predicates_of(def_id);

    let generics = tcx.generics_of(def_id);
    let is_our_default = |def: &ty::GenericParamDef| {
        match def.kind {
            GenericParamDefKind::Type { has_default, .. } => {
                has_default && def.index >= generics.parent_count as u32
            }
            _ => unreachable!()
        }
    };

    // Check that concrete defaults are well-formed. See test `type-check-defaults.rs`.
    // For example this forbids the declaration:
    // struct Foo<T = Vec<[u32]>> { .. }
    // Here the default `Vec<[u32]>` is not WF because `[u32]: Sized` does not hold.
    for param in &generics.params {
        if let GenericParamDefKind::Type {..} = param.kind {
            if is_our_default(&param) {
                let ty = fcx.tcx.type_of(param.def_id);
                // ignore dependent defaults -- that is, where the default of one type
                // parameter includes another (e.g., <T, U = T>). In those cases, we can't
                // be sure if it will error or not as user might always specify the other.
                if !ty.needs_subst() {
                    fcx.register_wf_obligation(ty, fcx.tcx.def_span(param.def_id),
                        ObligationCauseCode::MiscObligation);
                }
            }
        }
    }

    // Check that trait predicates are WF when params are substituted by their defaults.
    // We don't want to overly constrain the predicates that may be written but we want to
    // catch cases where a default my never be applied such as `struct Foo<T: Copy = String>`.
    // Therefore we check if a predicate which contains a single type param
    // with a concrete default is WF with that default substituted.
    // For more examples see tests `defaults-well-formedness.rs` and `type-check-defaults.rs`.
    //
    // First we build the defaulted substitution.
    let substs = Substs::for_item(fcx.tcx, def_id, |param, _| {
        match param.kind {
            GenericParamDefKind::Lifetime => {
                // All regions are identity.
                fcx.tcx.mk_param_from_def(param)
            }
            GenericParamDefKind::Type {..} => {
                // If the param has a default,
                if is_our_default(param) {
                    let default_ty = fcx.tcx.type_of(param.def_id);
                    // and it's not a dependent default
                    if !default_ty.needs_subst() {
                        // then substitute with the default.
                        return default_ty.into();
                    }
                }
                // Mark unwanted params as err.
                fcx.tcx.types.err.into()
            }
        }
    });
    // Now we build the substituted predicates.
    let default_obligations = predicates.predicates.iter().flat_map(|&pred| {
        struct CountParams { params: FxHashSet<u32> }
        impl<'tcx> ty::fold::TypeVisitor<'tcx> for CountParams {
            fn visit_ty(&mut self, t: Ty<'tcx>) -> bool {
                match t.sty {
                    ty::Param(p) => {
                        self.params.insert(p.idx);
                        t.super_visit_with(self)
                    }
                    _ => t.super_visit_with(self)
                }
            }

            fn visit_region(&mut self, _: ty::Region<'tcx>) -> bool {
                true
            }
        }
        let mut param_count = CountParams { params: FxHashSet() };
        let has_region = pred.visit_with(&mut param_count);
        let substituted_pred = pred.subst(fcx.tcx, substs);
        // Don't check non-defaulted params, dependent defaults (including lifetimes)
        // or preds with multiple params.
        if {
            substituted_pred.references_error() || param_count.params.len() > 1
                || has_region
        } {
                None
        } else if predicates.predicates.contains(&substituted_pred) {
            // Avoid duplication of predicates that contain no parameters, for example.
            None
        } else {
            Some(substituted_pred)
        }
    }).map(|pred| {
        // convert each of those into an obligation. So if you have
        // something like `struct Foo<T: Copy = String>`, we would
        // take that predicate `T: Copy`, substitute to `String: Copy`
        // (actually that happens in the previous `flat_map` call),
        // and then try to prove it (in this case, we'll fail).
        //
        // Note the subtle difference from how we handle `predicates`
        // below: there, we are not trying to prove those predicates
        // to be *true* but merely *well-formed*.
        let pred = fcx.normalize_associated_types_in(span, &pred);
        let cause = traits::ObligationCause::new(span, fcx.body_id, traits::ItemObligation(def_id));
        traits::Obligation::new(cause, fcx.param_env, pred)
    });

    let mut predicates = predicates.instantiate_identity(fcx.tcx);

    if let Some(return_ty) = return_ty {
        predicates.predicates.extend(check_existential_types(tcx, fcx, def_id, span, return_ty));
    }

    let predicates = fcx.normalize_associated_types_in(span, &predicates);

    debug!("check_where_clauses: predicates={:?}", predicates.predicates);
    let wf_obligations =
        predicates.predicates
                    .iter()
                    .flat_map(|p| ty::wf::predicate_obligations(fcx,
                                                                fcx.param_env,
                                                                fcx.body_id,
                                                                p,
                                                                span));

    for obligation in wf_obligations.chain(default_obligations) {
        debug!("next obligation cause: {:?}", obligation.cause);
        fcx.register_predicate(obligation);
    }
}

fn check_fn_or_method<'a, 'fcx, 'gcx, 'tcx>(tcx: TyCtxt<'a, 'gcx, 'gcx>,
                                    fcx: &FnCtxt<'fcx, 'gcx, 'tcx>,
                                    span: Span,
                                    sig: ty::PolyFnSig<'tcx>,
                                    def_id: DefId,
                                    implied_bounds: &mut Vec<Ty<'tcx>>)
{
    let sig = fcx.normalize_associated_types_in(span, &sig);
    let sig = fcx.tcx.liberate_late_bound_regions(def_id, &sig);

    for input_ty in sig.inputs() {
        fcx.register_wf_obligation(&input_ty, span, ObligationCauseCode::MiscObligation);
    }
    implied_bounds.extend(sig.inputs());

    fcx.register_wf_obligation(sig.output(), span, ObligationCauseCode::MiscObligation);

    // FIXME(#25759) return types should not be implied bounds
    implied_bounds.push(sig.output());

    check_where_clauses(tcx, fcx, span, def_id, Some(sig.output()));
}

/// Checks "defining uses" of existential types to ensure that they meet the restrictions laid for
/// "higher-order pattern unification".
/// This ensures that inference is tractable.
/// In particular, definitions of existential types can only use other generics as arguments,
/// and they cannot repeat an argument. Example:
///
/// ```rust
/// existential type Foo<A, B>;
///
/// // ok -- `Foo` is applied to two distinct, generic types.
/// fn a<T, U>() -> Foo<T, U> { .. }
///
/// // not ok -- `Foo` is applied to `T` twice.
/// fn b<T>() -> Foo<T, T> { .. }
///
///
/// // not ok -- `Foo` is applied to a non-generic type.
/// fn b<T>() -> Foo<T, u32> { .. }
/// ```
///
fn check_existential_types<'a, 'fcx, 'gcx, 'tcx>(
    tcx: TyCtxt<'a, 'gcx, 'gcx>,
    fcx: &FnCtxt<'fcx, 'gcx, 'tcx>,
    fn_def_id: DefId,
    span: Span,
    ty: Ty<'tcx>,
) -> Vec<ty::Predicate<'tcx>> {
    trace!("check_existential_types: {:?}, {:?}", ty, ty.sty);
    let mut substituted_predicates = Vec::new();
    ty.fold_with(&mut ty::fold::BottomUpFolder {
        tcx: fcx.tcx,
        fldop: |ty| {
            if let ty::Anon(def_id, substs) = ty.sty {
                trace!("check_existential_types: anon_ty, {:?}, {:?}", def_id, substs);
                let generics = tcx.generics_of(def_id);
                // only check named existential types
                if generics.parent.is_none() {
                    let anon_node_id = tcx.hir.as_local_node_id(def_id).unwrap();
                    if may_define_existential_type(tcx, fn_def_id, anon_node_id) {
                        trace!("check_existential_types may define. Generics: {:#?}", generics);
                        let mut seen: FxHashMap<_, Vec<_>> = FxHashMap();
                        for (subst, param) in substs.iter().zip(&generics.params) {
                            match subst.unpack() {
                                ty::subst::UnpackedKind::Type(ty) => match ty.sty {
                                    ty::Param(..) => {},
                                    // prevent `fn foo() -> Foo<u32>` from being defining
                                    _ => {
                                        tcx
                                            .sess
                                            .struct_span_err(
                                                span,
                                                "non-defining existential type use \
                                                 in defining scope",
                                            )
                                            .span_note(
                                                tcx.def_span(param.def_id),
                                                &format!(
                                                    "used non-generic type {} for \
                                                     generic parameter",
                                                    ty,
                                                ),
                                            )
                                            .emit();
                                    },
                                }, // match ty
                                ty::subst::UnpackedKind::Lifetime(region) => {
                                    let param_span = tcx.def_span(param.def_id);
                                    if let ty::ReStatic = region {
                                        tcx
                                            .sess
                                            .struct_span_err(
                                                span,
                                                "non-defining existential type use \
                                                    in defining scope",
                                            )
                                            .span_label(
                                                param_span,
                                                "cannot use static lifetime, use a bound lifetime \
                                                instead or remove the lifetime parameter from the \
                                                existential type",
                                            )
                                            .emit();
                                    } else {
                                        seen.entry(region).or_default().push(param_span);
                                    }
                                },
                            } // match subst
                        } // for (subst, param)
                        for (_, spans) in seen {
                            if spans.len() > 1 {
                                tcx
                                    .sess
                                    .struct_span_err(
                                        span,
                                        "non-defining existential type use \
                                            in defining scope",
                                    ).
                                    span_note(
                                        spans,
                                        "lifetime used multiple times",
                                    )
                                    .emit();
                            }
                        }
                    } // if may_define_existential_type

                    // now register the bounds on the parameters of the existential type
                    // so the parameters given by the function need to fulfil them
                    // ```rust
                    // existential type Foo<T: Bar>: 'static;
                    // fn foo<U>() -> Foo<U> { .. *}
                    // ```
                    // becomes
                    // ```rust
                    // existential type Foo<T: Bar>: 'static;
                    // fn foo<U: Bar>() -> Foo<U> { .. *}
                    // ```
                    let predicates = tcx.predicates_of(def_id);
                    trace!(
                        "check_existential_types may define. adding predicates: {:#?}",
                        predicates,
                    );
                    for &pred in predicates.predicates.iter() {
                        let substituted_pred = pred.subst(fcx.tcx, substs);
                        // Avoid duplication of predicates that contain no parameters, for example.
                        if !predicates.predicates.contains(&substituted_pred) {
                            substituted_predicates.push(substituted_pred);
                        }
                    }
                } // if is_named_existential_type
            } // if let Anon
            ty
        },
        reg_op: |reg| reg,
    });
    substituted_predicates
}

fn check_method_receiver<'fcx, 'gcx, 'tcx>(fcx: &FnCtxt<'fcx, 'gcx, 'tcx>,
                                           method_sig: &hir::MethodSig,
                                           method: &ty::AssociatedItem,
                                           self_ty: Ty<'tcx>)
{
    // check that the method has a valid receiver type, given the type `Self`
    debug!("check_method_receiver({:?}, self_ty={:?})",
            method, self_ty);

    if !method.method_has_self_argument {
        return;
    }

    let span = method_sig.decl.inputs[0].span;

    let sig = fcx.tcx.fn_sig(method.def_id);
    let sig = fcx.normalize_associated_types_in(span, &sig);
    let sig = fcx.tcx.liberate_late_bound_regions(method.def_id, &sig);

    debug!("check_method_receiver: sig={:?}", sig);

    let self_ty = fcx.normalize_associated_types_in(span, &self_ty);
    let self_ty = fcx.tcx.liberate_late_bound_regions(
        method.def_id,
        &ty::Binder::bind(self_ty)
    );

    let self_arg_ty = sig.inputs()[0];

    let cause = fcx.cause(span, ObligationCauseCode::MethodReceiver);
    let self_arg_ty = fcx.normalize_associated_types_in(span, &self_arg_ty);
    let self_arg_ty = fcx.tcx.liberate_late_bound_regions(
        method.def_id,
        &ty::Binder::bind(self_arg_ty)
    );

    let mut autoderef = fcx.autoderef(span, self_arg_ty).include_raw_pointers();

    loop {
        if let Some((potential_self_ty, _)) = autoderef.next() {
            debug!("check_method_receiver: potential self type `{:?}` to match `{:?}`",
                potential_self_ty, self_ty);

            if fcx.infcx.can_eq(fcx.param_env, self_ty, potential_self_ty).is_ok() {
                autoderef.finalize();
                if let Some(mut err) = fcx.demand_eqtype_with_origin(
                    &cause, self_ty, potential_self_ty) {
                    err.emit();
                }
                break
            }
        } else {
            fcx.tcx.sess.diagnostic().mut_span_err(
                span, &format!("invalid `self` type: {:?}", self_arg_ty))
            .note(&format!("type must be `{:?}` or a type that dereferences to it", self_ty))
            .help("consider changing to `self`, `&self`, `&mut self`, or `self: Box<Self>`")
            .code(DiagnosticId::Error("E0307".into()))
            .emit();
            return
        }
    }

    let is_self_ty = |ty| fcx.infcx.can_eq(fcx.param_env, self_ty, ty).is_ok();
    let self_kind = ExplicitSelf::determine(self_arg_ty, is_self_ty);

    if !fcx.tcx.features().arbitrary_self_types {
        match self_kind {
            ExplicitSelf::ByValue |
            ExplicitSelf::ByReference(_, _) |
            ExplicitSelf::ByBox => (),

            ExplicitSelf::ByRawPointer(_) => {
                feature_gate::feature_err(
                    &fcx.tcx.sess.parse_sess,
                    "arbitrary_self_types",
                    span,
                    GateIssue::Language,
                    "raw pointer `self` is unstable")
                .help("consider changing to `self`, `&self`, `&mut self`, or `self: Box<Self>`")
                .emit();
            }

            ExplicitSelf::Other => {
                feature_gate::feature_err(
                    &fcx.tcx.sess.parse_sess,
                    "arbitrary_self_types",
                    span,
                    GateIssue::Language,"arbitrary `self` types are unstable")
                .help("consider changing to `self`, `&self`, `&mut self`, or `self: Box<Self>`")
                .emit();
            }
        }
    }
}

fn check_variances_for_type_defn<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                           item: &hir::Item,
                                           hir_generics: &hir::Generics)
{
    let item_def_id = tcx.hir.local_def_id(item.id);
    let ty = tcx.type_of(item_def_id);
    if tcx.has_error_field(ty) {
        return;
    }

    let ty_predicates = tcx.predicates_of(item_def_id);
    assert_eq!(ty_predicates.parent, None);
    let variances = tcx.variances_of(item_def_id);

    let mut constrained_parameters: FxHashSet<_> =
        variances.iter().enumerate()
                    .filter(|&(_, &variance)| variance != ty::Bivariant)
                    .map(|(index, _)| Parameter(index as u32))
                    .collect();

    identify_constrained_type_params(tcx,
                                        ty_predicates.predicates.as_slice(),
                                        None,
                                        &mut constrained_parameters);

    for (index, _) in variances.iter().enumerate() {
        if constrained_parameters.contains(&Parameter(index as u32)) {
            continue;
        }

        let param = &hir_generics.params[index];
        report_bivariance(tcx, param.span, param.name.ident().name);
    }
}

fn report_bivariance<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                        span: Span,
                        param_name: ast::Name)
{
    let mut err = error_392(tcx, span, param_name);

    let suggested_marker_id = tcx.lang_items().phantom_data();
    match suggested_marker_id {
        Some(def_id) => {
            err.help(
                &format!("consider removing `{}` or using a marker such as `{}`",
                            param_name,
                            tcx.item_path_str(def_id)));
        }
        None => {
            // no lang items, no help!
        }
    }
    err.emit();
}

fn reject_shadowing_parameters(tcx: TyCtxt, def_id: DefId) {
    let generics = tcx.generics_of(def_id);
    let parent = tcx.generics_of(generics.parent.unwrap());
    let impl_params: FxHashMap<_, _> = parent.params.iter().flat_map(|param| match param.kind {
        GenericParamDefKind::Lifetime => None,
        GenericParamDefKind::Type {..} => Some((param.name, param.def_id)),
    }).collect();

    for method_param in &generics.params {
        match method_param.kind {
            // Shadowing is checked in resolve_lifetime.
            GenericParamDefKind::Lifetime => continue,
            _ => {},
        };
        if impl_params.contains_key(&method_param.name) {
            // Tighten up the span to focus on only the shadowing type
            let type_span = tcx.def_span(method_param.def_id);

            // The expectation here is that the original trait declaration is
            // local so it should be okay to just unwrap everything.
            let trait_def_id = impl_params[&method_param.name];
            let trait_decl_span = tcx.def_span(trait_def_id);
            error_194(tcx, type_span, trait_decl_span, &method_param.name.as_str()[..]);
        }
    }
}

/// Feature gates RFC 2056 - trivial bounds, checking for global bounds that
/// aren't true.
fn check_false_global_bounds<'a, 'gcx, 'tcx>(
        fcx: &FnCtxt<'a, 'gcx, 'tcx>,
        span: Span,
        id: ast::NodeId,
) {
    use rustc::ty::TypeFoldable;

    let empty_env = ty::ParamEnv::empty();

    let def_id = fcx.tcx.hir.local_def_id(id);
    let predicates = fcx.tcx.predicates_of(def_id).predicates;
    // Check elaborated bounds
    let implied_obligations = traits::elaborate_predicates(fcx.tcx, predicates);

    for pred in implied_obligations {
        // Match the existing behavior.
        if pred.is_global() && !pred.has_late_bound_regions() {
            let pred = fcx.normalize_associated_types_in(span, &pred);
            let obligation = traits::Obligation::new(
                traits::ObligationCause::new(
                    span,
                    id,
                    traits::TrivialBound,
                ),
                empty_env,
                pred,
            );
            fcx.register_predicate(obligation);
        }
    }

    fcx.select_all_obligations_or_error();
}

pub struct CheckTypeWellFormedVisitor<'a, 'tcx: 'a> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
}

impl<'a, 'gcx> CheckTypeWellFormedVisitor<'a, 'gcx> {
    pub fn new(tcx: TyCtxt<'a, 'gcx, 'gcx>)
               -> CheckTypeWellFormedVisitor<'a, 'gcx> {
        CheckTypeWellFormedVisitor {
            tcx,
        }
    }
}

impl<'a, 'tcx, 'v> Visitor<'v> for CheckTypeWellFormedVisitor<'a, 'tcx> {
    fn nested_visit_map<'this>(&'this mut self) -> NestedVisitorMap<'this, 'v> {
        NestedVisitorMap::None
    }

    fn visit_item(&mut self, i: &hir::Item) {
        debug!("visit_item: {:?}", i);
        let def_id = self.tcx.hir.local_def_id(i.id);
        ty::query::queries::check_item_well_formed::ensure(self.tcx, def_id);
        intravisit::walk_item(self, i);
    }

    fn visit_trait_item(&mut self, trait_item: &'v hir::TraitItem) {
        debug!("visit_trait_item: {:?}", trait_item);
        let def_id = self.tcx.hir.local_def_id(trait_item.id);
        ty::query::queries::check_trait_item_well_formed::ensure(self.tcx, def_id);
        intravisit::walk_trait_item(self, trait_item)
    }

    fn visit_impl_item(&mut self, impl_item: &'v hir::ImplItem) {
        debug!("visit_impl_item: {:?}", impl_item);
        let def_id = self.tcx.hir.local_def_id(impl_item.id);
        ty::query::queries::check_impl_item_well_formed::ensure(self.tcx, def_id);
        intravisit::walk_impl_item(self, impl_item)
    }
}

///////////////////////////////////////////////////////////////////////////
// ADT

struct AdtVariant<'tcx> {
    fields: Vec<AdtField<'tcx>>,
}

struct AdtField<'tcx> {
    ty: Ty<'tcx>,
    span: Span,
}

impl<'a, 'gcx, 'tcx> FnCtxt<'a, 'gcx, 'tcx> {
    fn non_enum_variant(&self, struct_def: &hir::VariantData) -> AdtVariant<'tcx> {
        let fields =
            struct_def.fields().iter()
            .map(|field| {
                let field_ty = self.tcx.type_of(self.tcx.hir.local_def_id(field.id));
                let field_ty = self.normalize_associated_types_in(field.span,
                                                                  &field_ty);
                AdtField { ty: field_ty, span: field.span }
            })
            .collect();
        AdtVariant { fields: fields }
    }

    fn enum_variants(&self, enum_def: &hir::EnumDef) -> Vec<AdtVariant<'tcx>> {
        enum_def.variants.iter()
            .map(|variant| self.non_enum_variant(&variant.node.data))
            .collect()
    }

    fn impl_implied_bounds(&self, impl_def_id: DefId, span: Span) -> Vec<Ty<'tcx>> {
        match self.tcx.impl_trait_ref(impl_def_id) {
            Some(ref trait_ref) => {
                // Trait impl: take implied bounds from all types that
                // appear in the trait reference.
                let trait_ref = self.normalize_associated_types_in(span, trait_ref);
                trait_ref.substs.types().collect()
            }

            None => {
                // Inherent impl: take implied bounds from the self type.
                let self_ty = self.tcx.type_of(impl_def_id);
                let self_ty = self.normalize_associated_types_in(span, &self_ty);
                vec![self_ty]
            }
        }
    }
}

fn error_392<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, span: Span, param_name: ast::Name)
                       -> DiagnosticBuilder<'tcx> {
    let mut err = struct_span_err!(tcx.sess, span, E0392,
                  "parameter `{}` is never used", param_name);
    err.span_label(span, "unused type parameter");
    err
}

fn error_194(tcx: TyCtxt, span: Span, trait_decl_span: Span, name: &str) {
    struct_span_err!(tcx.sess, span, E0194,
              "type parameter `{}` shadows another type parameter of the same name",
              name)
        .span_label(span, "shadows another type parameter")
        .span_label(trait_decl_span, format!("first `{}` declared here", name))
        .emit();
}
