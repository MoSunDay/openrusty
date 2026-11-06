use super::*;
use alloc::vec;

#[test]
fn flag_accepts_only_one() {
    assert!(flag("pset=1", "pset"));
    assert!(flag("a=2&pset=1&b=3", "pset"));
    assert!(flag("pset=1&b=3", "pset"));
    assert!(flag("a=2&pset=1", "pset"));
}

#[test]
fn flag_rejects_other_values() {
    assert!(!flag("pset=", "pset"));
    assert!(!flag("pset", "pset"));
    assert!(!flag("pset=0", "pset"));
    assert!(!flag("pset=11", "pset"));
    assert!(!flag("pset=true", "pset"));
    assert!(!flag("other=1", "pset"));
    assert!(!flag("", "pset"));
}

#[test]
fn parse_mode_maps_all_modes() {
    assert_eq!(parse_mode("mode=setgetdel"), Some(ProbeMode::SetGetDel));
    assert_eq!(parse_mode("mode=absent"), Some(ProbeMode::Absent));
    assert_eq!(parse_mode("mode=ttl-set"), Some(ProbeMode::TtlSet));
    assert_eq!(parse_mode("mode=ttl-check"), Some(ProbeMode::TtlCheck));
    assert_eq!(parse_mode("mode=postread"), Some(ProbeMode::PostRead));
    assert_eq!(
        parse_mode("mode=rewritecheck"),
        Some(ProbeMode::RewriteCheck)
    );
    assert_eq!(parse_mode("mode=bodycheck"), Some(ProbeMode::BodyCheck));
    assert_eq!(parse_mode("mode=logcheck"), Some(ProbeMode::LogCheck));
    assert_eq!(parse_mode("mode=scan"), Some(ProbeMode::Scan));
    assert_eq!(parse_mode("mode=wsheaders"), Some(ProbeMode::WsHeaders));
    assert_eq!(parse_mode("mode=balancer"), Some(ProbeMode::Balancer));
    assert_eq!(parse_mode("a=1&mode=absent&b=2"), Some(ProbeMode::Absent));
}

#[test]
fn valid_peer_view_requires_three_healthy_peers() {
    let peer = |name: &str, healthy: bool| host::PeerInfo {
        name: String::from(name),
        addr: String::from("127.0.0.1:19101"),
        healthy,
    };
    let three = vec![peer("n1", true), peer("n2", true), peer("n3", true)];
    assert_eq!(valid_peer_view(&three), Some(0));
    let two = vec![peer("n1", true), peer("n2", true)];
    assert_eq!(valid_peer_view(&two), None);
    let one_down = vec![peer("n1", true), peer("n2", false), peer("n3", true)];
    assert_eq!(valid_peer_view(&one_down), None);
    let four = vec![
        peer("n1", true),
        peer("n2", true),
        peer("n3", true),
        peer("n4", true),
    ];
    assert_eq!(valid_peer_view(&four), None);
}

#[test]
fn parse_mode_rejects_unknown_or_missing() {
    assert_eq!(parse_mode("mode=bogus"), None);
    assert_eq!(parse_mode("mode="), None);
    assert_eq!(parse_mode("mode"), None);
    assert_eq!(parse_mode("other=setgetdel"), None);
    assert_eq!(parse_mode(""), None);
}

#[test]
fn parse_mode_accepts_wsheaders() {
    assert_eq!(parse_mode("mode=wsheaders"), Some(ProbeMode::WsHeaders));
    assert_eq!(
        parse_mode("a=1&mode=wsheaders&b=2"),
        Some(ProbeMode::WsHeaders)
    );
}

#[test]
fn parse_mode_accepts_resp_body() {
    assert_eq!(parse_mode("mode=resp-body"), Some(ProbeMode::RespBody));
    assert_eq!(
        parse_mode("a=1&mode=resp-body&text=t&deny=403"),
        Some(ProbeMode::RespBody)
    );
    // The mode token itself must match exactly.
    assert_eq!(parse_mode("mode=resp_body"), None);
    assert_eq!(parse_mode("mode=respbody"), None);
}

#[test]
fn resp_body_text_pins_wire_format() {
    assert_eq!(resp_body_text("hi"), "kv-probe-resp-body:hi");
    assert_eq!(resp_body_text(""), "kv-probe-resp-body:");
    assert_eq!(
        resp_body_text("with space/slash"),
        "kv-probe-resp-body:with space/slash"
    );
    // Percent-decoding happens in `url::query_param`, before the
    // prefix is attached.
    assert_eq!(
        resp_body_text(&url::query_param("text=a%20b", "text").unwrap()),
        "kv-probe-resp-body:a b"
    );
}

#[test]
fn deny_decision_maps_the_deny_param() {
    // Absent -> terminal Done (200 + body, or empty 204).
    assert_eq!(deny_decision(None), Decision::Done);
    // Valid statuses ride along verbatim.
    assert_eq!(deny_decision(Some("403")), Decision::Deny(403));
    assert_eq!(deny_decision(Some("500")), Decision::Deny(500));
    assert_eq!(deny_decision(Some("100")), Decision::Deny(100));
    assert_eq!(deny_decision(Some("599")), Decision::Deny(599));
    // Malformed or out-of-range values have no wire representation.
    assert_eq!(deny_decision(Some("99")), Decision::Deny(400));
    assert_eq!(deny_decision(Some("600")), Decision::Deny(400));
    assert_eq!(deny_decision(Some("0")), Decision::Deny(400));
    assert_eq!(deny_decision(Some("")), Decision::Deny(400));
    assert_eq!(deny_decision(Some("403x")), Decision::Deny(400));
    assert_eq!(deny_decision(Some("forbidden")), Decision::Deny(400));
}

