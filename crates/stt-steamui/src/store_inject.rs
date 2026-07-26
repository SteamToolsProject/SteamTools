//! 商店页 DOM 注入脚本 (基于本机 CEF 采集的选择器).
//!
//! 不依赖 CSteamApp 布局; 点击只入队 app_id, 真正入库走 host/inbox.

/// 写入 `steamtools/store_inject.js` 的脚本正文.
/// 用 r## 避免 JS 里 `"#id"` 提前结束 raw string.
///
/// 注意: 同一 CHTMLWindow 会先空白再导航到商店. 脚本必须可重复执行,
/// 不能 "跑过一次就永久 return", 否则配额用尽后商店页再也挂不上按钮.
pub const STORE_INJECT_JS: &str = r##"
(function () {
  window.__SteamToolsPending = window.__SteamToolsPending || [];

  function appIdFromLocation() {
    var href = String(location.href || "");
    var path = String(location.pathname || "");
    var m =
      path.match(/\/(?:agecheck\/)?app\/(\d+)/) ||
      href.match(/\/(?:agecheck\/)?app\/(\d+)/) ||
      href.match(/[?&]appid=(\d+)/i);
    if (m) return m[1];
    var el =
      document.querySelector("[data-appid]") ||
      document.querySelector("#review_appid") ||
      document.querySelector("input[name='appid']") ||
      document.querySelector("div.game_area_purchase [data-ds-appid]") ||
      document.querySelector("[data-ds-appid]");
    if (el) {
      var v =
        el.getAttribute("data-appid") ||
        el.getAttribute("data-ds-appid") ||
        el.value;
      if (v && /^\d+$/.test(String(v))) return String(v);
    }
    return null;
  }

  function isOwnedUi() {
    return !!document.querySelector(
      ".game_area_already_owned, .game_area_already_owned_ctn, .already_in_library"
    );
  }

  function enqueue(appId, reason) {
    if (!appId) return;
    var id = String(appId);
    window.__SteamToolsPending.push({
      app_id: Number(id),
      reason: reason || "click",
      href: String(location.href || ""),
      ts: Date.now(),
    });
    try {
      console.log("[SteamTools] queued app_id=" + id + " reason=" + (reason || "click"));
    } catch (e) {}
  }

  // 借 Steam 自己的按钮类 (btn_blue_steamui / btn_grey_steamui + btn_medium),
  // 渐变/字号/圆角/高度与「添加至购物车」完全一致; 返回文字所在的 span.
  function styleBtn(el, owned) {
    el.className = (owned ? "btn_grey_steamui" : "btn_blue_steamui") + " btn_medium";
    el.setAttribute("role", "button");
    el.setAttribute("data-stt-store-btn", "1");
    el.style.cssText = "cursor:" + (owned ? "default" : "pointer") + ";margin-left:2px;";
    var label = document.createElement("span");
    label.textContent = owned ? "已在库中" : "入库";
    el.appendChild(label);
    return label;
  }

  function findAnchor() {
    // 本体购买行的 .game_purchase_action_bg; 跳过试玩版与捆绑包区块.
    var games = document.querySelectorAll(".game_area_purchase_game");
    for (var i = 0; i < games.length; i++) {
      var g = games[i];
      if (String(g.className).indexOf("demo_above_purchase") >= 0) continue;
      if (g.closest && (g.closest(".demo_above_purchase") || g.closest("[data-ds-bundleid]")))
        continue;
      var c = g.querySelector(".btn_addtocart");
      if (c && c.parentElement) return c.parentElement;
      var b = g.querySelector(".game_purchase_action_bg");
      if (b) return b;
    }
    var cart = document.querySelector(".btn_addtocart");
    return (
      (cart && cart.parentElement) ||
      document.querySelector(".game_purchase_action_bg") ||
      document.querySelector(".game_purchase_action") ||
      document.querySelector("#game_area_purchase") ||
      document.querySelector(".game_area_purchase_game") ||
      document.querySelector(".game_area_already_owned")
    );
  }

  function mountOnce() {
    if (document.querySelector("[data-stt-store-btn]")) return true;
    if (/agecheck/i.test(location.pathname || "")) return false;
    var appId = appIdFromLocation();
    if (!appId) return false;

    var owned = isOwnedUi();
    var btn = document.createElement("a");
    var label = styleBtn(btn, owned);
    if (!owned) {
      btn.addEventListener("click", function (ev) {
        try {
          ev.preventDefault();
          ev.stopPropagation();
        } catch (e) {}
        enqueue(appId, "store_btn");
        label.textContent = "已排队";
        btn.className = "btn_grey_steamui btn_medium";
        btn.style.cursor = "default";
      });
    }

    var anchor = findAnchor();
    if (anchor) {
      anchor.appendChild(btn);
    } else {
      // 锚点未就绪时仍挂固定按钮, 保证狗粮可见.
      btn.style.position = "fixed";
      btn.style.right = "24px";
      btn.style.bottom = "88px";
      btn.style.marginLeft = "0";
      btn.style.boxShadow = "0 2px 8px rgba(0,0,0,.4)";
      (document.body || document.documentElement).appendChild(btn);
    }
    try {
      console.log(
        "[SteamTools] store button mounted app_id=" +
          appId +
          " owned=" +
          owned +
          " anchor=" +
          !!anchor +
          " href=" +
          location.href
      );
    } catch (e) {}
    return true;
  }

  function tick() {
    try {
      mountOnce();
    } catch (e) {
      try {
        console.warn("[SteamTools] mount error", e);
      } catch (e2) {}
    }
  }

  // 每次注入都 tick; 观察器/定时器只装一次.
  tick();
  if (!window.__SteamToolsStoreInjected) {
    window.__SteamToolsStoreInjected = true;
    var n = 0;
    window.__SteamToolsStoreTimer = setInterval(function () {
      tick();
      n += 1;
      if (n > 120) clearInterval(window.__SteamToolsStoreTimer);
    }, 500);
    try {
      var mo = new MutationObserver(function () {
        tick();
      });
      mo.observe(document.documentElement || document.body, {
        childList: true,
        subtree: true,
      });
      window.__SteamToolsStoreMo = mo;
    } catch (e) {}
  }
})();
"##;

