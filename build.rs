//! Build script for `flow-wm`.
//!
//! This project targets Windows exclusively. The build script panics if the
//! target OS is not Windows, preventing accidental compilation on other
//! platforms. Because `compile_error!` is a macro for the main compilation
//! phase (not build-time), `panic!` in `build.rs` is the correct approach to
//! halt the build early.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        panic!(
            "flow-wm only supports Windows targets. \
             Build with --target x86_64-pc-windows-msvc"
        );
    }
    println!("cargo:rerun-if-changed=build.rs");
}
