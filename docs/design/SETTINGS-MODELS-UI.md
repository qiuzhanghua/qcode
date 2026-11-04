# qcode 设置页「模型」section UI 设计（V1）

> 状态：设计文档，待评审
> 范围：设置窗口「模型」section 的**界面结构、组件、交互与视觉细则**
> 配套：[SETTINGS-MODELS-V2.md](./SETTINGS-MODELS-V2.md)（模型接入架构 M0–M4，数据层已落地）、
> [SETTINGS-UI-V2.md](./SETTINGS-UI-V2.md)（设置窗口整体布局：左导航 + 分区）

---

## 1. 背景与目标

Models 设置 section 的目标形态是「提供方行卡片 + 行内展开编辑器 + 模型目录编辑 + 端点探测候选选择器 + 自定义提供方声明 + 删除确认」的组合。qcode 现状（`Page::Models`）是**表格行编辑**：只读 Provider 目录文本列表 + 默认模型三字段 + providers 表（内嵌 `__key` 密文、预设按钮、探测按钮文本输出）。

本设计在 qcode 的 GPUI 设置窗口落地上述 Models section UI，目标：

1. Provider 行从表格行升级为**卡片行**：显示名 + 自定义标签 + 凭证状态圆点 + 编辑/删除。
2. 行内展开**编辑器卡片**：API 密钥（只写）+ 折叠「自定义设置」（API 地址 / 模型目录 / 自定义路由的显示名与协议）。
3. 端点探测从**文本输出**升级为**候选选择器 Modal**（搜索 + 全选/取消全选 + 添加所选）。
4. 新增**自定义提供方声明卡**与**删除确认 Modal**。
5. 状态反馈：已保存提示、凭证圆点、模型目录「继承/自定义」徽标。

数据层沿用已落地的 M0–M4（`ProviderRegistry::list_configurable_providers`、`CredentialState`、`ProviderCatalog`、`ConfigEdit` 叶子 diff），UI 只做呈现与交互升级。

---

## 2. Models section UI 解剖（参考基准）

### 2.1 页面骨架

```
h2 标题「模型」 + intro「填入各提供方的 API 密钥即可使用其模型。」
[只读提示]（可选，readOnly 部署）
[已保存 {provider}。]（role=status，aria-live=polite）
<ul> 提供方行卡片列表（一次只展开一张编辑器卡）
<添加区> 两个等宽按钮：添加提供方 / 添加自定义提供方
[删除确认 Modal]
```

- section 最大宽度 720px；标题 16px/500，intro 14px 三级文字色。
- 页面完全由「可配置 Provider 目录」驱动（route + displayName + 是否已配置 + 凭证状态）。
- 已保存提示在 apply 落地、目录刷新后按**最新显示名**命名（apply 可改路由名）。

### 2.2 Provider 行卡片

- 圆角 16、0.5px 描边、内边距 12×14，纵向 gap 12。
- 行头：**显示名** + 「自定义」小标签（仅适配器报告的 hand-declared 路由）+ **凭证状态圆点**（绿实心 = 已配置，红实心 = 缺失；无引用/无需 key 不显示）+ 右侧**编辑/删除**小胶囊（28px 高、12px 字）。
- 每行可展开一张编辑器卡；`editing` / `adding` / `declaring` 三态互斥，**关掉一张不丢弃另一张的草稿**。
- 首次运行姿态：整 section 尚无任何可用 provider 时，缺密钥的 provider **直接渲染为其 setup 编辑卡**（而非行），直到用户关闭该卡；之后回退为普通行（补 key 走「编辑」）。

### 2.3 行内编辑器卡片（ProviderEditor）

- 填充色模块（圆角 12，`bg-module-platform`），内边距 14×16。
- 主字段：**单个 API 密钥密码框**（只写，页面从不问环境变量名）。placeholder 分态：
  - 已配置：「已配置——输入新值可替换」
  - 原生认证（pi-ai 留空）：「输入 API 密钥，或留空使用环境认证」
  - 环境锁定：「由启动环境提供（只读）」
