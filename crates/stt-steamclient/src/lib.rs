//! steamclient package / ownership: 纯逻辑 + 可降级 hooks.
//!
//! 偏移常量见 `layout`; hook 见 `hooks` (可 `STEAMTOOLS_PACKAGE=off` 关掉).

mod download;
mod hooks;
mod install;
#[cfg(feature = "download-key")]
mod key;
mod layout;
mod license;
#[cfg(feature = "download-manifest")]
mod manifest;
mod manifest_code;
mod ownership;
#[cfg(any(feature = "download-manifest", feature = "download-key"))]
mod verified;

pub use download::{
    plan_download_kit, DownloadCapability, DownloadCapabilityReport, DownloadCapabilityStatus,
    DownloadDataAvailability, DownloadFeatureSet, DownloadKitReport, DownloadRuntimeSwitches,
};
pub use hooks::{
    add_configured_app, apply_ui_actions, hook_stats, is_attached, notify_license_changed,
    register_runtime, remove_configured_app, runtime_queue, set_configured_apps,
    set_ui_action_handler, try_install_package_hooks,
};
pub use install::{
    plan_package_install, PackageInstallReport, PackageInstallStatus, PACKAGE_HOOK_SYMBOLS,
    PACKAGE_OPTIONAL_SYMBOLS, PACKAGE_P0_SYMBOLS,
};
#[cfg(feature = "download-key")]
pub use key::{
    depot_key_hook_stats, is_depot_key_hook_attached, replace_depot_keys,
    try_install_depot_key_hook, DepotKeySnapshotReport,
};
pub use layout::{
    app_ownership, package_info, utl_vector, APP_OWNERSHIP_SIZE, APP_RELEASE_STATE_RELEASED,
    INJECTED_PACKAGE_ACCESS_TOKEN, INJECTED_PACKAGE_ID, PACKAGE_STATUS_AVAILABLE,
    PACKAGE_STATUS_INVALID,
};
pub use license::{plan_init_fake_license, LicenseNotifyPlan, LicenseQueue, UiLicenseAction};
#[cfg(feature = "download-manifest")]
pub use manifest::{
    is_manifest_hook_attached, manifest_hook_stats, replace_manifest_overrides,
    try_install_manifest_hook,
};
pub use manifest_code::{
    ManifestCodeFailureKind, ManifestCodeProvider, ManifestCodeProviderResult, ManifestCodeRequest,
    ManifestCodeResolution, ManifestCodeResolverChain, ManifestCodeStage, ManifestCodeTraceEntry,
    ManifestCodeTraceOutcome, ManifestCodeUnresolved,
};
pub use ownership::{
    app_is_configured, configured_in_rules, decide_ownership_rewrite, ForgedOwnershipFields,
    OwnershipRewrite,
};
