//! 诊断: 用宿主同一份 CDP 代码跑一次轮询, 打印结果.
//!
//! cargo run -p stt-steamui --example cdp_probe -- [--force]
//!
//! `--force` 先删掉页面上已有的按钮再挂 (改了脚本要重挂时用).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let force = args.iter().any(|a| a == "--force");

    let mut js = String::new();
    if force {
        js.push_str(
            r#"(function(){var b=document.querySelector("[data-stt-store-btn]");if(b&&b.remove)b.remove();})();"#,
        );
    }
    js.push_str(&stt_steamui::cdp_store_inject_js());

    let r = stt_steamui::poll_store_cdp("127.0.0.1:8080", &js);
    println!(
        "cdp_up={} store_pages={} injected={} force={force}",
        r.cdp_up, r.store_pages, r.injected
    );
    println!("pending={:?}", r.pending_app_ids);
    for n in &r.notes {
        println!("note: {n}");
    }
}
