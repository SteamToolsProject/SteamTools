// 导出表写在这里 (与 stt-loader-dwmapi 同思路). 生成临时 .def:
// MSVC 需要固定 ordinal / NONAME; 纯 /EXPORT 会被 rustc 自动 def 冲掉.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }

    let named: &[(&str, u16)] = &[
        ("XInputGetState", 2),
        ("XInputSetState", 3),
        ("XInputGetCapabilities", 4),
        ("XInputEnable", 5),
        ("XInputGetBatteryInformation", 7),
        ("XInputGetKeystroke", 8),
        ("XInputGetAudioDeviceIds", 10),
    ];
    let ordinals: &[(&str, u16)] = &[
        ("XInputOrdinal100", 100),
        ("XInputOrdinal101", 101),
        ("XInputOrdinal102", 102),
        ("XInputOrdinal103", 103),
        ("XInputOrdinal104", 104),
        ("XInputOrdinal108", 108),
    ];

    let mut def = String::from("LIBRARY \"xinput1_4\"\nEXPORTS\n");
    for (name, ord) in named {
        def.push_str(&format!("    {name} @{ord}\n"));
    }
    for (name, ord) in ordinals {
        def.push_str(&format!("    {name} @{ord} NONAME\n"));
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let def_path = std::path::Path::new(&out_dir).join("xinput1_4.def");
    std::fs::write(&def_path, def).expect("write xinput1_4.def");
    println!("cargo:rustc-cdylib-link-arg=/DEF:{}", def_path.display());
}