- 折叠「**自定义设置**」（chevron 展开，原生 details 标记隐藏）：API 地址、模型目录；仅自定义路由显示「显示名称 + API 协议」。
- 底部动作行：**取消（左）/ 保存（右）**，保存中标签切「保存中…」。
- 提交 = settings 叶子路径 op + 密钥单独只写存储；冲突（revision 过期）明确报错。
- **刻意没有"推理强度"**：它是按模型的能力，同一 provider 下模型间不一致，provider 级控件只能设成某些模型会拒绝的值；放在 composer 的模型选择里。

### 2.4 模型目录编辑器（ModelListEditor / DeepSeekModelsEditor）

- 表头：标题 + 「正在使用适配器默认模型 / 已自定义模型目录」徽标 + 「恢复默认模型」+ 「获取可用模型」。
- 每行：**模型 ID** + **显示名称** + chevron（展开容量）+ 删除；容量（上下文窗口 / 最大输出 token）在行内折叠区，K/M 后缀输入（`256K`、`1M`）。
- **获取可用模型**用表单当前值（含未保存的密钥）问端点 → 候选 **Modal**：
  - 搜索框（匹配 id 与名称，不丢隐藏项勾选）+ 全选/取消全选（**只作用于可见项**）。
  - 已配置模型默认不勾选（避免覆盖用户改过的容量）；「添加所选」才写入。
  - 探测失败不阻塞：失败信息显示在行旁，仍可手填。
- 空态：「模型选择器中将不显示任何模型；目录外 ID 仍可直接发送。」
- 列表为数组**按值整体替换**：先显示继承的有效行，第一次编辑物化用户层覆盖，「恢复默认模型」取消覆盖。

### 2.5 添加 / 自定义提供方流程

- **添加提供方**：按钮 → 卡片：顶部 dormant provider `<select>`（已装 catalog 全部可加路由）+ 编辑器（隐藏标题）。
- **添加自定义提供方**：按钮 → 创建卡（独立于编辑器，因为这是 create 而非 edit）：
  - Provider ID（规则：小写字母开头 + 小写/数字/短横线；占用校验；字段级 hint + 错误）
  - 显示名称（留空回退 route）、API 地址、API 协议（select）、API 密钥（可留空 = 环境鉴权）、模型目录（**至少一个模型**，create 卡要求，因为无法兜底）
  - 动作行：取消 / **创建提供方**；写冲突（revision）明确报错。

### 2.6 删除确认

- Modal 标题「删除 {provider}？」；正文**区分两种情况**：
  - 无页面管理的密钥：「删除 {provider} 会移除其配置；其使用的凭证（如有）由其他位置管理，将会保留。」
  - 有页面管理的密钥：「删除 {provider} 会移除其配置和存储的 API 密钥。」
- 动作：取消 / 删除（danger 边框）；删除中禁用；失败显示在 Modal 内。

### 2.7 视觉与无障碍约定

- 颜色全走 alias token（`--dsw-alias-*`），无裸色值；0.5px 描边；圆角体系 16/12/10/8/6；按钮胶囊 18px（行内控件 14px 胶囊）。
- 焦点 = 2px 描边圆角；图标按钮必须有 `aria-label`；状态消息 `role=status`。
- 空态虚线框（"nothing here yet" 用虚线而非实线）。

---

## 3. qcode 现状与差距

### 3.1 现状（`crates/qcode-gui/src/setup` + `crates/qcode-kernel`）

- `Page::Models`：h2+intro（`render.rs` 顶部）→ 只读目录文本列表 → 默认模型三字段（`default_provider` + `[default-model].provider/.model`，`forms.rs` FIELDS）→ providers 表（`tables.rs`，含 `__key` 密文列、`add_provider_preset` 目录预设按钮、`probe_models` 文本输出）。
- 数据层已具备：`ProviderRegistry::list_configurable_providers`（目录行含 route/display_name/protocol/configured/credential_state）、`CredentialState::{Configured,Missing,Keyless}`、`ProviderCatalog`（`CATALOG` 静态表 + `models`）、`ConfigEdit` 叶子 diff、`qcode_providers::discover_models`（探测已存在）。
- 保存：`auto_save`（变更即写盘）+ 表单提交并存。

