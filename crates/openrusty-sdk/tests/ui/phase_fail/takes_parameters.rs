//! A phase handler must take no parameters.
#![allow(dead_code, unused_imports)]

use openrusty_sdk::Decision;

#[openrusty_sdk::phase(access)]
fn on_access(_req: u32) -> Decision {
    Decision::Ok
}

fn main() {}
