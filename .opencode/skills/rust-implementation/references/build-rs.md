# build.rs — Windows-only guard

Place this file at the crate root (`build.rs`):

```rust
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        panic!("scrolling-tiling-manager only supports Windows targets");
    }
    // Embed a Windows application manifest for DPI awareness and visual styles
    // (requires winres or embed-resource crate, or manual .rc compilation)
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=manifest.xml");
}
```

## Application Manifest (`manifest.xml`)

```xml
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <!-- Windows 10/11 -->
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <asmv3:application xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
    <asmv3:windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/PM</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">
        PerMonitorV2, PerMonitor
      </dpiAwareness>
    </asmv3:windowsSettings>
  </asmv3:application>
</assembly>
```

Embed with `winres` crate in `build.rs`:

```rust
fn main() {
    let mut res = winres::WindowsResource::new();
    res.set_manifest_file("manifest.xml");
    res.compile().expect("failed to compile Windows resources");
}
```
