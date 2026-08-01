//! 把 staging 里的宿主换进 Steam 根目录 (rename 旧文件 -> .old, 再拷新的).
//!
//! 当前进程里 `stbase.dll` 已被映射, 不能覆盖写; Windows 允许改名已加载的 DLL,
//! 所以先改名旧文件腾出名字, 再把新文件拷进根目录. 旧文件留 `.old` 备份,
//! 下次启动时新 DLL 报出新的版本号, 由清理逻辑删掉 `.old`.

use std::path::{Path, PathBuf};

use crate::error::UpdateError;
use crate::version::Version;
use crate::{staging_dir, state_file, update_dir, HOST_FILE_NAME};

/// 应用的 tag 落盘状态: 内容就是 `vX.Y.Z`.
pub fn read_applied_tag(steam_root: &Path) -> Option<String> {
    let path = state_file(steam_root);
    let text = std::fs::read_to_string(path).ok()?;
    let tag = text.trim();
    if tag.is_empty() {
        None
    } else {
        Some(tag.to_owned())
    }
}

fn write_applied_tag(steam_root: &Path, tag: &str) -> Result<(), UpdateError> {
    let dir = update_dir(steam_root);
    std::fs::create_dir_all(&dir).map_err(|source| UpdateError::Io {
        path: dir.clone(),
        source,
    })?;
    let path = state_file(steam_root);
    std::fs::write(&path, tag).map_err(|source| UpdateError::Io { path, source })
}

/// 执行 swap: staging -> Steam 根目录.
///
/// 返回后 `stbase.dll` 已是新版本, 但当前进程仍用旧映射, 重启 Steam 生效.
pub fn apply_staged(steam_root: &Path, tag: &str) -> Result<(), UpdateError> {
    let staged = staging_dir(steam_root).join(HOST_FILE_NAME);
    let target = steam_root.join(HOST_FILE_NAME);
    let old = steam_root.join(format!("{HOST_FILE_NAME}.old"));
    let new = steam_root.join(format!("{HOST_FILE_NAME}.new"));

    if !staged.is_file() {
        return Err(UpdateError::Io {
            path: staged,
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "staged host missing before swap",
            ),
        });
    }

    // 同目录新名写入: 目标名被已加载 DLL 占着, 先腾名 (改名允许).
    if old.exists() {
        std::fs::remove_file(&old).map_err(|source| UpdateError::Io {
            path: old.clone(),
            source,
        })?;
    }
    std::fs::rename(&target, &old).map_err(|source| UpdateError::Io {
        path: old.clone(),
        source,
    })?;
    if new.exists() {
        std::fs::remove_file(&new).map_err(|source| UpdateError::Io {
            path: new.clone(),
            source,
        })?;
    }
    std::fs::copy(&staged, &new).map_err(|source| UpdateError::Io {
        path: new.clone(),
        source,
    })?;
    // 拷贝完成再落最终名: 万一上面失败, `.old` 还在, 下次启动可回滚.
    std::fs::rename(&new, &target).map_err(|source| UpdateError::Io {
        path: target.clone(),
        source,
    })?;
    write_applied_tag(steam_root, tag)?;
    Ok(())
}

/// 上次 swap 可能只完成一半 (旧文件被改名但新文件没落盘).
///
/// 启动时调用: 若 `stbase.dll` 缺失但有 `.old`, 把 `.old` 改回去, 保证宿主可用.
pub fn rollback_if_broken(steam_root: &Path) -> Result<bool, UpdateError> {
    let target = steam_root.join(HOST_FILE_NAME);
    let old = steam_root.join(format!("{HOST_FILE_NAME}.old"));
    if !target.exists() && old.exists() {
        std::fs::rename(&old, &target).map_err(|source| UpdateError::Io {
            path: target,
            source,
        })?;
        return Ok(true);
    }
    Ok(false)
}

