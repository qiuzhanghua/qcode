# qcode 设置界面布局重构设计（V2）

> 状态：草稿，待评审
> 范围：设置窗口整体布局（外壳 / 导航 / section 划分 / 交互与保存语义），与已有 [SETTINGS-MODELS-V2.md](./SETTINGS-MODELS-V2.md)（模型接入架构，已落地 M0–M4）配套

---

## 1. 背景与目标

qcode 设置窗口目前是"顶部 ●/○ 横向导航 + 10 个页面 + 表单提交式保存"（`SetupView`，与首次运行向导共用）。
用户反馈：模型 provider 配置应有**独立的 model 栏**集中管理（已落地为 `Page::Models`）。

本设计采用**左侧导航栏 + 右侧 section 内容区**的布局，
按功能域划分 section，统一视觉惯例与保存语义。核心目标：

1. 从"横向平铺的 10 个页面"改为"左侧导航 + 分组 section"，降低跳转成本。
2. 独立 **Models** section 集中模型/provider/凭证配置（已具备，纳入本布局）。
3. 统一共享家具（section 头、行、卡片、折叠组）与保存语义，消除当前各页自成一体的观感。

---

## 2. 参考设置布局（事实依据）

### 2.1 窗口外壳

- 触发：设置按钮（图标 + 标签，窄栏仅图标）→ 全屏遮罩 + **居中模态面板**（800px 宽，圆角 32）。
- 关闭：右上 X / 遮罩点击 / Escape；关闭后焦点还给触发按钮。
- 无"返回应用"按钮，关闭即退出。

### 2.2 左侧导航栏（188px）

- 每项 = 图标 + 标签；**激活项背景填充** + `aria-current`。
- section 列表由各插件按 `order` 排序注册：`general(0)`、`models(10)`、`plugins(15)`、`agent-presets(20)`。
- 右侧内容区只渲染激活 section；无搜索框。

### 2.3 各 section 布局

| section | 结构 | 保存语义 |
|---|---|---|
| **general** | 单列行列表：每行"左 title+desc、右控件"，行间分割线，末行去边框；各功能插件注册行（permission 预设、语言、外观主题方块、字号 stepper、转写视图、回车行为） | **即时自动持久化**，无保存按钮 |
| **models** | h2+intro → provider 路由**卡片列表**（显示名+自定义标签+凭证状态圆点+编辑/删除+行内展开编辑器）；底部 Add（dormant 下拉）+ 自定义路由卡 + 端点探测（fetch models 候选 Modal）；首次运行 setup 卡 | 编辑器 Apply/Cancel |
| **plugins** | h2+intro → tab（configurable / all 清单）；配置 tab 为**可折叠卡片**（名称/描述两行头 + chevron，展开后控件 + Discard/Save，保存成功自动收起）；清单 tab 带搜索框 + 状态分组 | 卡片级**草稿 + 显式 Save/Discard** |
| **agent-presets** | h2+intro → 按信任分两组（内置/自定义）的**响应式卡片网格**；卡内"设为默认"按钮 + 徽标 + 图标操作行（查看/复制/删除）；新建走复制对话框 Modal | 操作即时生效，删除等需 Modal 确认 |

### 2.4 共享家具与惯例

- 分区头 = **h2 标题 + intro 段**（plugins、agent-presets、inventory 一致）。
- 折叠组 / 折叠卡（PluginCard、inventory 分组）；行 = "左 title+desc、右控件"。
- 原语：Menu、Modal、Tooltip、Button、RiskConfirmation 及图标集。
- 字段控件：ValueField（label+input+hint+override 徽标+reset）、SecretField。

---

## 3. qcode 现状与差距

### 3.1 现状（`crates/qcode-gui/src/setup`）

- 外壳：独立 780x620 窗口，`cmd-,` 与侧边栏按钮打开（`shell/view.rs`）。
- 导航：顶部 ●/○ 横向导航 10 页（`Page::ALL`，`forms.rs`），向导模式另加左侧步骤栏。
- 内容：schema 驱动（`FIELDS`/`TABLES`/`GUI_FIELDS`），`setup/render.rs` 按 page 渲染。
- 保存：表单提交式（`SetupView::save` → `ConfigEdit`），Review 页集中确认；设置模式显示 About。
- 已落地：`Page::Models` 独立模型栏（默认模型 + providers 表 + 目录条 + 预设 + 探测）。

### 3.2 差距

