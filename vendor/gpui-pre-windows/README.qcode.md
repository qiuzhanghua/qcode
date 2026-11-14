# gpui-pre-windows (qcode fork)

Upstream: [`gpui-pre-windows` 0.3.12](https://crates.io/crates/gpui-pre-windows/0.3.12)
(Zed's `gpui_windows` backend, snapshot of `zed@d2a4fb6`), Apache-2.0. Sources
are the crate as published on crates.io; `src/platform.rs` and `build.rs` are
modified (see "Patches" below).
This version pairs with `gpui-kit 0.9.1` (`gpui-base 0.9.1`, `gpui-pre =0.3.12`).

## Why it is vendored

The Windows VSync thread iterates every tracked HWND every frame and calls
`RedrawWindow` on it. A window's HWND is destroyed by GPUI's own teardown path
(`WindowsWindow::drop` → `DestroyWindow`), but its entry in the platform's raw
window-handle list is only pruned later, once the `WM_DESTROY`-driven
`WM_GPUI_CLOSE_ONE_WINDOW` message is processed. Between those two points the
VSync thread can hit a destroyed handle and log a spurious error to the
console on every window close:

```
ERROR 无效的窗口句柄。 (0x80070578)   // ERROR_INVALID_WINDOW_HANDLE
```

That shows up in qcode when the settings window is closed (qcode-gui defers
the window removal past the deactivation callback, see
`crates/qcode-gui/src/setup/mod.rs`).

The fork guards the redraw with `IsWindow` so a destroyed window is skipped
instead of failing:

```rust
// src/platform.rs, begin_vsync_thread
if !IsWindow(Some(hwnd.as_raw())).as_bool() {
    continue;
}
```

## Patches

Two, both small and both explained at the site:

1. `src/platform.rs` — the `IsWindow` guard in `begin_vsync_thread` described
   above.
2. `build.rs` — the HLSL gate. Upstream writes

   ```rust
   #[cfg(target_os = "windows")]
   { #[cfg(not(debug_assertions))] compile_shaders(); }
   ```

   A build script is compiled for and runs on the **host**, so both cfgs
   describe the host, not the target. Cross-compiling a release Windows build
   from macOS/Linux therefore compiled no shaders at all, while
   `src/directx_renderer.rs`'s `include!(concat!(env!("OUT_DIR"),
   "/shaders_bytes.rs"))` is live under `not(debug_assertions)` — the file was
   missing and the build died with a message that never mentions `fxc.exe`:

   ```
   error: couldn't find file `.../out/shaders_bytes.rs`
   ```

   The patch reads `CARGO_CFG_TARGET_OS` / `CARGO_CFG_DEBUG_ASSERTIONS` (cargo
   sets these for the *target*; the boolean ones are present-and-empty when
   true) and, when the shader compiler is unavailable, prints a
   `cargo::error=` explaining that release Windows builds need `fxc.exe` from
   the Windows SDK, that `GPUI_FXC_PATH` overrides it, and that debug builds
   compile shaders at runtime via `D3DCompile` and need nothing.

   Binaries still cannot be produced on a non-Windows host: `fxc.exe` has no
   native macOS/Linux build and nothing open-source emits SM 4.1 DXBC (`dxc`
   only emits SM 6+ DXIL). `build-for-release` skips those targets with that
   reason instead of failing mid-build; CI builds them on `windows-latest`.

## Updating

When gpui-kit / the GPUI pre-release crates are upgraded, re-sync this fork:

1. Replace the tree with the new `gpui-pre-windows` sources from the cargo
   registry (or `https://static.crates.io/crates/gpui-pre-windows/…crate`),
   keeping `README.qcode.md`.
2. Re-apply both patches above (`IsWindow` guard, `build.rs` HLSL gate) and
   update the version/zed-rev facts in this file.
3. Refresh the lockfile: `cargo update -p gpui-pre-windows`.

Step 3 is easy to forget and fails silently: the vendored package version has
to match the `=0.3.12` requirement of `gpui-pre`, otherwise cargo prints only

```
warning: patch `gpui-pre-windows v0.3.10 (…/vendor/gpui-pre-windows)` was not used in the crate graph
```

and builds the registry copy instead — green build, no guard. Verify with
`cargo tree -p qcode-gui -i gpui-pre-windows` (the entry must carry the
`(/…/vendor/gpui-pre-windows)` suffix) and check that the `Cargo.lock` entry
has no `source`/`checksum` line.
