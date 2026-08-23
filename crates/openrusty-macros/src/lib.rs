//! Procedural macros for the OpenRusty plugin SDK.
//!
//! Provides `#[phase(<name>)]`, which renames the annotated handler to the
//! canonical `__orr_phase_<name>` symbol (signature, body and visibility
//! preserved) while keeping the original name usable as an alias, so
//! `openrusty_sdk::dispatch!` and user code can still call it by name.

extern crate proc_macro;

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Ident, ItemFn};

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
/// names produce a compile error.
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
