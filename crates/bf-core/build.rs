//! Release-only builds.
//!
//! Every crate in the workspace depends on `bf-core`, so refusing the
//! debug profile here refuses it for the whole workspace: `cargo build`,
//! `cargo run`, `cargo test` and `cargo check` without `--release` stop with
//! this message instead of filling `target/debug`.
//!
//! Use `cargo build --release`, the `cargo rb` / `cargo rr` / `cargo rt`
//! aliases, or `scripts/build.ps1` (versioned output in `dist/`).

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var("PROFILE").as_deref() == Ok("debug") {
        println!(
            "cargo::error=ByteFlow builds are release-only. Add --release (or use `cargo rb`, `cargo rr`, `cargo rt`, or scripts/build.ps1)."
        );
    }
}
