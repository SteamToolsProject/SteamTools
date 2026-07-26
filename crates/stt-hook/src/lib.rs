//! Detour helpers and a process-local self-test hook.

#![cfg(windows)]

mod detour;
mod error;
mod self_test;

pub use detour::{HookTransaction, InlineHook};
pub use error::{HookError, Result};
pub use self_test::{run_harmless_self_test, stt_hook_probe_target};
