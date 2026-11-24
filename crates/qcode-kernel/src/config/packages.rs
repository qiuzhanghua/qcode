//! Declarative table of provider packages（套餐）.
//!
//! Providers like 火山引擎 / 智谱 Bigmodel / 阿里云 sell the same models under
//! several packages（套餐）, each with its own API key and its own model set.
//! The design (PROVIDER-PACKAGES-TJ-AUTH §3) models **one package = one route**:
//! every package below is its own `providers.<route>` entry with an independent
//! key reference and model list, so resolution, keychain and the request path
//! need no change.
//!
//! This table is separate from [`super::CATALOG`]: catalog rows are plain
//! provider routes, while a package row is a *package of a vendor* and shows a
//! composite display name「厂商名 (套餐名)」. Routes that exist in both tables
//! (`xiaomi`, `zai`) are served by this table, which carries the package label.

use super::Protocol;

/// One package of one vendor: a `providers.<route>` preset entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageEntry {
    /// Provider route key (`providers.<route>` in the config).
    pub route: &'static str,
    /// 套餐名，取 [`PKG_PAY_AS_YOU_GO`] / [`PKG_CODING_PLAN`] / [`PKG_AGENT_PLAN`]
    /// 三者之一。作为产品词展示，与 catalog 的厂商产品名同一惯例，不翻译
    /// （理由见那几个常量的注释）。
    pub label: &'static str,
    /// Default endpoint; a user `base_url` overrides it.
    pub base_url: &'static str,
    /// Wire protocol this package speaks (all six vendors are OpenAI-compatible).
    pub protocol: Protocol,
    /// Recommended environment variable holding the key. A user override wins.
    pub api_key_env: &'static str,
    /// Default model when the route carries none.
    pub default_model: Option<&'static str>,
    /// Models this package advertises; empty means「由探测/手填」, matching the
    /// catalog rows added 2026-11-19.
    pub models: &'static [&'static str],
}

/// One vendor and the packages it sells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageGroup {
    /// Group key, used by the settings picker to fold packages under one vendor.
    pub vendor: &'static str,
    /// 厂商显示名（不翻译，产品名，与 catalog 同惯例）。
    pub display_name: &'static str,
    /// Packages of this vendor, in presentation order.
    pub packages: &'static [PackageEntry],
}

/// 套餐名是**厂商自己的产品词**，与 catalog 的厂商名同一惯例，不翻译。
///
/// 提成常量而不是每行写一个字面量，有两个具体原因：
/// - 门禁 6（禁止新增硬编码英文 UI 文案）按**位置**判定，`label:` 是它认的位置。
///   13 个引用点各标一句 `// i18n:keep` 只会把意图埋进噪音；这里标一次就够。
/// - 同一批出现三次以上，散着写迟早会有一处写成 `Coding plan`——那是产品词，
///   拼错就是错字。
///
/// 它们与要翻的散文不同：`按量计费` 是计费方式的通用说法，但它作为套餐名是
/// 厂商给的名字（火山引擎、阿里云等都用这同一个词指「按量」这一档）。翻成
/// "pay as you go" 会让中英两边的套餐列表看起来是两个不同的产品。
const PKG_PAY_AS_YOU_GO: &str = "按量计费";
const PKG_CODING_PLAN: &str = "Coding Plan";
const PKG_AGENT_PLAN: &str = "Agent Plan";

