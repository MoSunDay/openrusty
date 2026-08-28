//! Phase decisions and the `dispatch!` macro generating the `orr_on_phase`
//! export required by docs/wasm-abi.md.

/// Outcome of one phase handler, encoded to the ABI return code via
/// [`Decision::to_abi`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// `0` (NGX_OK): handled, continue the chain.
    Ok,
    /// `-5` (NGX_DECLINED): pass, continue the chain.
    Declined,
    /// `-4` (NGX_DONE): stop the phase chain (`content`: empty 204).
    Done,
    /// `100..=599`: deny the request with this HTTP status.
    Deny(u16),
}

impl Decision {
    /// Reserved invalid return code for out-of-range `Deny` statuses. Mirrors
    /// `openrusty_core::phase::abi::ERROR` (the SDK cannot depend on core);
    /// the host's `Decision::from_abi` rejects it, so the plugin failure
    /// policy applies.
    const BAD_CODE: i32 = -1;

    /// Encode as the `orr_on_phase` i32 return value.
    ///
    /// A `Deny(status)` outside `100..=599` has no wire representation, so it
    /// encodes as [`Decision::BAD_CODE`] instead of the raw status. The host
    /// rejects that code and applies the plugin failure policy rather than
    /// silently decoding `Deny(0)` as `Decision::Ok` (auth bypass) or
    /// `Deny(700)` as a valid HTTP status.
    pub fn to_abi(self) -> i32 {
        match self {
            Decision::Ok => 0,
            Decision::Declined => -5,
            Decision::Done => -4,
            Decision::Deny(status) if (100..=599).contains(&status) => status as i32,
            Decision::Deny(_) => Self::BAD_CODE,
        }
    }
}

/// Generate the `orr_on_phase(phase, ctx) -> i32` export, dispatching to
/// the handler listed for each phase id and returning
/// `Decision::Declined` for unregistered phases.
///
/// Handlers are `fn() -> Decision` items or paths, typically annotated with
/// `#[openrusty_sdk::phase(<name>)]` (that macro keeps the original name
/// available as an alias, so it can be referenced here).
///
/// ```ignore
/// openrusty_sdk::dispatch! {
///     balancer => on_balancer,
///     log => on_log,
/// }
/// ```
#[macro_export]
macro_rules! dispatch {
    (@id post_read) => { 0 };
    (@id rewrite) => { 1 };
    (@id access) => { 2 };
    (@id content) => { 3 };
    (@id balancer) => { 4 };
    (@id header_filter) => { 5 };
    (@id body_filter) => { 6 };
    (@id log) => { 7 };
    (@id $unknown:ident) => {
        compile_error!(concat!(
            "openrusty_sdk::dispatch!: unknown phase `",
            stringify!($unknown),
            "`; expected one of: post_read, rewrite, access, content, \
             balancer, header_filter, body_filter, log"
        ))
    };
    ($($phase:ident => $handler:expr),* $(,)?) => {
        #[no_mangle]
        pub extern "C" fn orr_on_phase(phase: i32, _ctx: i32) -> i32 {
            match phase {
                $(
                    $crate::dispatch!(@id $phase) => ($handler)().to_abi(),
                )*
                _ => $crate::dispatch::Decision::Declined.to_abi(),
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_abi_mapping() {
        assert_eq!(Decision::Ok.to_abi(), 0);
        assert_eq!(Decision::Declined.to_abi(), -5);
        assert_eq!(Decision::Done.to_abi(), -4);
        assert_eq!(Decision::Deny(100).to_abi(), 100);
        assert_eq!(Decision::Deny(403).to_abi(), 403);
        assert_eq!(Decision::Deny(599).to_abi(), 599);
        assert_eq!(Decision::Deny(503).to_abi(), 503);
    }

    #[test]
    fn out_of_range_deny_encodes_bad_code() {
        for status in [0u16, 1, 70, 99, 600, 700, u16::MAX] {
            let code = Decision::Deny(status).to_abi();
            // Never OK, never a decodable HTTP status: the host classifies
            // this as a bad code and applies the plugin failure policy.
            assert_ne!(code, 0, "Deny({status}) encoded as OK");
            assert_eq!(code, Decision::BAD_CODE);
            assert!(
                !(100..=599).contains(&code),
                "Deny({status}) encoded as valid status {code}"
            );
        }
    }
}