/// 从商店 URL / 路径解析 app_id.
pub fn app_id_from_store_path(path_or_url: &str) -> Option<u32> {
    // 支持完整 URL 或 pathname.
    let s = path_or_url.trim();
    let path = if let Some(rest) = s.strip_prefix("https://") {
        rest.find('/').map(|i| &rest[i..]).unwrap_or("/")
    } else if let Some(rest) = s.strip_prefix("http://") {
        rest.find('/').map(|i| &rest[i..]).unwrap_or("/")
    } else {
        s
    };
    // /app/123 /agecheck/app/123
    let bytes = path.as_bytes();
    let mut i = 0;
    while i + 5 < bytes.len() {
        if bytes[i] == b'/'
            && i + 4 < bytes.len()
            && &path[i + 1..i + 4] == "app"
            && bytes[i + 4] == b'/'
        {
            let start = i + 5;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > start {
                if let Ok(id) = path[start..end].parse::<u32>() {
                    return Some(id);
                }
            }
        }
        // agecheck/app/
        if path[i..].starts_with("/agecheck/app/") {
            let start = i + "/agecheck/app/".len();
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > start {
                return path[start..end].parse().ok();
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_app_urls() {
        assert_eq!(
            app_id_from_store_path(
                "https://store.steampowered.com/app/3240220/Grand_Theft_Auto_V/"
            ),
            Some(3240220)
        );
        assert_eq!(
            app_id_from_store_path("https://store.steampowered.com/agecheck/app/3240220/"),
            Some(3240220)
        );
        assert_eq!(app_id_from_store_path("/app/570"), Some(570));
        assert_eq!(
            app_id_from_store_path("https://steamloopback.host/index.html"),
            None
        );
    }

    #[test]
    fn inject_js_has_markers() {
        assert!(STORE_INJECT_JS.contains("data-stt-store-btn"));
        assert!(STORE_INJECT_JS.contains("__SteamToolsPending"));
        assert!(STORE_INJECT_JS.contains("btn_addtocart"));
        assert!(STORE_INJECT_JS.contains("game_area_already_owned"));
        // 可重复注入: 不能在文件头就永久 return.
        assert!(!STORE_INJECT_JS
            .trim_start()
            .starts_with("(function () {\n  if (window.__SteamToolsStoreInjected) return;"));
        assert!(STORE_INJECT_JS.contains("findAnchor"));
        // 外观借 Steam 自己的按钮类, 不再手写像素.
        assert!(STORE_INJECT_JS.contains("btn_blue_steamui"));
        assert!(STORE_INJECT_JS.contains("btn_medium"));
    }
}
