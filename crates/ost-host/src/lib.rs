//! Host DLL entry (`SteamTools.dll`).
//!
//! DllMain only kicks a worker thread; real init runs there.

use ost_core::AppRules;

pub fn init_placeholder() -> AppRules {
    AppRules::new()
}

/// Write the first host log under `steam_root/steamtools/`.
pub fn run_init(steam_root: &std::path::Path) -> std::io::Result<()> {
    let data = ost_platform::ensure_data_dir(steam_root)?;
    let log_path = ost_platform::host_log_path(steam_root);

    let legacy_note = if ost_platform::legacy_data_dir_exists(steam_root) {
        format!(
            "legacy_data_dir_present={}\n",
            ost_platform::legacy_data_dir(steam_root).display()
        )
    } else {
        String::new()
    };

    let body = format!(
        "SteamTools host init\n\
         steam_root={}\n\
         data_dir={}\n\
         {legacy}\
         status=init complete\n",
        steam_root.display(),
        data.display(),
        legacy = legacy_note,
    );

    std::fs::write(&log_path, body)?;
    Ok(())
}

#[cfg(windows)]
mod windows_entry {
    use std::ffi::c_void;

    use super::run_init;

    const DLL_PROCESS_ATTACH: u32 = 1;
    const DLL_PROCESS_DETACH: u32 = 0;

    unsafe extern "system" fn init_thread_proc(param: *mut c_void) -> u32 {
        if let Some(root) = ost_platform::steam_root_from_raw(param) {
            if let Err(e) = run_init(&root) {
                let fallback = root.join("SteamTools-host-error.txt");
                let _ = std::fs::write(fallback, format!("run_init failed: {e}"));
            }
        }
        0
    }

    #[no_mangle]
    pub extern "system" fn DllMain(hinst: *mut c_void, reason: u32, _reserved: *mut c_void) -> i32 {
        match reason {
            DLL_PROCESS_ATTACH => {
                unsafe {
                    ost_platform::disable_thread_library_calls_raw(hinst);
                    let _ = ost_platform::spawn_thread_raw(init_thread_proc, hinst);
                }
                1
            }
            DLL_PROCESS_DETACH => 1,
            _ => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn placeholder_init_empty() {
        let rules = init_placeholder();
        assert_eq!(rules.epoch(), 0);
    }

    #[test]
    fn run_init_writes_host_log() {
        let dir = std::env::temp_dir().join(format!("steamtools-host-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        run_init(&dir).unwrap();

        let log = dir.join("steamtools").join("host.log");
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("init complete"), "{text}");
        assert!(text.contains("steam_root="), "{text}");

        let _ = fs::remove_dir_all(&dir);
    }
}
