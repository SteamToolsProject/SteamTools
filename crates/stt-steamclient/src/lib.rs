//! steamclient package / ownership: 纯逻辑 + 可降级 hooks.
//!
//! 偏移常量见 `layout`; hook 见 `hooks` (可 `STEAMTOOLS_PACKAGE=off` 关掉).

mod hooks;
mod install;
mod layout;
mod license;
mod manifest_code;
mod ownership;

pub use hooks::{
    add_configured_app, apply_ui_actions, hook_stats, is_attached, notify_license_changed,
    register_runtime, remove_configured_app, runtime_queue, set_configured_apps,
    set_ui_action_handler, try_install_package_hooks,
};
pub use install::{
    plan_package_install, PackageInstallReport, PackageInstallStatus, PACKAGE_HOOK_SYMBOLS,
    PACKAGE_OPTIONAL_SYMBOLS, PACKAGE_P0_SYMBOLS,
};
pub use layout::{
    app_ownership, package_info, utl_vector, APP_OWNERSHIP_SIZE, APP_RELEASE_STATE_RELEASED,
    INJECTED_PACKAGE_ACCESS_TOKEN, INJECTED_PACKAGE_ID, PACKAGE_STATUS_AVAILABLE,
    PACKAGE_STATUS_INVALID,
};
pub use license::{plan_init_fake_license, LicenseNotifyPlan, LicenseQueue, UiLicenseAction};
pub use manifest_code::{
    ManifestCodeFailureKind, ManifestCodeProvider, ManifestCodeProviderResult, ManifestCodeRequest,
    ManifestCodeResolution, ManifestCodeResolverChain, ManifestCodeStage, ManifestCodeTraceEntry,
    ManifestCodeTraceOutcome, ManifestCodeUnresolved,
};
pub use ownership::{
    app_is_configured, configured_in_rules, decide_ownership_rewrite, ForgedOwnershipFields,
    OwnershipRewrite,
};
