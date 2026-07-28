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
#[cfg(feature = "download-request-code")]
mod net_recv;
#[cfg(any(feature = "download-token", feature = "download-request-code"))]
mod net_send;
mod ownership;
#[cfg(feature = "download-request-code")]
mod request_code;
#[cfg(feature = "download-token")]
mod token;
#[cfg(any(
    feature = "download-manifest",
    feature = "download-key",
    feature = "download-token",
    feature = "download-request-code"
))]
mod verified;
#[cfg(any(feature = "download-token", feature = "download-request-code"))]
mod wire;

pub use download::{
    plan_download_kit, DownloadCapability, DownloadCapabilityReport, DownloadCapabilityStatus,
    DownloadDataAvailability, DownloadFeatureSet, DownloadKitReport, DownloadRuntimeSwitches,
};
pub use hooks::{
    add_configured_app, apply_ui_actions, hook_stats, is_attached, notify_license_changed,
    package_info_stats, register_runtime, remove_configured_app, runtime_queue,
    set_configured_apps, set_ui_action_handler, try_install_package_hooks,
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
#[cfg(feature = "download-request-code")]
pub use request_code::{
    cancel_manifest_code_work, complete_manifest_code_work, inspect_manifest_code_request_frame,
    is_manifest_code_hook_attached, manifest_code_hook_stats, register_manifest_code_worker,
    replace_manifest_code_depots, rewrite_manifest_code_response_frame,
    rewrite_manifest_code_runtime_response, try_install_manifest_code_hooks,
    ManifestCodeCompletion, ManifestCodeDepotSnapshotReport, ManifestCodeJob, ManifestCodeJobTable,
    ManifestCodeJobTicket, ManifestCodeRegister, ManifestCodeResolveWork,
    ManifestCodeResponseRewrite,
};
#[cfg(feature = "download-token")]
pub use token::{
    access_token_hook_stats, is_access_token_hook_attached, replace_access_tokens,
    rewrite_access_token_frame, try_install_access_token_hook, AccessTokenRewrite,
    AccessTokenSnapshotReport,
};
