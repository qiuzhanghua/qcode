# xim-ctext (qcode fork)

Upstream: [`xim-ctext` 0.3.0](https://crates.io/crates/xim-ctext/0.3.0), MIT,
sources from <https://github.com/Riey/xim-rs> (crate checksum
`0.3.0` as published; only `Cargo.toml` and `src/lib.rs` are local).

## Why it is vendored

`zed-xim` (pulled in by the GPUI Linux backend) decodes every XIM preedit and
commit string with `xim_ctext::compound_text_to_utf8(..).expect("Encoding Error")`.
Upstream's decoder only understands UTF-8 (`ESC % G`) and ISO-2022-JP, so any
other COMPOUND_TEXT charset designator returns `UnsupportedEncoding` and takes
the process down with a panic inside the event loop:

```
thread 'main' panicked at zed-xim-0.4.0-zed/src/client.rs:237:67:
Encoding Error: UnsupportedEncoding
```

That is not an exotic input. With `LANG=zh_CN.UTF-8`, Xlib's UTF-8 →
COMPOUND_TEXT conversion (what IBus/fcitx XIM servers hand to clients) emits
`ESC $ ( A` + 7-bit GB 2312 for Chinese, and raw ISO-8859-1 bytes for
`café` — so simply showing a Chinese preedit crashed the window.

The fork below keeps decoding (instead of dropping the text, which would make
Chinese input impossible) and makes it total:

| designator | charset | decoded as |
| --- | --- | --- |
| `ESC % G` … (`ESC % @`) | UTF-8 | UTF-8 |
| `ESC $ ( A`, `ESC $ ) A` | GB 2312-80 | GBK (superset) |
| `ESC $ ( B` | JIS X 0208 | EUC-JP |
| `ESC $ ( C` | KS C 5601 | EUC-KR |
| `ESC - A` … `ESC - F` | ISO-8859-1…6 | windows-1252 / ISO-8859-2…6 |
| anything else | — | lossy UTF-8 → windows-1252 |

`compound_text_to_utf8` keeps its `Result` signature so the API still matches
`zed-xim`, but it never returns `Err`; malformed bytes become U+FFFD.

## Tests

The crate is a workspace member, so `cargo test --workspace` runs its decoder
tests (they replay the byte strings Xlib emits for `你好`, `你好, 世界!`,
`café`, `你好😀`, …).

## Updating

If upstream ever fixes this, delete the directory, drop the `[patch.crates-io]`
entry and the `vendor/xim-ctext` workspace member, then re-run
`cargo test --workspace`.
