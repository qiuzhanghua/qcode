# GUI 字体跨平台记录：默认字体与 `.SystemUIFont` 告警

> 状态：待 review。日期：2026-11-05。
> 记录排查结论与后续优化方向，本轮不改代码。
> 当前 Linux 已实测；macOS / Windows 章节待补，新增平台按第 8 节的矩阵往后加。
> 涉及版本：`gpui-pre-linux 0.3.11`、`gpui-component 0.9.0`、qcode 0.7.2。

## 1. 现象

Linux 启动 qcode 时终端出现一行 WARN：

```
WARN failed to load font Font { family: ".SystemUIFont", features: FontFeatures, fallbacks: None, weight: FontWeight(400.0), style: Normal }: requested font family contains no font matching the other parameters: no font found
```

窗口和中文显示都正常，只是每次启动都会打这一条。Windows 上暂未在本机验证；macOS 预期不出现（`.SystemUIFont` 在 Apple 平台映射为 `.AppleSystemUIFont`），**尚未实测**，见第 8 节。

## 2. 机制

### 2.1 `.SystemUIFont` 在 Linux 上映射到 IBM Plex Sans

`.SystemUIFont` 是虚拟族名，GPUI 在 Linux 平台的文本系统初始化时写死了“系统字体”：

```rust
// gpui-pre-linux-0.3.11/src/linux/platform.rs:130
let text_system = Arc::new(crate::linux::CosmicTextSystem::new("IBM Plex Sans"));
```

查字体时 `.SystemUIFont` 会被映射成这个 `system` 参数（`gpui-pre/src/text_system.rs:1267`）：

```rust
".SystemUIFont" => system,
```

所以 **Linux 的默认 UI 字体是 IBM Plex Sans**，而不是 fontconfig 的 `sans-serif`。

### 2.2 没装 IBM Plex Sans 时的回退栈

`gpui-pre-0.3.11/src/text_system.rs:262` 起：

```
.ZedMono → .ZedSans → Helvetica → Segoe UI → Ubuntu → Adwaita Sans
→ Cantarell → Noto Sans → DejaVu Sans → Arial
```

这台机器上实际落到 **Ubuntu**（见下）。

### 2.3 为什么会打 WARN，且只打一次

- 第一次查 `.SystemUIFont` 时，fontdb 找不到 IBM Plex Sans，`gpui-pre` 记一条 `log::warn!`（`text_system.rs:617`），失败结果进缓存
- `gpui-component 0.9` 的 `theme/system_font.rs::resolve_default_font()` 会探测一次“GPUI 实际解析到哪个已安装族名”，然后把它写到 theme 上（`theme/mod.rs:337` 在 theme 变更时调用）
- 之后所有文本都用具体族名（这里是 `Ubuntu`），不再走失败查找，所以只有启动时一条

探测成功时会有一条 INFO：

```
INFO UI font ".SystemUIFont" resolves to "Ubuntu", naming it on the theme.
```

### 2.4 fontconfig alias 不生效（实测）

试过用 `FONTCONFIG_FILE` 把 `IBM Plex Sans` alias 到 `Noto Sans`：

- `fc-match 'IBM Plex Sans'` 确实返回 Noto Sans
- qcode 仍然报同样的 WARN，仍解析到 Ubuntu

cosmic-text 用的是 fontdb（虽然 fontdb 默认 feature 带 `fontconfig`，会读字体目录），但 family 的 prefer alias 不参与它的族名匹配。**不要用 fontconfig alias 来消这条日志。**

## 3. 本机实测

环境：Ubuntu 24.04，`fc-match sans-serif` → Noto Sans，已装 Noto Sans CJK SC / Ubuntu / DejaVu Sans，未装 IBM Plex Sans。

| 场景 | 结果 |
|---|---|
| 现状（无 IBM Plex Sans） | WARN + `INFO ... resolves to "Ubuntu"` |
| `apt-get download fonts-ibm-plex` 后把 ttf 临时放进 `~/.local/share/fonts/` | **WARN 消失**；中文正常渲染（逐字回退到 Noto Sans CJK） |
| fontconfig alias `IBM Plex Sans → Noto Sans` | WARN 仍在，仍解析到 Ubuntu |

临时字体已删除，`fc-match 'IBM Plex Sans'` 回到 Noto Sans。

## 4. 影响评估

- 纯启动日志：一次失败查找 + 一行 WARN，之后命中缓存
- 不影响渲染、中文、性能（每帧重复查找的问题已被 `resolve_default_font` 规避）
- 唯一“用户可见”的副作用是终端里多一行，以及排查别的问题时容易误以为字体坏了

## 5. 现在能做的配置（不改代码）

### 5.1 安装 IBM Plex Sans（唯一能彻底消掉 WARN 的配置）

```bash
sudo apt install fonts-ibm-plex      # multiverse，6.1.1-1
```

装完重启 qcode，`.SystemUIFont` 直接命中 IBM Plex Sans，WARN 不再出现。中文没有 CJK 字形，会走 GPUI 的逐字回退（实际落到 Noto Sans CJK），已确认显示正常；如果观感不满意，`sudo apt remove fonts-ibm-plex` 即可恢复。

### 5.2 忽略

一行启动日志，无功能影响。若不想动系统字体，这是最省事的选项。

### 5.3 目前做不到的

