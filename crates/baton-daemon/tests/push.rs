//! Runs push.rs's unit tests before it is wired into the binary
//! (docs/relay.md, Integration). Once `mod push;` is in main.rs the same
//! tests also run as unit tests; this file can then be deleted.
#[path = "../src/push.rs"]
mod push;
