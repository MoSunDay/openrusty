//! A phase handler must return `Decision`.
#![allow(dead_code, unused_imports)]

#[openrusty_sdk::phase(rewrite)]
fn on_rewrite() -> i32 {
    0
}

fn main() {}