### 3.2 差距

| 维度 | 参考 | qcode 现状 |
|---|---|---|
| 提供方呈现 | 卡片行 + 凭证圆点 + 自定义标签 | 只读文本列表 + 表格行 |
| 凭证状态 | 行上绿/红实心圆点 | 目录文本「凭证: 已存/缺失」 |
| 编辑形态 | 行内展开编辑器卡（密钥 + 折叠自定义设置 + 模型目录） | 表格行 + 内嵌 `__key` 列 |
| 模型目录 | 每行 id/name/容量折叠 + 继承徽标 + 恢复默认 | 无（`ProviderConfig` 无 `models` 字段，探测仅文本输出） |
| 探测结果 | 候选选择器 Modal（搜索/全选/添加所选） | `probe_result` 文本（「探测中…/探测失败」） |
| 自定义路由 | 独立创建卡（id/名/端点/协议/密钥/模型） | 无表单入口（只能手写 config.toml 或改表格行） |
| 删除 | 确认 Modal，区分密钥归属 | 表格行可删，无确认 |
| 反馈 | 已保存 status 消息 | 无 |

### 3.3 关键数据缺口

`ProviderConfig` 目前**没有 `models` 字段**（只有 `CatalogEntry.models` 静态表）。模型目录编辑器要可写，需先在配置层补 `providers.<route>.models: Vec<ModelEntry>`（id + name + contextWindow + maxTokens，全部可选除 id），并接入 resolve 的模型清单联合逻辑（SETTINGS-MODELS-V2 §4.3 已规划手工路由 `models`）。这是本 UI 设计的前置数据项，列入落地阶段。

---

## 4. 目标 UI 设计（qcode 落地）

### 4.1 总体布局线框

```
设置窗口 · Models section（内容区，左侧导航选中「模型」）
┌─────────────────────────────────────────────────────────────┐
│  h2「模型」  intro「填入各提供方的 API 密钥即可使用其模型。」      │
│                                                             │
│  默认模型（2 行 PreferenceRow，行式：左 label+help 右控件）      │
│   · 默认 Provider   [select: deepseek ▾]                     │
│   · 默认模型        [select: deepseek-flash ▾]               │
│  ──────────────────────────────────────────────（分割线）      │
│                                                             │
│  [已保存 deepseek。]                          ← status 消息   │
│                                                             │
│  ┌─ 行卡片: deepseek ─────────────────────────────────────┐ │
│  │  DeepSeek  ● 已配置              [编辑]  [删除]         │ │
│  │  （展开 → 行内编辑器卡，见 4.2-B）                       │ │
│  └───────────────────────────────────────────────────────┘ │
│  ┌─ 行卡片: anthropic ────────────────────────────────────┐ │
│  │  Anthropic  ○ 缺失               [编辑]  [删除]         │ │
│  └───────────────────────────────────────────────────────┘ │
│  ┌─ 行卡片: ollama ───────────────────────────────────────┐ │
│  │  Ollama                       [编辑]  [删除]（无圆点）   │ │
│  └───────────────────────────────────────────────────────┘ │
│  ┌─ 添加区 ───────────────────────────────────────────────┐ │
│  │  [＋ 添加提供方]         [＋ 添加自定义提供方]            │ │
│  └───────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘

A) 添加提供方（展开后）：
   ┌─ 添加卡 ─────────────────────────────────────────────┐
   │  提供方  [select: dormant 目录路由 ▾]                 │
   │   ┌─ 编辑器（隐藏标题）────────────────────────────┐  │
   │   │  API 密钥  [password ................]         │  │
   │   │  ▸ 自定义设置                                  │  │
   │   │  [取消]                        [保存]          │  │
   │   └──────────────────────────────────────────────┘  │
   └────────────────────────────────────────────────────┘

B) 行内编辑器（行卡片展开后）：
   ┌─ 编辑器卡 ──────────────────────────────────────────┐
   │  DeepSeek (deepseek)                                 │
   │  API 密钥  [password: 已配置——输入新值可替换 .....]    │
   │  ▸ 自定义设置                                        │
   │     API 地址   [https://api.deepseek.com/v1 ....]    │
   │     ── 模型目录 ────────────────────────────────     │
   │     模型目录  [继承/已自定义]  [恢复默认] [获取可用模型] │
   │     [deepseek-flash      │ 显示名称 │ ▸ │ 🗑]        │
   │     [+ 添加模型]                                     │
   │  [取消]                              [保存]          │
   └─────────────────────────────────────────────────────┘

C) 获取可用模型 → 候选 Modal：
   ┌─ Modal「选择要添加的模型」────────────────────────────┐
   │  搜索模型 [................]  [取消全选]              │
   │  ☑ MiniMax-M2.7-highspeed                           │
   │  ☑ MiniMax-M3                                       │
   │  [取消]                    [添加所选]                 │
   └─────────────────────────────────────────────────────┘

D) 自定义提供方卡：
   ┌─ 自定义提供方 ───────────────────────────────────────┐
   │  Provider ID   [acme-gateway ............]           │
   │     （hint：小写字母开头……）                          │
   │  显示名称      [Acme Gateway ............]           │
   │  API 地址      [https://gateway.example/v1 ..]       │
   │  API 协议      [openai-completions ▾]                │
   │  API 密钥      [password: 输入 API 密钥 .........]    │
   │  ── 模型目录（≥1 模型，create 要求）────────────       │
   │  [取消]                    [创建提供方]               │
   └─────────────────────────────────────────────────────┘

E) 删除确认 Modal：
   ┌─ Modal「删除 anthropic？」───────────────────────────┐
   │  删除 anthropic 会移除其配置和存储的 API 密钥。        │
   │  [取消]                    [删除 anthropic]（danger）  │
   └─────────────────────────────────────────────────────┘
```

