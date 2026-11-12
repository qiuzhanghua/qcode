# gpui-pre-windows (qcode fork)

Upstream: [`gpui-pre-windows` 0.3.12](https://crates.io/crates/gpui-pre-windows/0.3.12)
(Zed's `gpui_windows` backend, snapshot of `zed@d2a4fb6`), Apache-2.0. Sources
are the crate as published on crates.io; only `src/platform.rs` is modified.
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

## Updating

When gpui-kit / the GPUI pre-release crates are upgraded, re-sync this fork:

1. Replace the tree with the new `gpui-pre-windows` sources from the cargo
   registry (or `https://static.crates.io/crates/gpui-pre-windows/…crate`),
   keeping `README.qcode.md`.
2. Re-apply the `IsWindow` guard above and update the version/zed-rev facts in
   this file.
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
