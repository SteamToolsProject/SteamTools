//! CheckAppOwnership 出参改写策略 (纯逻辑, 不碰内存).

use stt_core::AppId;

use crate::layout::{APP_RELEASE_STATE_RELEASED, INJECTED_PACKAGE_ID};

/// 原函数返回后, 对配置内 app 的处理.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipRewrite {
    /// 不在我们的配置里, 保持原结果.
    LeaveOriginal,
    /// 真拥有 (ExistInPackageNums > 1 且原结果 true): 记 MarkOwned, 只保证 Released.
    MarkSteamOwned,
    /// 伪造拥有: package0 + Released + owns + 非 free.
    ForgeInjected,
}

/// 上游 `Hooks_Package` 对配置 depot/app 的分支.
///
/// `configured`: 是否在 AppRules / lua 清单中.
/// `original_owned`: 原 `CheckAppOwnership` 返回值.
/// `exist_in_package_nums`: 出参里累加后的包数量 (读 `+0x14`).
pub fn decide_ownership_rewrite(
    configured: bool,
    original_owned: bool,
    exist_in_package_nums: u32,
) -> OwnershipRewrite {
    if !configured {
        return OwnershipRewrite::LeaveOriginal;
    }
    if original_owned && exist_in_package_nums > 1 {
        OwnershipRewrite::MarkSteamOwned
    } else {
        OwnershipRewrite::ForgeInjected
    }
}

/// 伪造路径要写入的字段快照 (attach 后按 layout 常量落盘).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForgedOwnershipFields {
    pub package_id: u32,
    pub release_state: u32,
    pub owns_license: bool,
    pub free_license: bool,
}

impl ForgedOwnershipFields {
    pub const fn injected() -> Self {
        Self {
            package_id: INJECTED_PACKAGE_ID,
            release_state: APP_RELEASE_STATE_RELEASED,
            owns_license: true,
            // 上游: 置 false, 避免家庭共享 DLC 场景把库条目藏掉.
            free_license: false,
        }
    }
}

/// 配置里是否应拦截该 app (owned 或带 depot key 都算).
pub fn app_is_configured(is_owned: bool, has_depot_key: bool) -> bool {
    is_owned || has_depot_key
}

/// 便于单测: 从 rules 视角判断.
pub fn configured_in_rules(rules: &stt_core::AppRules, app_id: AppId) -> bool {
    app_is_configured(rules.is_owned(app_id), rules.app_has_depot_key(app_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stt_core::{AppRules, CatalogBundle};

    #[test]
    fn non_configured_leaves_original() {
        assert_eq!(
            decide_ownership_rewrite(false, true, 9),
            OwnershipRewrite::LeaveOriginal
        );
    }

    #[test]
    fn real_multi_package_marks_owned() {
        assert_eq!(
            decide_ownership_rewrite(true, true, 2),
            OwnershipRewrite::MarkSteamOwned
        );
    }

    #[test]
    fn forge_when_not_really_owned() {
        assert_eq!(
            decide_ownership_rewrite(true, false, 0),
            OwnershipRewrite::ForgeInjected
        );
        assert_eq!(
            decide_ownership_rewrite(true, true, 1),
            OwnershipRewrite::ForgeInjected
        );
        assert_eq!(
            ForgedOwnershipFields::injected(),
            ForgedOwnershipFields {
                package_id: 0,
                release_state: 4,
                owns_license: true,
                free_license: false,
            }
        );
    }

    #[test]
    fn rules_owned_or_key_counts() {
        let mut rules = AppRules::new();
        assert!(!configured_in_rules(&rules, 1));
        rules.add_app(1);
        assert!(configured_in_rules(&rules, 1));
        let mut rules2 = AppRules::new();
        let mut bundle = CatalogBundle::default();
        bundle.app_depots.insert(2, vec![20]);
        bundle.depot_keys.insert(20, "ab".into());
        rules2.apply_catalog_bundle(&bundle);
        assert!(configured_in_rules(&rules2, 2));
    }
}