/// The package table, in presentation order.
///
/// 套餐清单 2026-11-23 用户确认。端点取自各厂商公开 OpenAI 兼容地址或既有
/// catalog 条目（xiaomi / zai 与 catalog 同 route，端点保持一致）；标注
/// TODO 的端点待厂商文档核对后替换。模型清单留空，由设置页「获取可用模型」
/// 探测填充——模型 id 变化快，静态清单只会过时。
pub const PACKAGES: &[PackageGroup] = &[
    PackageGroup {
        vendor: "volcano",
        display_name: "火山引擎",
        packages: &[
            PackageEntry {
                route: "volcano",
                label: PKG_PAY_AS_YOU_GO,
                base_url: "https://ark.cn-beijing.volces.com/api/v3",
                protocol: Protocol::OpenAi,
                api_key_env: "VOLC_ARK_API_KEY",
                default_model: None,
                models: &[],
            },
            PackageEntry {
                route: "volcano-coding",
                label: PKG_CODING_PLAN,
                base_url: "https://ark.cn-beijing.volces.com/api/coding/v3",
                protocol: Protocol::OpenAi,
                api_key_env: "VOLC_ARK_CODING_API_KEY",
                default_model: None,
                models: &[],
            },
            PackageEntry {
                route: "volcano-agent",
                label: PKG_AGENT_PLAN,
                base_url: "https://ark.cn-beijing.volces.com/api/plan/v3",
                protocol: Protocol::OpenAi,
                api_key_env: "VOLC_ARK_AGENT_API_KEY",
                default_model: None,
                models: &[],
            },
        ],
    },
    PackageGroup {
        vendor: "bigmodel",
        display_name: "智谱 Bigmodel",
        packages: &[
            PackageEntry {
                route: "bigmodel",
                label: PKG_PAY_AS_YOU_GO,
                base_url: "https://open.bigmodel.cn/api/paas/v4",
                protocol: Protocol::OpenAi,
                api_key_env: "BIGMODEL_API_KEY",
                default_model: None,
                models: &[],
            },
            PackageEntry {
                route: "bigmodel-coding",
                label: PKG_CODING_PLAN,
                base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
                protocol: Protocol::OpenAi,
                api_key_env: "BIGMODEL_CODING_API_KEY",
                default_model: None,
                models: &[],
            },
        ],
    },
    PackageGroup {
        vendor: "dashscope",
        display_name: "阿里云",
        packages: &[
            PackageEntry {
                route: "dashscope",
                label: PKG_PAY_AS_YOU_GO,
                base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
                protocol: Protocol::OpenAi,
                api_key_env: "DASHSCOPE_API_KEY",
                default_model: None,
                models: &[],
            },
            PackageEntry {
                route: "dashscope-coding",
                label: PKG_CODING_PLAN,
                base_url: "https://coding.dashscope.aliyuncs.com/v1",
                protocol: Protocol::OpenAi,
                api_key_env: "DASHSCOPE_CODING_API_KEY",
                default_model: None,
                models: &[],
            },
        ],
    },
    PackageGroup {
        vendor: "xiaomi",
        display_name: "小米",
        packages: &[
            PackageEntry {
                route: "xiaomi",
                label: PKG_PAY_AS_YOU_GO,
                base_url: "https://api.xiaomimimo.com/v1",
                protocol: Protocol::OpenAi,
                api_key_env: "XIAOMI_API_KEY",
                default_model: None,
                models: &[],
            },
            PackageEntry {
                route: "xiaomi-coding",
                label: PKG_CODING_PLAN,
                // 小米的套餐实为「Token Plan」；OpenAI 兼容端点与按量的
                // `api.xiaomimimo.com/v1` 不同，中国集群为 token-plan-cn 子域。
                base_url: "https://token-plan-cn.xiaomimimo.com/v1",
                protocol: Protocol::OpenAi,
                api_key_env: "XIAOMI_CODING_API_KEY",
                default_model: None,
                models: &[],
            },
        ],
    },
    PackageGroup {
        vendor: "zai",
        display_name: "Z.ai",
        packages: &[
            PackageEntry {
                route: "zai",
                label: PKG_PAY_AS_YOU_GO,
                base_url: "https://api.z.ai/api/paas/v4",
                protocol: Protocol::OpenAi,
                api_key_env: "ZAI_API_KEY",
                default_model: None,
                models: &[],
            },
            PackageEntry {
                route: "zai-coding",
                label: PKG_CODING_PLAN,
                base_url: "https://api.z.ai/api/coding/paas/v4",
                protocol: Protocol::OpenAi,
                api_key_env: "ZAI_CODING_API_KEY",
                default_model: None,
                models: &[],
            },
        ],
    },
    PackageGroup {
        vendor: "byteplus",
        display_name: "BytePlus",
        packages: &[
            PackageEntry {
                route: "byteplus",
                label: PKG_PAY_AS_YOU_GO,
                base_url: "https://ark.ap-southeast.bytepluses.com/api/v3",
                protocol: Protocol::OpenAi,
                api_key_env: "BYTEPLUS_API_KEY",
                default_model: None,
                models: &[],
            },
            PackageEntry {
                route: "byteplus-coding",
                label: PKG_CODING_PLAN,
                base_url: "https://ark.ap-southeast.bytepluses.com/api/coding/v3",
                protocol: Protocol::OpenAi,
                api_key_env: "BYTEPLUS_CODING_API_KEY",
                default_model: None,
                models: &[],
            },
        ],
    },
];