/// 清理上一次应用留下的 `.old` 与 staging, 返回清理了多少个文件 (仅用于日志).
pub fn cleanup_after_apply(steam_root: &Path) -> Result<u32, UpdateError> {
    let mut removed = 0u32;
    let old = steam_root.join(format!("{HOST_FILE_NAME}.old"));
    if old.exists() {
        std::fs::remove_file(&old).map_err(|source| UpdateError::Io {
            path: old.clone(),
            source,
        })?;
        removed += 1;
    }
    let staging = staging_dir(steam_root);
    if staging.is_dir() {
        std::fs::remove_dir_all(&staging).map_err(|source| UpdateError::Io {
            path: staging.clone(),
            source,
        })?;
        removed += 1;
    }
    let state = state_file(steam_root);
    if state.exists() {
        std::fs::remove_file(&state).map_err(|source| UpdateError::Io {
            path: state.clone(),
            source,
        })?;
        removed += 1;
    }
    // update 目录本身腾空后一起清掉, 别留空壳.
    let update = update_dir(steam_root);
    if update.is_dir() && std::fs::remove_dir(&update).is_ok() {
        removed += 1;
    }
    Ok(removed)
}

/// 当前运行版本 (编译期注入).
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("workspace version must be semver")
}

/// 状态文件路径的便捷别名, 给 worker 用.
pub fn applied_tag_path(steam_root: &Path) -> PathBuf {
    state_file(steam_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_host_bytes(tag: &str) -> Vec<u8> {
        format!("host-{tag}").into_bytes()
    }

    fn stage_fake(root: &Path, tag: &str) {
        let dir = staging_dir(root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(HOST_FILE_NAME), fake_host_bytes(tag)).unwrap();
    }

    #[test]
    fn apply_swaps_host_and_writes_tag() {
        let root = tempfile::tempdir().unwrap();
        // 模拟根目录已有旧宿主 (已加载, 但测试里只是文件).
        std::fs::write(root.path().join(HOST_FILE_NAME), b"old-host").unwrap();
        stage_fake(root.path(), "v0.2.0");

        apply_staged(root.path(), "v0.2.0").unwrap();

        let target = root.path().join(HOST_FILE_NAME);
        assert_eq!(std::fs::read(&target).unwrap(), fake_host_bytes("v0.2.0"));
        // 旧文件备份还在.
        assert!(root.path().join(format!("{HOST_FILE_NAME}.old")).is_file());
        assert_eq!(read_applied_tag(root.path()).as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn apply_rollback_restores_old_when_target_missing() {
        let root = tempfile::tempdir().unwrap();
        // 模拟上次 swap 只完成一半: 只有 .old, 没有新文件.
        std::fs::write(
            root.path().join(format!("{HOST_FILE_NAME}.old")),
            b"old-host",
        )
        .unwrap();

        let rolled_back = rollback_if_broken(root.path()).unwrap();
        assert!(rolled_back);
        assert!(root.path().join(HOST_FILE_NAME).is_file());
        assert!(!root.path().join(format!("{HOST_FILE_NAME}.old")).exists());
    }

    #[test]
    fn cleanup_removes_old_staging_and_state() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(format!("{HOST_FILE_NAME}.old")), b"old").unwrap();
        stage_fake(root.path(), "v0.2.0");
        write_applied_tag(root.path(), "v0.2.0").unwrap();

        let removed = cleanup_after_apply(root.path()).unwrap();
        assert_eq!(removed, 4);
        assert!(!root.path().join(format!("{HOST_FILE_NAME}.old")).exists());
        assert!(!staging_dir(root.path()).exists());
        assert!(!state_file(root.path()).exists());
        assert!(!update_dir(root.path()).exists());
    }

    #[test]
    fn cleanup_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let first = cleanup_after_apply(root.path()).unwrap();
        let second = cleanup_after_apply(root.path()).unwrap();
        assert_eq!(first, 0);
        assert_eq!(second, 0);
    }
}