### 4.2 组件拆解与映射

| 参考组件/概念 | qcode 落地（GPUI） |
|---|---|
| `ModelsSection`（section 骨架） | `SetupView` Models 页渲染（`setup/render.rs`），h2+intro 已有 |
| Provider 行卡片（`li.rowCard` + 圆点 + 编辑/删除） | 新增 `ProviderCard` 渲染函数/组件；行列表替代现有目录文本列表 + providers 表 |
| 凭证圆点（绿/红 8px） | 复用 `CredentialState`：`Configured`→绿、`Missing`→红、`Keyless`→不显示 |
| `ProviderEditor`（密钥 + 折叠自定义设置 + 动作行） | 行内展开编辑区：复用现有输入控件，新增折叠区（GPUI 折叠组件） |
| `ModelListEditor` / `DeepSeekModelsEditor` | 新增模型目录编辑器（id/name/chevron/trash + 容量折叠 + 继承徽标 + 恢复默认） |
| `EditorFooter`（取消/保存） | 复用现有动作行按钮样式（`primaryButton`/`secondaryButton` 已有等价） |
| 候选选择器 Modal（`fetchModels`） | `qcode_providers::discover_models` 已有 → 输出改为候选 Modal；删除现有 `probe_result` 文本 |
| `CustomProviderCard` | 新增声明卡（route/base_url/protocol/key/模型），写 `providers.<route>` 全 profile |
| 删除确认 Modal | 新增；正文按是否有页面管理密钥区分 |
| 已保存 status 消息 | 新增 `saved_notice`（保存成功后置，下次操作清除） |
| provider-card / footer 扩展插槽（slot） | **不做**（qcode 无插件系统） |

### 4.3 交互与状态规则

