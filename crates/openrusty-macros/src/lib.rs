//! Procedural macros for the OpenRusty plugin SDK.
//!
//! Provides `#[phase(<name>)]`, which renames the annotated handler to the
//! canonical `__orr_phase_<name>` symbol (signature, body and visibility
//! preserved) while keeping the original name usable as an alias, so
//! `openrusty_sdk::dispatch!` and user code can still call it by name.
//!
//! The macro also validates the handler shape up front (`fn() -> Decision`),
//! so a wrong signature fails with one clear message instead of confusing
//! downstream errors in `dispatch!` or the ABI encoding.

extern crate proc_macro;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_macro_input, Ident, ItemFn, ReturnType};

/// Phase names accepted by `#[phase(...)]` (see docs/wasm-abi.md).
const PHASES: &[&str] = &[
    "post_read",
    "rewrite",
    "access",
    "content",
    "balancer",
    "header_filter",
    "body_filter",
    "log",
];

/// True when `ty` is the SDK decision type: `Decision` (after
/// `use openrusty_sdk::Decision;`) or `openrusty_sdk::Decision` (the
/// crate-root re-export in `openrusty-sdk/src/lib.rs`).
fn is_decision(ty: &syn::Type) -> bool {
    let syn::Type::Path(path) = ty else {
        return false;
    };
    let names: Vec<String> = path
        .path
        .segments
        .iter()
        .map(|seg| seg.ident.to_string())
        .collect();
    names.as_slice() == ["Decision"] || names.as_slice() == ["openrusty_sdk", "Decision"]
}

/// Signature diagnostics for one annotated handler: empty when the handler
/// matches the `fn() -> Decision` contract, otherwise one compile error per
/// violated rule.
fn signature_errors(func: &ItemFn) -> TokenStream2 {
    let mut errors = TokenStream2::new();
    if !func.sig.inputs.is_empty() {
        errors.extend(
            syn::Error::new_spanned(
                &func.sig.inputs,
                "#[phase] handler must take no parameters; \
                 phases are invoked as `fn() -> Decision`",
            )
            .to_compile_error(),
        );
    }
    let returns_decision = matches!(&func.sig.output, ReturnType::Type(_, ty) if is_decision(ty));
    if !returns_decision {
        errors.extend(
            syn::Error::new_spanned(
                &func.sig.output,
                "#[phase] handler must return Decision (openrusty_sdk::Decision)",
            )
            .to_compile_error(),
        );
    }
    errors
}

/// Mark a function as the handler for one nginx-like request phase.
///
/// ```ignore
/// #[openrusty_sdk::phase(balancer)]
/// fn pick_peer() -> openrusty_sdk::Decision {
///     openrusty_sdk::Decision::Declined
/// }
/// ```
///
/// The function is renamed to `__orr_phase_<name>` (signature, body and
/// visibility intact) and re-aliased under its original name. Unknown phase
/// names produce a compile error, as do handlers that take parameters or do
/// not return `Decision`.
#[proc_macro_attribute]
pub fn phase(attr: TokenStream, item: TokenStream) -> TokenStream {
    let name = parse_macro_input!(attr as Ident);
    let phase_name = name.to_string();
    if !PHASES.contains(&phase_name.as_str()) {
        return syn::Error::new(
            name.span(),
            format!(
                "unknown phase `{phase_name}`; expected one of: {}",
                PHASES.join(", ")
            ),
        )
        .to_compile_error()
        .into();
    }

    let mut func = parse_macro_input!(item as ItemFn);
    let errors = signature_errors(&func);
    if !errors.is_empty() {
        // Keep the original function so user code still resolves; the
        // errors carry a span on the offending signature part.
        return quote! { #func #errors }.into();
    }

    let original = func.sig.ident.clone();
    let vis = func.vis.clone();
    func.sig.ident = Ident::new(&format!("__orr_phase_{phase_name}"), name.span());
    let renamed = func.sig.ident.clone();

    let expanded = quote! {
        #func
        #vis use self::#renamed as #original;
    };
    expanded.into()
}
