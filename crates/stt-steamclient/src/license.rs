//! NotifyLicenseChanged 协议: pending add/remove + UI 联动意图 (纯逻辑).

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use stt_core::AppId;

use crate::layout::PACKAGE_STATUS_AVAILABLE;

/// 对 steamui 库 UX 的联动 (由 host 转给 `LibraryUx`, 本层不依赖 steamui).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiLicenseAction {
    CancelRemoval(AppId),
    QueueRemoval(AppId),
}

/// 一次 notify 的计划结果 (attach 后才真正改 PackageInfo / 调 MarkLicense).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseNotifyPlan {
    pub additions: Vec<AppId>,
    pub removals: Vec<AppId>,
    /// 会写入 AppIdVec 的 id (additions 去重后).
    pub insert_ids: Vec<AppId>,
    /// 应从 AppIdVec 删掉且最终要 QueueRemoval 的 id.
    pub remove_ids: Vec<AppId>,
    pub ui_actions: Vec<UiLicenseAction>,
    pub should_mark_license_changed: bool,
    /// 本次变更是否已经写入 Steam client 并完成通知.
    pub client_applied: bool,
    pub skip_reason: Option<&'static str>,
}

/// 线程安全的许可变更队列 (对标 LuaConfig pending + NotifyLicenseChanged).
#[derive(Debug, Default)]
pub struct LicenseQueue {
    inner: Mutex<LicenseQueueInner>,
}

#[derive(Debug, Default)]
struct LicenseQueueInner {
    pending_add: VecDeque<AppId>,
    pending_remove: VecDeque<AppId>,
    /// 已注入 AppIdVec 的快照 (无真实内存时用逻辑集模拟).
    injected: HashSet<AppId>,
    fake_license_ready: bool,
}