1. **一卡互斥**：`editing: Option<String>`（行编辑）、`adding: bool`（添加卡）、`declaring: bool`（声明卡）三者互斥——打开任一卡时清掉其余，避免草稿互相丢弃；setup 卡与三卡独立（它没有草稿）。
2. **每行独立折叠**：模型目录的行级「容量」展开用 `HashSet<usize>`（**行索引**），删除行后重索引——沿用项目 memory 教训：不能用全局单一布尔（多行互相干扰）。
3. **凭证圆点只表示已确认事实**：绿 = 该路由引用（`api_key_env` 或派生）确实有值/Keychain 已存；红 = 具名引用缺失；Keyless 不显示。探测环境变量时不清空（目录只读事实，见 `credential_state` 现有语义）。
4. **模型目录继承语义**：无用户 `models` 时显示目录静态模型 + 「正在使用适配器默认模型」徽标；第一次编辑物化用户层覆盖 → 「已自定义模型目录」+「恢复默认模型」按钮（取消覆盖）。
5. **探测用表单当前值**：含未保存的 API 地址与密钥；失败不阻塞手填；候选 Modal 搜索不清勾选、全选只作用于可见项；「添加所选」才写入。
6. **保存语义（本设计的关键决策）**：Provider 卡片采用 **保存/取消** 卡片语义（密钥与模型清单是较大编辑单元，含校验与凭证写入，不适合逐字符即时写盘）；默认模型两行 **即时保存**（叶子 diff）。→ 对 SETTINGS-UI-V2 §4.5 的修订：Models section 不再整体"即时自动保存"，改为"默认模型行即时 + Provider 卡片显式保存"。
7. **删除语义**：仅当该路由配置确实引用页面管理的密钥（`api_key_env` 指向本页派生引用或 Keychain 已存且可写）时，删除才连带移除密钥；否则保留。
8. **路由名不可在编辑卡改名**：Provider ID 是 settings 键与凭证引用词干，改名 = 删除 + 新建。

### 4.4 视觉细则（GPUI）

- **颜色**：全部经 qcode 现有主题 token（alias token 原则，不写裸色值）。绿圆点 = success 色，红圆点 = error 色。
- **圆角体系**：行卡片 16、编辑器卡 12、模型行 10、输入框 8、图标按钮 6、胶囊按钮 18（行内 14）。与现有设置窗口控件一致。
- **描边**：0.5px；空态/添加区用**虚线**（"nothing here yet" 惯例）。
- **字号**：标题 16、行名 14、行内标签/徽标/提示 12、输入框 14——全部走项目全局字号轴（Ctrl+=/- 缩放生效），禁止绝对 px 硬编码（项目 memory 约束）。
- **弹窗**：删除确认与候选选择器复用设置窗口既有 Modal 风格（项目弹窗圆角惯例：上两角圆角、下两角直角——与三处提示框一致；若设置窗口已有独立 Modal 风格，随现有惯例）。
- **焦点/无障碍**：焦点 2px 描边；图标按钮（chevron、trash）必须有 aria-label/标题；已保存消息 `role=status` 播报。

### 4.5 文案（中文，qcode 现有用词）

| 场景 | 文案 |
|---|---|
| intro | 填入各提供方的 API 密钥即可使用其模型。 |
| 已保存 | 已保存 {provider}。 |
| 密钥 placeholder（已配置） | 已配置——输入新值可替换 |
| 密钥 placeholder（默认） | 输入 API 密钥，或留空使用环境认证 |
| 折叠标题 | 自定义设置 |
| API 地址 | API 地址 |
| 模型目录徽标 | 正在使用适配器默认模型 / 已自定义模型目录 |
| 恢复默认 | 恢复默认模型 |
| 获取可用模型 | 获取可用模型（探测中…） |
| 候选 Modal | 选择要添加的模型 / 搜索模型 / 全选 / 取消全选 / 添加所选 / 没有匹配的模型 |
| 模型空态 | 模型选择器中将不显示任何模型；目录外 ID 仍可直接发送。 |
| 容量 | 上下文窗口 / 最大输出 token（占位 256K / 32K） |
| 自定义卡 | 自定义提供方 / Provider ID / 显示名称 / API 协议 / 创建提供方 |
| 删除 | 删除 {provider}？（正文按凭证归属两版） |
| 添加 | 添加提供方 / 添加自定义提供方 |
| 动作 | 取消 / 保存 / 保存中… |
| 冲突 | 这张卡片打开期间，这些设置已被其他地方改动。请关闭后重新打开，在当前值上编辑。 |

