//! `baton` — daemon (`baton serve`), TUI and setup. Slice 1 ships the
//! engine only; the HTTP daemon lands in slice 2 (see PLAN.md).

fn main() {
    eprintln!(
        "baton {} — engine only (slice 1). See PLAN.md.",
        env!("CARGO_PKG_VERSION")
    );
}