| 维度 | 参考布局 | qcode 现状 |
|---|---|---|
| 导航 | 左侧图标+标签栏，激活项高亮 | 顶部 ●/○ 横向圆点，无图标无分组 |
| section 组织 | 按功能域分区（general/models/plugins/presets） | 10 页平铺，无分组 |
| 保存语义 | 分区不同：即时自动 / 草稿+Save / 表单 Apply | 全局表单提交 + Review 页 |
| 共享家具 | h2+intro、折叠卡、行惯例、Menu/Modal | 各页自成一体，无统一分区头 |
| 凭证/模型 | 卡片 + 凭证圆点 + 行内展开编辑 | 表格行编辑（已具备同等能力） |

---

## 4. 目标布局设计（qcode 落地）

### 4.1 整体框架

```
┌──────────────────────────────────────────────────────┐
│  设置窗口（独立窗口，宽度加大至 ~880）                   │
│  左上: 文档/帮助槽    右上: ✕ 关闭                    │
├──────────────┬───────────────────────────────────────┤
│  左导航 200px  │  内容区（只渲染激活 section）          │
│  ┌──────────┐ │  ┌─────────────────────────────────┐ │
│  │◉ Models  │ │  │  h2 标题 + intro                │ │
│  │ Agent     │ │  │  ...（见 4.3 各 section）        │ │
│  │ Network…  │ │  └─────────────────────────────────┘ │
│  │ Search    │ │                                       │
│  │ MCP       │ │                                       │
│  │ Skills    │ │                                       │
│  │ Approval… │ │                                       │
│  │ Appearance│ │                                       │
│  │ Update    │ │                                       │
│  └──────────┘ │                                       │
└──────────────┴───────────────────────────────────────┘
```

- 导航：`●/○` 横向圆点 → **左侧图标+标签栏**（激活项填充高亮）。`Page` 枚举保留，导航渲染从顶部改为左侧（`setup/render.rs` 导航段重写）。
- 窗口：`780x620` → 约 `880x680`，为左栏留出空间。
- 保存语义：改为**分区级**（见 4.5），Review 页仅用于向导模式。

### 4.2 section 划分与页面映射

| 左侧导航项 | 现 `Page` | 内容 |
|---|---|---|
| **Models** | Models | 默认模型（`default_provider` + `[default-model]`）+ providers 表 + 目录条 + 预设/探测（已落地，纳入布局） |
| Agent | Agent | context / max_turns / max_cost / busy_delivery / snapshot |
| Network & Sandbox | NetworkSandbox | sandbox + network 字段 |
| Search | Search | 搜索后端与 key |
| MCP | Mcp | MCP servers 表 |
| Skills | Skills | skills 开关与 roots |
| Approval & Pricing | ApprovalPricing | approval.rules 表 + pricing 表 |
| Appearance | （新增，从 UpdateAppearance 拆出） | 主题/字号/密度/发送模式（`gui.json` 的 `GUI_FIELDS`） |
| Update | UpdateAppearance（剩余） | 更新检查与源 |
| About | Review（设置模式） | 版本信息 |
| Welcome | Welcome | 仅向导模式显示 |

> 拆出 Appearance：`GUI_FIELDS` 与 `config.toml` 字段（Update）语义不同，外观也是独立分区。向导模式的 Review/Welcome 保留原语义。

### 4.3 各 section 布局细则

- **分区头统一**：每个 section 顶部 = **h2 标题 + intro 段**。
- **Models**：
  - 顶部默认模型区（3 个字段，已落地）。
  - Provider 目录条（只读：路由/协议/凭证状态，已落地）。
  - providers **表格**（行编辑）作为卡片列表的等价物——列即"行内展开编辑器"；如后续要卡片式呈现，视觉层替换不影响数据层。
  - 底部：catalog 预设按钮 + 探测模型按钮（已落地）。
- **Agent / Network / Search / Skills / Update**：沿用现有 schema 字段，改为"分区头 + 字段区"；字段仍由 `FIELDS` 驱动。
- **MCP / Approval & Pricing**：表 + 分区头。
- **Appearance**：`GUI_FIELDS` 迁到独立 section（`gui.json` 单独保存，不并入 `config.toml`）。
- **行惯例**：标量字段呈现为"左 label+help、右控件"的行，行间分割线——当前 `render.rs` 是上下堆叠，改为左右行布局（视觉层，`FIELDS` 数据结构不动）。

### 4.4 共享组件（新增，`setup/` 内）

