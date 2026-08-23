//! Request phases mirroring nginx, and the decisions a plugin may return.
//!
//! ABI mapping (see docs/wasm-abi.md): the guest export `orr_on_phase`
//! returns an `i32` that decodes into [`Decision`] via [`Decision::from_abi`].

/// nginx request processing phases exposed to plugins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum Phase {
    /// Right after the request is read.
    PostRead = 0,
    /// URI rewriting / routing decisions.
    Rewrite = 1,
    /// Access control.
    Access = 2,
    /// Response generation; the default handler proxies here.
    Content = 3,
    /// Peer selection for the proxied request (balancer-by in nginx terms).
    Balancer = 4,
    /// Upstream response headers are available for inspection/modification.
    HeaderFilter = 5,
    /// Upstream response body chunks stream through.
    BodyFilter = 6,
    /// Request finished; accounting.
    Log = 7,
}

impl Phase {
    /// Phases that run before a response is produced.
    pub const PRE_PROXY: [Phase; 4] = [
        Phase::PostRead,
        Phase::Rewrite,
        Phase::Access,
        Phase::Content,
    ];

    pub fn from_i32(v: i32) -> Option<Phase> {
        match v {
            0 => Some(Phase::PostRead),
            1 => Some(Phase::Rewrite),
            2 => Some(Phase::Access),
            3 => Some(Phase::Content),
            4 => Some(Phase::Balancer),
            5 => Some(Phase::HeaderFilter),
            6 => Some(Phase::BodyFilter),
            7 => Some(Phase::Log),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Phase::PostRead => "post_read",
            Phase::Rewrite => "rewrite",
            Phase::Access => "access",
            Phase::Content => "content",
            Phase::Balancer => "balancer",
            Phase::HeaderFilter => "header_filter",
            Phase::BodyFilter => "body_filter",
            Phase::Log => "log",
        }
    }
}

/// Outcome of one plugin phase invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// NGX_OK: the plugin did its work; continue the chain.
    Ok,
    /// NGX_DECLINED: the plugin passed; continue the chain.
    Declined,
    /// NGX_DONE: stop the phase chain. In `content` this short-circuits the
    /// response (v1: empty 204); in `body_filter` it marks the end of stream.
    Done,
    /// Short-circuit with an HTTP error status (nginx style: phases return a
    /// status code to abort).
    Deny(u16),
}

/// ABI codes, aligned with nginx's NGX_* values.
pub mod abi {
    pub const OK: i32 = 0;
    pub const DECLINED: i32 = -5;
    pub const DONE: i32 = -4;
}

impl Decision {
    /// Decode a raw `orr_on_phase` return value.
    pub fn from_abi(code: i32) -> Option<Decision> {
        match code {
            0 => Some(Decision::Ok),
            -5 => Some(Decision::Declined),
            -4 => Some(Decision::Done),
            c if (100..=599).contains(&c) => Some(Decision::Deny(c as u16)),
            _ => None,
        }
    }

    pub fn to_abi(self) -> i32 {
        match self {
            Decision::Ok => abi::OK,
            Decision::Declined => abi::DECLINED,
            Decision::Done => abi::DONE,
            Decision::Deny(status) => status as i32,
        }
    }

    /// True when the phase chain must stop after this decision.
    pub fn is_terminal(self) -> bool {
        matches!(self, Decision::Done | Decision::Deny(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_roundtrip() {
        for d in [
            Decision::Ok,
            Decision::Declined,
            Decision::Done,
            Decision::Deny(403),
            Decision::Deny(503),
        ] {
            assert_eq!(Decision::from_abi(d.to_abi()), Some(d));
        }
    }

    #[test]
    fn rejects_unknown_codes() {
        assert_eq!(Decision::from_abi(-2), None);
        assert_eq!(Decision::from_abi(99), None);
        assert_eq!(Decision::from_abi(600), None);
    }

    #[test]
    fn phase_roundtrip() {
        for p in [
            Phase::PostRead,
            Phase::Rewrite,
            Phase::Access,
            Phase::Content,
            Phase::Balancer,
            Phase::HeaderFilter,
            Phase::BodyFilter,
            Phase::Log,
        ] {
            assert_eq!(Phase::from_i32(p as i32), Some(p));
        }
        assert_eq!(Phase::from_i32(8), None);
    }
}
