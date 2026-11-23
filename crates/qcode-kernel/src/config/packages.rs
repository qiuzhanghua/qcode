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
    /// 套餐名（`按量计费` / `Coding Plan` / `Agent Plan`）。作为产品词展示，
    /// 与 catalog 的厂商产品名同一惯例，不翻译。
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
                label: "按量计费",
                base_url: "https://ark.cn-beijing.volces.com/api/v3",
                protocol: Protocol::OpenAi,
                api_key_env: "VOLC_ARK_API_KEY",
                default_model: None,
                models: &[],
            },
            PackageEntry {
                route: "volcano-coding",
                label: "Coding Plan",
                base_url: "https://ark.cn-beijing.volces.com/api/v3",
                protocol: Protocol::OpenAi,
                api_key_env: "VOLC_ARK_CODING_API_KEY",
                default_model: None,
                models: &[],
            },
            PackageEntry {
                route: "volcano-agent",
                label: "Agent Plan",
                base_url: "https://ark.cn-beijing.volces.com/api/v3",
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
                label: "按量计费",
                base_url: "https://open.bigmodel.cn/api/paas/v4",
                protocol: Protocol::OpenAi,
                api_key_env: "BIGMODEL_API_KEY",
                default_model: None,
                models: &[],
            },
            PackageEntry {
                route: "bigmodel-coding",
                label: "Coding Plan",
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
                label: "按量计费",
                base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
                protocol: Protocol::OpenAi,
                api_key_env: "DASHSCOPE_API_KEY",
                default_model: None,
                models: &[],
            },
            PackageEntry {
                route: "dashscope-coding",
                label: "Coding Plan",
                // TODO(套餐): Coding Plan 端点待厂商文档核对，暂同按量端点。
                base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
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
                label: "按量计费",
                base_url: "https://api.xiaomimimo.com/v1",
                protocol: Protocol::OpenAi,
                api_key_env: "XIAOMI_API_KEY",
                default_model: None,
                models: &[],
            },
            PackageEntry {
                route: "xiaomi-coding",
                label: "Coding Plan",
                // TODO(套餐): Coding Plan 端点待厂商文档核对，暂同按量端点。
                base_url: "https://api.xiaomimimo.com/v1",
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
                label: "按量计费",
                base_url: "https://api.z.ai/api/coding/paas/v4",
                protocol: Protocol::OpenAi,
                api_key_env: "ZAI_API_KEY",
                default_model: None,
                models: &[],
            },
            PackageEntry {
                route: "zai-coding",
                label: "Coding Plan",
                // TODO(套餐): Coding Plan 端点待厂商文档核对，暂同 catalog 的 zai 端点。
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
                label: "按量计费",
                // TODO(套餐): BytePlus OpenAI 兼容端点待厂商文档确认后替换。
                base_url: "",
                protocol: Protocol::OpenAi,
                api_key_env: "BYTEPLUS_API_KEY",
                default_model: None,
                models: &[],
            },
            PackageEntry {
                route: "byteplus-coding",
                label: "Coding Plan",
                // TODO(套餐): BytePlus Coding Plan 端点待厂商文档确认后替换。
                base_url: "",
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
            }
        }
    }

    #[test]
    fn entry_lookup_finds_group_and_entry() {
        let (group, entry) = package_entry("volcano-coding").expect("volcano-coding is packaged");
        assert_eq!(group.vendor, "volcano");
        assert_eq!(entry.label, "Coding Plan");
        assert_eq!(entry.api_key_env, "VOLC_ARK_CODING_API_KEY");
        assert!(package_entry("openai").is_none());
        assert!(package_group("dashscope").is_some());
        assert!(package_group("openai").is_none());
    }
}