- `gui.json` 只能配 `font_size` / `code_font`（等宽字体），没有 UI 字体字段
- qcode 没有加载自定义 theme JSON
- 没有环境变量能覆盖 `.SystemUIFont` / IBM Plex Sans
- fontconfig alias 无效（见 2.4）

## 6. 后续优化建议（需要改代码，本轮不做）

按收益排序：

1. **启动时显式定 UI 字体**
   在 `Theme` 初始化后直接设置 `theme.font_family`，按语言/已装字体选择，例如：
   - `LANG` 含 `zh` → `Noto Sans CJK SC`
   - 否则 → `Noto Sans` / `Ubuntu`
   - 都没有 → 保留 `.SystemUIFont`
   这样既没有 WARN，也不依赖 IBM Plex Sans 是否安装。

2. **给 `gui.json` 增加 `ui_font` 字段**
   在 `crates/qcode-gui/src/gui_prefs.rs` 里与 `mono_font_family` 一样应用到 `Theme.font_family`，并在 Settings 里暴露一个下拉框（列出 `cx.text_system().all_font_names()`，或只列一组候选）。用户就不需要装系统字体。

3. **候选字体探测**
   用 `fontdb` / GPUI 的已装字体列表，按 `[Noto Sans CJK SC, Noto Sans, Ubuntu, DejaVu Sans]` 选第一个存在的；纯函数，容易写单测。

4. **确认 `resolve_default_font` 的调用时机**
   gpui-component 0.9 已有这套逻辑，但 warn 仍会打一次。可以看上游后续版本是否改成“先探测、后加载”，或 qcode 在初始化时先把字体族名定好，避免第一次失败查找。

5. **文档**
   README 的 Linux 依赖里加一句：可选装 `fonts-ibm-plex`，或用 Noto Sans CJK；说明 `.SystemUIFont` 的映射关系，免得后来人再查一遍。

## 7. 验证命令

看当前解析到哪个字体、是否还有 WARN：

```bash
RUST_LOG=warn,gpui_component=info \
  ./target/release/qc gui --data-dir /tmp/qc-font-probe 2>&1 \
  | grep -E 'failed to load font|UI font'
```

- 有 IBM Plex Sans：没有输出（WARN 消失）
- 没有：一条 WARN + `INFO ... resolves to "Ubuntu"`

看系统字体：

```bash
fc-match 'IBM Plex Sans'
fc-match '.SystemUIFont'
fc-list : family | grep -i 'Plex\|Noto Sans CJK\|Ubuntu'
```

## 8. 平台覆盖与 macOS 待测

### 8.1 当前验证矩阵

| 平台 | 状态 | 备注 |
|---|---|---|
| Linux / X11 | 已验证 | 本文所有结论 |
| Linux / Wayland | 未测 | 字体解析与 X11 应一致，但窗口/字体渲染未实测 |
| Windows | 未在本机验证 | 用户反馈没有该 WARN；映射走 `gpui-pre-windows` 的 DirectWrite |
| macOS | **待测** | `.SystemUIFont` 在 Apple 平台映射为 `.AppleSystemUIFont` |

macOS 的映射来自 `gpui-pre-apple-0.3.10/src/text_system.rs:290`：

```rust
let name = gpui::font_name_with_fallbacks(name, ".AppleSystemUIFont");
```

`.AppleSystemUIFont` 是系统 UI 字体（随系统版本落到 SF Pro / PingFang 等），正常情况下不会像 Linux 的 IBM Plex Sans 那样缺失，所以预期没有这条 WARN。

### 8.2 macOS 上要确认的点

```bash
# 1. 启动日志里是否还有 failed to load font
RUST_LOG=warn,gpui_component=info \
  ./target/release/qc gui --data-dir /tmp/qc-font-mac 2>&1 \
  | grep -E 'failed to load font|UI font'

# 2. 中文是否落到 PingFang
fc-list 2>/dev/null | grep -i 'PingFang'
# 或直接看界面中文观感
```

观察项：

- 是否出现同样的 `failed to load font ... .SystemUIFont`：预期 **没有**
- 如果出现 `INFO UI font ... resolves to ...`，记录解析到的族名（macOS 上可能是 `.AppleSystemUIFont` 或具体 SF 字体）
- 中文是否正常（预期 PingFang SC），与 Windows 版观感对比
- `gui.json` 的 `font_size` / `code_font` 在 macOS 上是否生效（等宽字体尤其要看代码块）
- 如果以后给 `gui.json` 加 `ui_font`，macOS 默认值不要写死 Linux 的候选（IBM Plex / Noto Sans），应按平台给默认

### 8.3 不要做的事（macOS）

- 不需要装 `fonts-ibm-plex`：那是 Linux 的映射产物
- 不要用 `.SystemUIFont` 之外的硬编码字体作为默认，否则 macOS 会失去系统字体随版本更新的好处

## 9. 参考

- `gpui-pre-linux-0.3.11/src/linux/platform.rs:130` — `CosmicTextSystem::new("IBM Plex Sans")`
- `gpui-pre-0.3.11/src/text_system.rs:262` — fallback 栈
- `gpui-pre-0.3.11/src/text_system.rs:1267` — `.SystemUIFont` → system
- `gpui-pre-0.3.11/src/text_system.rs:617` — WARN 出处
- `gpui-component-0.9.0/src/theme/system_font.rs` — `resolve_default_font`
- `gpui-component-0.9.0/src/theme/mod.rs:337` — 调用点
- `qcode/crates/qcode-gui/src/gui_prefs.rs:110-116` — 目前只设置 `mono_font_family`