impl LicenseQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn queue_addition(&self, app_id: AppId) {
        let mut g = self.lock();
        g.pending_remove.retain(|&id| id != app_id);
        // 已注入的 app 再入队会重复追加 AppIdVec, 导致 Steam license 处理异常 (库清空).
        if g.injected.contains(&app_id) {
            return;
        }
        if !g.pending_add.contains(&app_id) {
            g.pending_add.push_back(app_id);
        }
    }

    pub fn queue_removal(&self, app_id: AppId) {
        let mut g = self.lock();
        g.pending_add.retain(|&id| id != app_id);
        if !g.pending_remove.contains(&app_id) {
            g.pending_remove.push_back(app_id);
        }
    }

    pub fn pending_add_len(&self) -> usize {
        self.lock().pending_add.len()
    }

    pub fn pending_remove_len(&self) -> usize {
        self.lock().pending_remove.len()
    }

    pub fn injected_contains(&self, app_id: AppId) -> bool {
        self.lock().injected.contains(&app_id)
    }

    pub fn injected_len(&self) -> usize {
        self.lock().injected.len()
    }

    /// 单测复用进程内 OnceLock 队列时, 先清空再跑下一条.
    pub fn clear_for_test(&self) {
        let mut g = self.lock();
        g.pending_add.clear();
        g.pending_remove.clear();
        g.injected.clear();
        g.fake_license_ready = false;
    }

    pub fn is_fake_license_ready(&self) -> bool {
        self.lock().fake_license_ready
    }

    /// 配置里已有的 app 在 init 时灌入逻辑注入集 (不表示已写 client 内存).
    pub fn seed_injected_from_owned(&self, owned: impl IntoIterator<Item = AppId>) {
        let mut g = self.lock();
        for id in owned {
            g.injected.insert(id);
        }
    }

    /// 用 package0 AppIdVec 的真实内容重置逻辑 injected 集.
    ///
    /// Steam 原生「卸载」会重建/清空 package0, 但我们的逻辑集仍以为 id 在里面;
    /// 之后 reconcile 以为无差 → 库里其它入库游戏一起消失, 直到刷新清单重注入.
    /// notify 前先 resync, 再 reconcile, 才能把被 wipe 的 id 重新入 pending_add.
    pub fn resync_injected(&self, present: impl IntoIterator<Item = AppId>) {
        let mut g = self.lock();
        g.injected.clear();
        g.injected.extend(present);
    }

    /// package0 可用时标记假 license 已就绪 (实机 GetPackageInfo 成功后调用).
    pub fn mark_fake_license_ready(&self, package_status: u32) -> bool {
        if package_status != PACKAGE_STATUS_AVAILABLE {
            return false;
        }
        self.lock().fake_license_ready = true;
        true
    }

    /// 按当前 owned 快照与逻辑 injected 做差, 入 pending (不立刻 notify).
    ///
    /// 热重载 lua / 全量扫盘后用: 多出来的 queue_addition, 少了的 queue_removal.
    pub fn reconcile_owned(&self, owned: impl IntoIterator<Item = AppId>) {
        let owned: HashSet<AppId> = owned.into_iter().collect();
        let mut g = self.lock();
        let to_add: Vec<AppId> = owned.difference(&g.injected).copied().collect();
        let to_remove: Vec<AppId> = g.injected.difference(&owned).copied().collect();
        for id in to_add {
            g.pending_remove.retain(|&x| x != id);
            if !g.pending_add.contains(&id) {
                g.pending_add.push_back(id);
            }
        }
        for id in to_remove {
            g.pending_add.retain(|&x| x != id);
            if !g.pending_remove.contains(&id) {
                g.pending_remove.push_back(id);
            }
        }
    }

    /// 未 attach 时的 notify: 只更新逻辑 injected + UI 计划, 不要求 package0 Status.
    ///
    /// 真改 PackageInfo / MarkLicense 仍要等 detour; 本路径保证 core→队列→UI 协议先通.
    pub fn plan_notify_logic_only(&self) -> LicenseNotifyPlan {
        self.apply_pending(true)
    }

    /// 取出 pending 并生成 notify 计划; 成功则更新逻辑 injected 集.
    ///
    /// `package_status`: 当前 package0 的 Status; 非 Available 则 **不 drain** pending.
    /// `fake_license_initialized`: 是否已做过 InitFakeLicense (或逻辑等价).
    pub fn plan_notify(
        &self,
        package_status: u32,
        fake_license_initialized: bool,
    ) -> LicenseNotifyPlan {
        if package_status != PACKAGE_STATUS_AVAILABLE {
            return LicenseNotifyPlan {
                additions: Vec::new(),
                removals: Vec::new(),
                insert_ids: Vec::new(),
                remove_ids: Vec::new(),
                ui_actions: Vec::new(),
                should_mark_license_changed: false,
                client_applied: false,
                skip_reason: Some("package_status_not_available"),
            };
        }
        {
            let g = self.lock();
            if !fake_license_initialized && !g.fake_license_ready {
                return LicenseNotifyPlan {
                    additions: Vec::new(),
                    removals: Vec::new(),
                    insert_ids: Vec::new(),
                    remove_ids: Vec::new(),
                    ui_actions: Vec::new(),
                    should_mark_license_changed: false,
                    client_applied: false,
                    skip_reason: Some("fake_license_not_ready"),
                };
            }
        }
        self.apply_pending(false)
    }

    fn apply_pending(&self, logic_only: bool) -> LicenseNotifyPlan {
        let mut g = self.lock();
        let additions: Vec<_> = g.pending_add.drain(..).collect();
        let removals: Vec<_> = g.pending_remove.drain(..).collect();

        let mut added_ids = HashSet::new();
        let mut insert_ids = Vec::new();
        let mut ui_actions = Vec::new();

        for id in &additions {
            if added_ids.insert(*id) && g.injected.insert(*id) {
                insert_ids.push(*id);
                ui_actions.push(UiLicenseAction::CancelRemoval(*id));
            }
        }

        let mut remove_ids = Vec::new();
        for id in &removals {
            if added_ids.contains(id) {
                // 瞬时 remove+add: 不 QueueRemoval.
                continue;
            }
            if g.injected.remove(id) {
                remove_ids.push(*id);
                ui_actions.push(UiLicenseAction::QueueRemoval(*id));
            }
        }

        let should_mark = !insert_ids.is_empty() || !remove_ids.is_empty();
        if !logic_only {
            g.fake_license_ready = true;
        }

        LicenseNotifyPlan {
            additions,
            removals,
            insert_ids,
            remove_ids,
            ui_actions,
            should_mark_license_changed: should_mark,
            client_applied: false,
            skip_reason: if should_mark {
                None
            } else {
                Some("no_changes")
            },
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LicenseQueueInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl LicenseNotifyPlan {
    pub fn summary_line(&self) -> String {
        if let Some(reason) = self.skip_reason {
            if reason != "no_changes" {
                return format!(
                    "package=notify skip={reason} pending_add={} pending_rem={}",
                    self.additions.len(),
                    self.removals.len()
                );
            }
        }
        format!(
            "package=notify mode={} insert={} remove={} mark={} ui={}",
            if self.client_applied {
                "client"
            } else {
                "logic"
            },
            self.insert_ids.len(),
            self.remove_ids.len(),
            self.should_mark_license_changed,
            self.ui_actions.len()
        )
    }
}

/// 一次性把当前 owned 全部当作 additions (InitFakeLicense 逻辑集).
pub fn plan_init_fake_license(
    package_status: u32,
    owned_app_ids: &[AppId],
) -> Result<Vec<AppId>, &'static str> {
    if package_status != PACKAGE_STATUS_AVAILABLE {
        return Err("package_status_not_available");
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for &id in owned_app_ids {
        if seen.insert(id) {
            out.push(id);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_then_notify_cancels_removal() {
        let q = LicenseQueue::new();
        assert!(q.mark_fake_license_ready(PACKAGE_STATUS_AVAILABLE));
        q.queue_addition(10);
        q.queue_addition(10);
        let plan = q.plan_notify(PACKAGE_STATUS_AVAILABLE, true);
        assert_eq!(plan.insert_ids, vec![10]);
        assert!(plan.should_mark_license_changed);
        assert_eq!(plan.ui_actions, vec![UiLicenseAction::CancelRemoval(10)]);
        assert!(q.injected_contains(10));
    }

    #[test]
    fn remove_queues_ui_when_was_injected() {
        let q = LicenseQueue::new();
        q.mark_fake_license_ready(PACKAGE_STATUS_AVAILABLE);
        q.seed_injected_from_owned([7]);
        q.queue_removal(7);
        let plan = q.plan_notify(PACKAGE_STATUS_AVAILABLE, true);
        assert_eq!(plan.remove_ids, vec![7]);
        assert_eq!(plan.ui_actions, vec![UiLicenseAction::QueueRemoval(7)]);
        assert!(!q.injected_contains(7));
    }

    #[test]
    fn transient_remove_add_skips_queue_removal() {
        let q = LicenseQueue::new();
        q.mark_fake_license_ready(PACKAGE_STATUS_AVAILABLE);
        q.seed_injected_from_owned([3]);
        q.queue_removal(3);
        q.queue_addition(3);
        // queue_addition 会清 pending_remove
        assert_eq!(q.pending_remove_len(), 0);
        let plan = q.plan_notify(PACKAGE_STATUS_AVAILABLE, true);
        assert!(plan.remove_ids.is_empty());
        // 3 已在 injected: 加回不重复注入, 防 AppIdVec 重复追加 (刷新清单后库清空 bug).
        assert!(plan.insert_ids.is_empty());
    }

    #[test]
    fn skips_when_status_bad_keeps_pending() {
        let q = LicenseQueue::new();
        q.queue_addition(1);
        let plan = q.plan_notify(3, false);
        assert_eq!(plan.skip_reason, Some("package_status_not_available"));
        assert!(!plan.should_mark_license_changed);
        assert_eq!(q.pending_add_len(), 1, "skip must not drain pending");
    }

    #[test]
    fn logic_only_applies_without_package_status() {
        let q = LicenseQueue::new();
        q.queue_addition(5);
        let plan = q.plan_notify_logic_only();
        assert!(plan.should_mark_license_changed);
        assert!(!plan.client_applied);
        assert_eq!(plan.insert_ids, vec![5]);
        assert!(plan.summary_line().contains("mode=logic"));
        assert!(q.injected_contains(5));
        assert_eq!(q.pending_add_len(), 0);
    }

    #[test]
    fn reconcile_owned_queues_diff() {
        let q = LicenseQueue::new();
        q.seed_injected_from_owned([1, 2]);
        q.reconcile_owned([2, 3]);
        assert_eq!(q.pending_add_len(), 1);
        assert_eq!(q.pending_remove_len(), 1);
        let plan = q.plan_notify_logic_only();
        assert!(plan.insert_ids.contains(&3));
        assert!(plan.remove_ids.contains(&1));
        assert!(q.injected_contains(2));
        assert!(q.injected_contains(3));
        assert!(!q.injected_contains(1));
    }

    #[test]
    fn resync_then_queue_addition_reheals_wiped_apps() {
        // 模拟: 逻辑以为 1/2/3 都在; Steam 卸载把 AppIdVec 清空.
        // notify 路径: resync(present=[]) 后对 configured 逐个 queue_addition.
        let q = LicenseQueue::new();
        q.seed_injected_from_owned([1, 2, 3]);
        q.resync_injected([]);
        for id in [1u32, 2, 3] {
            q.queue_addition(id);
        }
        assert_eq!(q.pending_add_len(), 3);
        assert_eq!(q.pending_remove_len(), 0);
        let plan = q.plan_notify_logic_only();
        assert_eq!(plan.insert_ids.len(), 3);
        assert!(plan.remove_ids.is_empty());
        assert!(q.injected_contains(1));
        assert!(q.injected_contains(2));
        assert!(q.injected_contains(3));
    }

    #[test]
    fn resync_keeps_present_ids_from_being_readded() {
        let q = LicenseQueue::new();
        q.seed_injected_from_owned([1, 2, 3]);
        // 卸载只清了 2、3; 1 仍在向量里.
        q.resync_injected([1]);
        for id in [1u32, 2, 3] {
            q.queue_addition(id);
        }
        assert_eq!(q.pending_add_len(), 2);
        let plan = q.plan_notify_logic_only();
        assert!(!plan.insert_ids.contains(&1));
        assert!(plan.insert_ids.contains(&2));
        assert!(plan.insert_ids.contains(&3));
    }

    #[test]
    fn resync_without_heal_does_not_invent_pending() {
        let q = LicenseQueue::new();
        q.seed_injected_from_owned([1, 2]);
        q.resync_injected([1]);
        assert!(q.injected_contains(1));
        assert!(!q.injected_contains(2));
        assert_eq!(q.pending_add_len(), 0);
        assert_eq!(q.pending_remove_len(), 0);
    }

    #[test]
    fn init_fake_license_dedups() {
        let ids = plan_init_fake_license(PACKAGE_STATUS_AVAILABLE, &[1, 1, 2]).unwrap();
        assert_eq!(ids, vec![1, 2]);
        assert!(plan_init_fake_license(3, &[1]).is_err());
    }
}
