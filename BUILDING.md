# Maintainer build

Use Windows x64, Rust GNU toolchain and PowerShell. GNU is required for the resource object used by this release.

```powershell
rustup toolchain install stable-x86_64-pc-windows-gnu
rustup override set stable-x86_64-pc-windows-gnu
.uild.ps1 -Test
```

The script compiles the local icon/license resource, runs the tests when requested, and builds the executable in `target/release/iconforge.exe`. It does not create shortcuts, change desktop icons or copy local settings.

`-Cargo` and `-Rustc` can point to an existing GNU toolchain. `-Offline` reuses a populated Cargo dependency cache. Keep `Cargo.lock` committed.

Runtime data is stored in the current Windows user's local application-data directory; none of that data belongs in this repository. Test and restore changes should use isolated application data and temporary shortcuts.