### 4.6 数据层与写入语义

| 数据 | 来源/落点 | 变更 |
|---|---|---|
| 目录行（route/display/protocol/configured/credential） | `ProviderRegistry::list_configurable_providers(&config)` | 复用，不变 |
| 默认模型（provider/model） | `[default-model]` + `default_provider` 回退链（M1 已落地） | 复用，不变 |
| 路由 profile | `providers.<route>`：base_url / api_key_env / api_key_command / protocol / stream_usage / headers / extra_body | 复用；**新增 `models` 字段**（前置数据缺口，见 3.3） |
| 密钥 | 现有 `api_key_env` / Keychain「Store key」/ `api_key_command` | 保留，UI 收敛为行内密码框（替代表格 `__key` 列） |
| 写路径 | `ConfigEdit` 叶子 diff（toml_edit 保留注释 + 原子写） | 复用；卡片保存 = 一次叶子写 + 一次凭证写 |
| 探测 | `qcode_providers::discover_models(base_url, key, headers)` | 复用，仅 UI 升级为候选 Modal |

写入顺序：先 settings 叶子写（携带 revision，冲突即报错），成功后再写凭证；凭证失败只重试凭证阶段，不重跑 settings 写。

### 4.7 落地步骤（阶段拆分）

| 阶段 | 内容 | 验证 |
|---|---|---|
| **U0 数据缺口补齐** | `ProviderConfig.models`（`Vec<ModelEntry>`：id/name/contextWindow/maxTokens）+ resolve 模型清单联合（目录 ∪ 用户）+ `known_keys` 登记 | config 测试绿；旧文件兼容 |
| **U1 行卡片化** | 目录文本列表 + providers 表 → 行卡片（显示名/自定义标签/凭证圆点/编辑/删除）；行内展开编辑器卡（密钥 + 折叠自定义设置 + 动作行） | 手工回归；凭证圆点随 env 变化正确 |
| **U2 模型目录编辑器** | 模型行（id/name/chevron/trash + 容量折叠 + K/M 输入 + 校验）、继承徽标、恢复默认、添加模型 | 校验（空 id/重复/容量非法）在写入前拒绝 |
| **U3 探测候选 Modal** | `probe_result` 文本 → 候选选择器 Modal（搜索/全选可见项/添加所选）；删除旧文本输出 | 探测一次完成添加；失败可手填 |
| **U4 自定义提供方卡** | 声明卡（id 规则/占用校验/端点/协议/密钥/≥1 模型）+ 创建写全 profile | 冲突（revision）报错；新路由出现在目录与下拉 |
| **U5 删除确认 + 反馈** | 删除确认 Modal（按凭证归属两版正文）+ 已保存 status 消息 | 删除区分密钥保留/清除；保存后状态播报 |
| **U6 打磨** | 空态虚线、焦点、reduced-motion、与左导航布局（SETTINGS-UI-V2）联调 | schema 测试绿；手工回归全部 section |

---

## 5. 明确不做 / 取舍

- **不引入插件扩展插槽**：qcode 无插件系统，不实现 provider-card/footer 插槽。
- **不做推理强度（effort）控件**：按模型能力放在 composer 模型选择里，Provider 卡不设。
- **不做 headers/extra_body 表单编辑**（本版）：仍留 config.toml 手写（SETTINGS-MODELS-V2 M4 的可选项，本设计不含）。
- **不改 Provider ID**：路由名 = settings 键 + 凭证引用词干，改名走删除+新建。
- **不做按目录全量探测**：探测仅对已填 base_url 的路由开放，且只支持 OpenAI 兼容 listing（Anthropic 报告不可探测，回退手填）。
- **默认模型不搬进 composer 选择器**（本版）：保留设置页默认模型行；composer 选择器升级属另一设计。
- **模型目录仍为纯静态或手填 + 探测采纳**：探测结果不自动落盘，配置文件是唯一事实源。

