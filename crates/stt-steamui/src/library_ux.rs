//! 库 UX 状态机 (对标 Hooks_SteamUI 的队列 / removed / 购买时间查询).
//!
//! 不碰 steamui 内存布局; 业务 detour 装上后调用这些纯逻辑.

use std::collections::HashSet;
use std::sync::Mutex;

use stt_core::{AppId, AppRules};

/// RunFrame 对单个待移除 app 的处理结果.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalDrainAction {
    /// 真拥有 (或策略跳过), 不改 UI.
    SkipSteamOwned,
    /// 应清 OwnershipFlags 并 MarkAppChange; 若已卸载则进 removed 集合.
    ClearOwnership { mark_removed_if_uninstalled: bool },
}

/// 线程安全的库 UX 控制器.
#[derive(Debug, Default)]
pub struct LibraryUx {
    inner: Mutex<LibraryUxInner>,
}

#[derive(Debug, Default)]
struct LibraryUxInner {
    pending_removals: Vec<AppId>,
    removed_app_ids: HashSet<AppId>,
}

impl LibraryUx {
    pub fn new() -> Self {
        Self::default()
    }

    /// 跨线程入队; UI RunFrame 再 drain (对标 QueueRemoval).
    pub fn queue_removal(&self, app_id: AppId) {
        let mut g = self.lock();
        if !g.pending_removals.contains(&app_id) {
            g.pending_removals.push(app_id);
        }
    }

    /// 取消待移除, 并从 removed 集合拿掉 (对标 CancelRemoval).
    pub fn cancel_removal(&self, app_id: AppId) {
        let mut g = self.lock();
        g.pending_removals.retain(|&id| id != app_id);
        g.removed_app_ids.remove(&app_id);
    }

    pub fn pending_len(&self) -> usize {
        self.lock().pending_removals.len()
    }

    pub fn removed_len(&self) -> usize {
        self.lock().removed_app_ids.len()
    }

    pub fn is_removed(&self, app_id: AppId) -> bool {
        self.lock().removed_app_ids.contains(&app_id)
    }

    /// 取出当前 pending 快照并清空队列 (RunFrame 开头 swap).
    pub fn take_pending_removals(&self) -> Vec<AppId> {
        let mut g = self.lock();
        std::mem::take(&mut g.pending_removals)
    }

    /// 全量 overview 变更时注入的 removed_appid 列表.
    pub fn removed_app_ids_snapshot(&self) -> Vec<AppId> {
        let g = self.lock();
        let mut v: Vec<_> = g.removed_app_ids.iter().copied().collect();
        v.sort_unstable();
        v
    }

    /// 单条 pending 在 RunFrame 中的策略 (对标 hkCSteamUIAppControllerRunFrame 循环体).
    ///
    /// `steam_owned`: 客户端权威拥有 (上游 IsOwned / MarkOwned); 真拥有则不从库摘.
    /// `uninstalled`: AppState 已卸载时才写入 removed 集合.
    pub fn decide_removal(steam_owned: bool, uninstalled: bool) -> RemovalDrainAction {
        if steam_owned {
            RemovalDrainAction::SkipSteamOwned
        } else {
            RemovalDrainAction::ClearOwnership {
                mark_removed_if_uninstalled: uninstalled,
            }
        }
    }

    /// 应用 RunFrame 决策: 若需 mark removed 则写入集合.
    pub fn apply_removal_outcome(&self, app_id: AppId, action: RemovalDrainAction) {
        if let RemovalDrainAction::ClearOwnership {
            mark_removed_if_uninstalled: true,
        } = action
        {
            self.lock().removed_app_ids.insert(app_id);
        }
    }

    /// FillInAppOverview: 配置里有该 app 则返回购买时间 (unix 秒).
    pub fn purchase_time_for(rules: &AppRules, app_id: AppId) -> Option<u32> {
        // 当前模型: addappid 会进 owned; 有 depot key 也算配置项.
        if rules.is_owned(app_id) || rules.app_has_depot_key(app_id) {
            rules.purchase_time(app_id)
        } else {
            None
        }
    }

    /// 配置重新出现该 app 时取消移除 (对标 NotifyLicenseChanged additions).
    pub fn on_rules_app_present(&self, app_id: AppId) {
        self.cancel_removal(app_id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LibraryUxInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_and_drain_pending() {
        let ux = LibraryUx::new();
        ux.queue_removal(1);
        ux.queue_removal(1);
        ux.queue_removal(2);
        assert_eq!(ux.pending_len(), 2);
        let d = ux.take_pending_removals();
        assert_eq!(d, vec![1, 2]);
        assert_eq!(ux.pending_len(), 0);
    }

    #[test]
    fn cancel_clears_pending_and_removed() {
        let ux = LibraryUx::new();
        ux.queue_removal(9);
        ux.apply_removal_outcome(
            9,
            RemovalDrainAction::ClearOwnership {
                mark_removed_if_uninstalled: true,
            },
        );
        assert!(ux.is_removed(9));
        ux.cancel_removal(9);
        assert!(!ux.is_removed(9));
        assert_eq!(ux.pending_len(), 0);
    }

    #[test]
    fn steam_owned_skips_clear() {
        assert_eq!(
            LibraryUx::decide_removal(true, true),
            RemovalDrainAction::SkipSteamOwned
        );
        assert_eq!(
            LibraryUx::decide_removal(false, true),
            RemovalDrainAction::ClearOwnership {
                mark_removed_if_uninstalled: true
            }
        );
    }

    #[test]
    fn purchase_time_only_for_configured_apps() {
        let mut rules = AppRules::new();
        rules.add_app(10);
        rules.set_purchase_time(10, 1700000000);
        assert_eq!(LibraryUx::purchase_time_for(&rules, 10), Some(1700000000));
        assert_eq!(LibraryUx::purchase_time_for(&rules, 11), None);
    }
}
