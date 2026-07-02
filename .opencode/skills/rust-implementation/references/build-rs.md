# build.rs — Windows-only guard

The `build.rs` at the crate root prevents compilation on non-Windows targets.
Because `compile_error!` is a macro for the main compilation phase (it cannot
appear in `build.rs`), the build script uses `panic!` instead, which halts the
build immediately with a clear error message.

```rust
//! Build script for flow-wm.
//!
//! This project targets Windows exclusively. The build script panics if the
//! target OS is not Windows, preventing accidental compilation on other
//! platforms.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        panic!(
            "flow-wm only supports Windows targets. \
             Build with --target x86_64-pc-windows-msvc"
        );
    }
    println!("cargo:rerun-if-changed=build.rs");
}
```

Rules:
- The `build.rs` check is the ONLY platform guard in the project. Do NOT add
  `#[cfg(target_os = "windows")]` anywhere in `src/` or `tests/`.
- `CARGO_CFG_TARGET_OS` is set by Cargo at build time and reflects the actual
  compilation target (respects `--target` flag and `[build] target` in `.cargo/config.toml`).

## Future: Application Manifest (`manifest.xml`)

When DPI awareness or visual styles need to be embedded, add a `manifest.xml`
and use the `embed-resource` or `winres` crate:

```rust
fn main() {
    // Platform gate
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        panic!("flow-wm only supports Windows targets");
    }

    // Embed manifest (when ready)
    // let mut res = winres::WindowsResource::new();
    // res.set_manifest_file("manifest.xml");
    // res.compile().expect("failed to compile Windows resources");

    println!("cargo:rerun-if-changed=build.rs");
}
```