#[test]
fn resp_body_mode_decisions_on_host_stubs() {
    // The host ffi stub mirrors the success contract, so the full
    // mode handler is testable: text -> Done, text + deny -> Deny(s),
    // no text -> body-less Done.
    assert_eq!(resp_body("mode=resp-body&text=hi"), Decision::Done);
    assert_eq!(
        resp_body("mode=resp-body&text=hi&deny=403"),
        Decision::Deny(403)
    );
    assert_eq!(resp_body("mode=resp-body"), Decision::Done);
    assert_eq!(resp_body("mode=resp-body&deny=403"), Decision::Deny(403));
    // An explicit empty text still writes the prefixed body.
    assert_eq!(
        resp_body("mode=resp-body&text=&deny=500"),
        Decision::Deny(500)
    );
    // Bad deny values deny with 400 even when a body was written.
    assert_eq!(
        resp_body("mode=resp-body&text=hi&deny=nope"),
        Decision::Deny(400)
    );
}

#[test]
fn resp_body_mode_uses_abi_documented_codes() {
    // Done short-circuits as NGX_DONE (-4); Deny(403) crosses the
    // ABI as the raw status (100..=599), per docs/wasm-abi.md.
    assert_eq!(Decision::Done.to_abi(), -4);
    assert_eq!(deny_decision(Some("403")).to_abi(), 403);
}

#[test]
fn body_seen_key_is_derived_per_request() {
    assert_eq!(
        body_seen_key_for("/echo", "bmark=1"),
        "probe:body_seen:/echo?bmark=1"
    );
    assert_eq!(body_seen_key_for("/echo", ""), "probe:body_seen:/echo");
    // Distinct requests (different tags) must never share a key.
    assert_ne!(
        body_seen_key_for("/echo", "bmark=1&btag=a"),
        body_seen_key_for("/echo", "bmark=1&btag=b")
    );
}

#[test]
fn parse_body_seen_accepts_valid_values() {
    assert_eq!(parse_body_seen("0:0"), Some((0, false)));
    assert_eq!(parse_body_seen("123:0"), Some((123, false)));
    assert_eq!(parse_body_seen("1:1"), Some((1, true)));
    assert_eq!(parse_body_seen("4096:1"), Some((4096, true)));
}

#[test]
fn parse_body_seen_rejects_malformed_values() {
    assert_eq!(parse_body_seen(""), None);
    assert_eq!(parse_body_seen("123"), None);
    assert_eq!(parse_body_seen(":1"), None);
    assert_eq!(parse_body_seen("1:"), None);
    assert_eq!(parse_body_seen("x:1"), None);
    assert_eq!(parse_body_seen("1:2"), None);
    assert_eq!(parse_body_seen("-1:1"), None);
    assert_eq!(parse_body_seen("1:1:1"), None);
}

#[test]
fn parse_mode_accepts_log404check() {
    assert_eq!(parse_mode("mode=l404check"), Some(ProbeMode::Log404Check));
    assert_eq!(
        parse_mode("a=1&mode=l404check&b=2"),
        Some(ProbeMode::Log404Check)
    );
    // The mode token itself must match exactly.
    assert_eq!(parse_mode("mode=l404checks"), None);
    assert_eq!(parse_mode("mode=log404check"), None);
}

#[test]
fn new_flags_parse_as_exactly_one() {
    for name in ["rwuri", "rwhdr", "redir", "l404"] {
        assert!(flag(&alloc::format!("{name}=1"), name));
        assert!(!flag(&alloc::format!("{name}=0"), name));
        assert!(!flag(name, name));
    }
}

#[test]
fn req_rewrite_flags_decline_on_host_stubs() {
    // Host-side unit tests stub the imports with success (0), so
    // neither flag -- nor their composition -- derails the chain.
    assert_eq!(apply_req_rewrite(""), Decision::Declined);
    assert_eq!(apply_req_rewrite("rwuri=1"), Decision::Declined);
    assert_eq!(apply_req_rewrite("rwhdr=1"), Decision::Declined);
    assert_eq!(apply_req_rewrite("rwuri=1&rwhdr=1"), Decision::Declined);
}

#[test]
fn redir_flag_answers_302() {
    assert_eq!(redirect_decision("redir=1"), Decision::Deny(302));
    assert_eq!(redirect_decision("a=1&redir=1"), Decision::Deny(302));
    assert_eq!(redirect_decision("redir=0"), Decision::Declined);
    assert_eq!(redirect_decision(""), Decision::Declined);
}

#[test]
fn log404_check_dones_on_host_stubs() {
    // The host stub reports every kv key absent, so the echo value
    // is "-"; the terminal Decision pins the short-circuit.
    assert_eq!(log404_check(), Decision::Done);
}
