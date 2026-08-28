//! Positive case: both accepted spellings of the decision return type.
#![allow(dead_code, unused_imports)]

use openrusty_sdk::Decision;

#[openrusty_sdk::phase(balancer)]
fn pick_peer() -> Decision {
    Decision::Declined
}

#[openrusty_sdk::phase(access)]
fn gate() -> openrusty_sdk::Decision {
    openrusty_sdk::Decision::Ok
}

fn main() {
    let _ = pick_peer();
    let _ = gate();
}
