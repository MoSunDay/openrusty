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
    /// Encode as the `orr_on_phase` i32 return value.
    pub fn to_abi(self) -> i32 {
        match self {
            Decision::Ok => 0,
            Decision::Declined => -5,
            Decision::Done => -4,
            Decision::Deny(status) => status as i32,
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
        assert_eq!(Decision::Deny(403).to_abi(), 403);
        assert_eq!(Decision::Deny(503).to_abi(), 503);
    }
}
