// 链接系统 dwmapi, 并转发公开的 Dwm* 入口.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }

    println!("cargo:rustc-link-lib=dylib=dwmapi");

    // 公共导入库里存在的符号.
    let named: &[(&str, u16)] = &[
        ("DwmAttachMilContent", 116),
        ("DwmDefWindowProc", 117),
        ("DwmDetachMilContent", 118),
        ("DwmEnableBlurBehindWindow", 119),
        ("DwmEnableComposition", 102),
        ("DwmEnableMMCSS", 120),
        ("DwmExtendFrameIntoClientArea", 121),
        ("DwmFlush", 122),
        ("DwmGetColorizationColor", 123),
        ("DwmGetCompositionTimingInfo", 125),
        ("DwmGetGraphicsStreamClient", 126),
        ("DwmGetGraphicsStreamTransformHint", 129),
        ("DwmGetTransportAttributes", 130),
        ("DwmGetUnmetTabRequirements", 133),
        ("DwmGetWindowAttribute", 134),
        ("DwmInvalidateIconicBitmaps", 149),
        ("DwmIsCompositionEnabled", 188),
        ("DwmModifyPreviousDxFrameDuration", 189),
        ("DwmQueryThumbnailSourceSize", 190),
        ("DwmRegisterThumbnail", 191),
        ("DwmRenderGesture", 192),
        ("DwmSetDxFrameDuration", 193),
        ("DwmSetIconicLivePreviewBitmap", 194),
        ("DwmSetIconicThumbnail", 195),
        ("DwmSetPresentParameters", 196),
        ("DwmSetWindowAttribute", 197),
        ("DwmShowContact", 198),
        ("DwmTetherContact", 199),
        ("DwmTransitionOwnedWindow", 200),
        ("DwmUnregisterThumbnail", 201),
        ("DwmUpdateThumbnailProperties", 202),
    ];

    for (name, ord) in named {
        println!("cargo:rustc-cdylib-link-arg=/EXPORT:{name}=DWMAPI.{name},@{ord}");
    }
}
