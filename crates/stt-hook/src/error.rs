#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("virtual protect failed: {0}")]
    Protect(windows::core::Error),
    #[error("unsupported architecture for inline detour")]
    UnsupportedArch,
    #[error("null target or detour")]
    NullPointer,
    #[error("hook already installed")]
    AlreadyInstalled,
    #[error("hook not installed")]
    NotInstalled,
    #[error("self-test failed: {0}")]
    SelfTestFailed(String),
}

pub type Result<T> = std::result::Result<T, HookError>;
