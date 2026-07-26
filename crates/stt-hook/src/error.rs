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
    #[error("target function not found in import table")]
    ImportNotFound,
    #[error("steal_len {got} out of range {min}..={max}")]
    InvalidStealLen { got: usize, min: usize, max: usize },
    #[error("trampoline allocation failed")]
    TrampolineAlloc,
    #[error("self-test failed: {0}")]
    SelfTestFailed(String),
}

pub type Result<T> = std::result::Result<T, HookError>;