- `SectionHeader`（h2 + intro）。
- `PreferenceRow`（左 title+desc、右控件、分割线）——替代标量字段的堆叠渲染。
- 折叠组（`<details>` 或 GPUI 折叠）——用于 plugins 式长表单（当前无插件页，先不做）。
- 复用现有 `FieldSpec`/`render_field`/`render_table` 数据层，仅改布局呈现。

### 4.5 保存语义（分而治之）

| 区 | 语义 | 实现 |
|---|---|---|
| Models / Agent / Network / Search / MCP / Skills / Approval | **即时自动保存**（行/字段变更即写盘，叶子 diff + `ConfigEdit`，注释保留） | 每个控件变更回调 → `apply_scalar/apply_tables` 对应项 → `ConfigEdit` 原子写；与 mtime 热更新配合（已有） |
| Appearance | 即时保存到 `gui.json`（现状已是） | 不变 |
| Update | 即时保存 | 同上 |
| About | 只读 | 不变 |
| Review/Welcome | 仅向导 | 不变 |

> 现状是"一次 Save 提交全部"，改为分区即时保存后，设置窗口不再需要"保存"主按钮；
> 向导模式保留表单提交（Review 页确认）。

### 4.6 技术适配（GPUI vs Web）

- 卡片网格在 GPUI 无 rem 布局，用 `flex` + 固定/百分比宽度实现；已有全局字号缩放机制（见项目 memory）。
- Modal/Tooltip/Menu：GPUI 已有对应组件（项目多处弹窗），复用以实现删除确认、探测结果候选等。
- 卡片列表与表格（现状）二选一时，优先表格（数据层不变、已具备编辑能力），卡片化作为纯视觉升级。

---

## 5. 与 SETTINGS-MODELS-V2.md 的关系

- 本文档管**界面布局**（外壳/导航/分区/保存语义）；SETTINGS-MODELS-V2.md 管**模型接入架构**（路由/协议/模型选择/凭证，M0–M4 已落地）。
- Models section 是本布局的锚点：其内容由已落地的 M0–M4 产物（目录条、protocol 列、凭证状态、预设 catalog 化、探测按钮）直接提供。
- 落地顺序：先布局框架（左导航 + 分区头 + 行布局），再逐 section 迁移；Models 保持现状内容不动。

## 6. 落地步骤（阶段拆分）

| 阶段 | 内容 | 验证 |
|---|---|---|
| **L0 导航框架** | 导航从顶部改为左侧栏（`render.rs` 导航段重写），窗口加宽；`Page::ALL` 顺序不变 | schema 测试绿；手工打开设置窗口 |
| **L1 分区头 + 行布局** | `SectionHeader` + `PreferenceRow`，标量字段改"左文字右控件"行式；各 section 加 h2+intro | 手工回归 10 页 |
| **L2 Appearance 拆分** | `GUI_FIELDS` 迁独立 section；`UpdateAppearance` 改名 `Update` | schema 测试绿 |
| **L3 分区即时保存** | Models 等 7 区改即时自动保存（变更即写盘），移除设置模式 Save 主按钮；向导保留表单提交 | 改字段 → 关闭重开仍在；mtime 热更新正常 |
| **L4 交互增强（可选）** | 删除确认 Modal、探测结果候选 Modal、折叠组 | 手工验证 |

## 7. 明确不做 / 取舍

- **不引入插件系统**：qcode 为编译期枚举 `Page`，不做动态注册。
- **不做卡片网格**：providers 保持表格（数据层已完备），卡片化仅作为后续纯视觉选项。
- **不做设置窗口内搜索**。
- **不合并 gui.json 进 config.toml**：Appearance 独立保存，维持现状。
- **向导模式保持表单提交**：Review 页确认语义不因设置窗口改造而变。

## 8. 关键决策摘要

1. 左侧导航栏 + 右侧内容区，替代顶部 ●/○ 平铺。
2. `Page` 枚举保留为 section 清单，导航渲染从顶部改左侧；Models 已独立为模型栏。
3. 统一分区头（h2+intro）与"左文字右控件"行惯例。
4. 保存语义分而治之：7 区即时自动保存（叶子 diff），Appearance 走 gui.json，向导保留表单提交。
5. providers 保持表格 = 卡片列表的等价数据层；卡片化为可选视觉升级。
6. Appearance 从 UpdateAppearance 拆出独立 section（GUI 偏好与 config.toml 分离）。