## 6. 关键决策摘要

1. Provider 从表格行升级为 **卡片行 + 凭证圆点 + 自定义标签**，目录（`list_configurable_providers`）驱动渲染。
2. 行内展开**编辑器卡片**：API 密钥（只写）+ 折叠「自定义设置」（API 地址/模型目录/自定义路由显示名+协议）+ 取消/保存动作行。
3. 探测从文本输出升级为**候选选择器 Modal**（搜索/全选可见项/添加所选），失败不阻塞手填。
4. **保存语义修订 SETTINGS-UI-V2**：Models section 拆两层——默认模型行即时保存（叶子 diff），Provider 卡片显式保存/取消（大编辑单元 + 校验 + 凭证写入）。
5. 新增**自定义提供方声明卡**与**删除确认 Modal**（删除按凭证归属区分保留/清除）。
6. 模型目录编辑器需要**前置数据缺口**：`ProviderConfig.models` 字段（U0），随后才有继承/自定义徽标与恢复默认。
7. 视觉全部走主题 token + 全局字号轴；弹窗复用设置窗口既有 Modal 风格；无障碍（aria-label / role=status）符合规范。

---

## 7. 实施记录（2026-11-03，U0–U5 已落地）

> 实现落地于 `crates/qcode-gui/src/setup/models.rs`（新增）+ `render.rs`/`tables.rs`/`save.rs`/`mod.rs` + `crates/qcode-kernel`（`config/mod.rs`、`config_edit.rs`、`config/catalog.rs`）。设置模式 Models 页按本设计渲染；向导模式保留原表格表单。`cargo check --workspace`、kernel/gui 单测与 `tests/schema.rs` 全绿。

| 阶段 | 落地状态 |
|---|---|
| U0 | ✅ `ProviderConfig.models: Vec<ModelEntry>`（id/name/context_window/max_tokens）+ `ConfigEdit::apply_config` 写 `[[providers.<route>.models]]` + `catalog_models(route)` 静态目录模型；旧文件 `serde(default)` 兼容；round-trip 测试覆盖 |
| U1 | ✅ 行卡片（显示名/协议/凭证圆点/编辑·删除·添加）+ 行内编辑器（Provider ID 只读 + 各字段 + 密钥钥匙串 + 动作行）；目录由 `list_configurable_providers` 驱动 |
| U2 | ✅ 模型目录编辑器：继承/已自定义徽标 + 恢复默认模型 + 添加模型 + 每行 id/name/上下文窗口/最大输出（容量为内联文本输入，**未做折叠**）；空 id 行落盘时丢弃 |
| U3 | ✅ 获取可用模型 → 候选选择器 Modal（搜索/全选·取消全选可见项/勾选/添加所选）；探测失败回退手填提示 |
| U4 | ✅ 自定义提供方卡：Provider ID（规则+占用校验）/显示名称/API 地址/Key env/协议/模型行 + 创建写全 profile 并展开编辑器 |
| U5 | ✅ 删除确认 Modal（注明钥匙串密钥不会被移除）+ 已保存 status 消息（成功保存/创建后显示） |
| U6 | ⏳ 未做：空态虚线框、焦点/aria 细化、reduced-motion、与 SETTINGS-UI-V2 左导航布局联调 |

### 与设计的两处偏差（实施取舍）

1. **保存语义**：沿用既有 auto-save（变更即写盘，SETTINGS-UI-V2 的即时保存），「保存/取消」按钮实际为收起编辑器 + 显示/清除已保存提示，未实现严格 Apply/Cancel 草稿缓冲——密钥与模型编辑仍是逐字段即时落盘。
2. **模型容量输入**：上下文窗口/最大输出为内联文本框（直接输入数字），未实现 K/M 后缀与行内折叠；「自定义提供方」卡的模型可留空创建（创建后仍可在编辑器内添加/探测）。
