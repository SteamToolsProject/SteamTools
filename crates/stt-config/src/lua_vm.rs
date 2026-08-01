//! 受限 Lua VM: 只暴露 DSL 与白名单函数, 无 os/io/require.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use mlua::{HookTriggers, Lua, LuaOptions, StdLib, Value, VmState};

/// DSL 脚本需要的最小标准库. BASE 库由 mlua 无条件加载, 无需枚举;
/// DEBUG/FFI 由 new_with 直接拒绝 (安全模式).
/// 不含 COROUTINE: mlua 0.10.x 的 set_hook 只覆盖主线程, 协程线程无钩子,
/// 可绕过指令预算; 摘掉后脚本无法创建协程, 主线程钩子即可覆盖全部执行.
fn safe_stdlib() -> StdLib {
    StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8
}

/// BASE 库中可读/执行任意文件或绕过的危险全局, 显式摘除.
const STRIPPED_GLOBALS: &[&str] = &["dofile", "loadfile", "load", "collectgarbage", "print"];

/// 单次 VM 执行的指令预算上限.
const MAX_INSTRUCTIONS: u64 = 100_000_000;

/// 钩子触发间隔 (条指令); 超预算前最多触发 200 次.
const HOOK_EVERY_NTH_INSTRUCTION: u32 = 500_000;

/// 创建受限 Lua VM: 白名单标准库 + 摘除危险全局 + 指令预算.
///
/// 每次执行前调用, 拿到全新 VM; 计数器随 VM 存活, 无需跨执行重置.
pub fn new_sandboxed() -> mlua::Result<Lua> {
    let lua = Lua::new_with(safe_stdlib(), LuaOptions::default())?;
    strip_globals(&lua)?;
    install_instruction_budget(&lua, HOOK_EVERY_NTH_INSTRUCTION, MAX_INSTRUCTIONS);
    Ok(lua)
}

fn strip_globals(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in STRIPPED_GLOBALS {
        globals.set(*name, Value::Nil)?;
    }
    Ok(())
}

/// 每 `every_nth` 条指令累计一次预算, 超过 `max` 时报错中止脚本.
fn install_instruction_budget(lua: &Lua, every_nth: u32, max: u64) {
    let counter = Arc::new(AtomicU64::new(0));
    let budget = Arc::clone(&counter);
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(every_nth),
        move |_lua, _debug| {
            let instructions =
                budget.fetch_add(u64::from(every_nth), Ordering::Relaxed) + u64::from(every_nth);
            if instructions > max {
                return Err(mlua::Error::external("lua instruction budget exceeded"));
            }
            Ok(VmState::Continue)
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangerous_globals_are_nil() {
        let lua = new_sandboxed().unwrap();
        for name in [
            "os",
            "io",
            "require",
            "dofile",
            "loadfile",
            "load",
            "collectgarbage",
            "print",
        ] {
            let value: Value = lua.globals().get(name).unwrap();
            assert!(value.is_nil(), "{name} must be nil");
        }
    }

    #[test]
    fn os_execute_is_not_executed() {
        let lua = new_sandboxed().unwrap();
        // os 未加载, 脚本引用即报错, calc 不会被执行.
        let err = lua.load(r#"os.execute("calc")"#).exec().unwrap_err();
        assert!(err.to_string().contains("os"), "unexpected error: {err}");
    }

    #[test]
    fn io_open_is_not_executed() {
        let lua = new_sandboxed().unwrap();
        let err = lua.load(r#"io.open("/etc/passwd")"#).exec().unwrap_err();
        assert!(err.to_string().contains("io"), "unexpected error: {err}");
    }

    #[test]
    fn require_errors() {
        let lua = new_sandboxed().unwrap();
        let err = lua.load(r#"require("x")"#).exec().unwrap_err();
        assert!(err.to_string().contains("nil"), "unexpected error: {err}");
    }

    #[test]
    fn dofile_errors() {
        let lua = new_sandboxed().unwrap();
        let err = lua.load(r#"dofile("evil.lua")"#).exec().unwrap_err();
        assert!(err.to_string().contains("nil"), "unexpected error: {err}");
    }

    #[test]
    fn infinite_loop_is_aborted_by_instruction_budget() {
        let lua = new_sandboxed().unwrap();
        let err = lua.load("while true do end").exec().unwrap_err();
        let message = err.to_string();
        assert!(message.contains("budget"), "unexpected error: {message}");
    }

    #[test]
    fn infinite_loop_aborts_with_lowered_budget() {
        // 私有入口可注入更紧的预算, 让单测不依赖默认 1 亿条指令.
        let lua = Lua::new_with(safe_stdlib(), LuaOptions::default()).unwrap();
        install_instruction_budget(&lua, 500, 10_000);
        let err = lua.load("while true do end").exec().unwrap_err();
        assert!(
            err.to_string().contains("budget"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn whitelisted_stdlib_still_works() {
        let lua = new_sandboxed().unwrap();
        let result: String = lua
            .load(
                r#"return string.upper("ok") .. tostring(math.floor(3.7)) .. tostring(utf8.len("ab")) .. table.concat({1,2,3}, ",")"#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, "OK321,2,3");
    }

    #[test]
    fn coroutine_global_is_nil() {
        // 协程是预算绕过通道, 沙箱摘掉 COROUTINE 后 coroutine 全局应为 nil.
        let lua = new_sandboxed().unwrap();
        let value: Value = lua.globals().get("coroutine").unwrap();
        assert!(value.is_nil(), "coroutine must be nil");
    }

    #[test]
    fn coroutine_create_errors() {
        let lua = new_sandboxed().unwrap();
        let err = lua
            .load(r#"coroutine.create(function() end)"#)
            .exec()
            .unwrap_err();
        assert!(
            err.to_string().contains("coroutine"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn coroutine_wrap_infinite_loop_errors_immediately() {
        // 回归: 旧版可经协程无限循环绕过预算而挂死; 现在 coroutine 为 nil,
        // 首条指令即报错, 不进入循环.
        let lua = new_sandboxed().unwrap();
        let err = lua
            .load(r#"coroutine.wrap(function() while true do end end)()"#)
            .exec()
            .unwrap_err();
        assert!(
            err.to_string().contains("coroutine"),
            "unexpected error: {err}"
        );
    }
}