/// Every vendor group, in presentation order.
pub fn package_groups() -> impl Iterator<Item = &'static PackageGroup> {
    PACKAGES.iter()
}

/// The group of one vendor.
pub fn package_group(vendor: &str) -> Option<&'static PackageGroup> {
    PACKAGES.iter().find(|group| group.vendor == vendor)
}

/// The package entry for one route, with its vendor group.
pub fn package_entry(route: &str) -> Option<(&'static PackageGroup, &'static PackageEntry)> {
    for group in PACKAGES {
        if let Some(entry) = group.packages.iter().find(|entry| entry.route == route) {
            return Some((group, entry));
        }
    }
    None
}

/// Every route that the package table owns. Catalog rows with the same route
/// are superseded by the package row (which carries the package label).
pub fn package_routes() -> impl Iterator<Item = &'static str> {
    PACKAGES
        .iter()
        .flat_map(|group| group.packages.iter())
        .map(|entry| entry.route)
}

/// 一个 route 在界面上的显示名：套餐条目「厂商名 (套餐名)」，catalog 条目
/// 只显示厂商名（如 `DeepSeek`，不带 route），其余原样返回 route 本身。
///
/// 主界面与设置页共用这一个口径——否则同一 route 在展开列表里显示中文
/// 「火山引擎 (Coding Plan)」，选中后的按钮/状态上却显示英文 `volcano-coding`。
/// 它是纯静态表查询，不读 config，可随处调用（含渲染路径）。
pub fn provider_display_label(route: &str) -> String {
    if let Some((group, pkg)) = package_entry(route) {
        return format!("{} ({})", group.display_name, pkg.label);
    }
    if let Some(entry) = super::catalog_entry(route) {
        return entry.display_name.to_string();
    }
    route.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packages_carry_unique_routes() {
        let mut routes = package_routes().collect::<Vec<_>>();
        let len = routes.len();
        routes.sort();
        routes.dedup();
        assert_eq!(routes.len(), len, "package routes must be unique");
        // 火山引擎三套餐、其余五家两套餐 = 13 条。
        assert_eq!(len, 13, "six vendors × packages = 13 entries");
    }

    #[test]
    fn every_package_is_openai_compatible_and_labeled() {
        for group in package_groups() {
            assert!(!group.vendor.is_empty());
            assert!(!group.display_name.is_empty());
            for entry in group.packages {
                assert_eq!(entry.protocol, Protocol::OpenAi);
                assert!(!entry.label.is_empty(), "{} lacks a label", entry.route);
                assert!(
                    !entry.api_key_env.is_empty(),
                    "{} lacks an api_key_env",
                    entry.route
                );
                // 套餐端点必须是合法的 https URL：不同套餐的端点各不相同
                // （按量 / Coding Plan / Agent Plan 走不同子域或路径），写错
                // 会把套餐 key 打到错误端点、拿 404 或烧按量额度。
                assert!(
                    entry.base_url.starts_with("https://"),
                    "{} base_url is not https: {}",
                    entry.route,
                    entry.base_url
                );
            }
        }
    }

    #[test]
    fn entry_lookup_finds_group_and_entry() {
        let (group, entry) = package_entry("volcano-coding").expect("volcano-coding is packaged");
        assert_eq!(group.vendor, "volcano");
        assert_eq!(entry.label, "Coding Plan");
        assert_eq!(entry.api_key_env, "VOLC_ARK_CODING_API_KEY");
        // Coding Plan 端点与按量端点不同：这是把套餐 key 打对端点的关键。
        assert_eq!(
            entry.base_url, "https://ark.cn-beijing.volces.com/api/coding/v3",
            "volcano Coding Plan 端点必须与按量 /api/v3 区分"
        );
        assert!(package_entry("openai").is_none());
        assert!(package_group("dashscope").is_some());
        assert!(package_group("openai").is_none());
    }
}
