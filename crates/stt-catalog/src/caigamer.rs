//! CaiGamer 兼容目录源.
//!
//! 该源只作为内置社区链的末级兜底, 不参与鉴权或批量同步. 响应是上游使用
//! RC4 包裹的 Python 字符串字典, 解密后立即转换为受限的 CatalogBundle.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use stt_core::{AppId, CatalogBundle, DepotId, ManifestOverride};
use stt_platform::{winhttp_get, HttpError, WinHttpGetOptions};

use crate::{
    keys_parse::collect_vdf_keys, validate_bundle, CatalogEnricher, CatalogError,
    CatalogFetchOutcome, CatalogProvider, CatalogResult, CatalogTraceEntry, CatalogTraceOutcome,
    EnrichContext, ProviderErrorKind,
};

const PROVIDER: &str = "community:caigamer";
// 上游只有 HTTP (HTTPS 端口无有效证书), 请求本身是公开混淆, 见 RC4 注释.
const DEFAULT_URL_TEMPLATE: &str = "http://auth1.caigamer.cn/GetAppinfo/{app_id}";
// RC4 密钥只是公开混淆, 只防一眼人读, 可从二进制中直接提取; 真正的传输保护是 TLS.
// 测试要构造同密钥加密的 payload, 所以 `pub(crate)`.
pub(crate) const RC4_KEY: &[u8] = &[
    0xA1, 0xFC, 0xA1, 0xFC, 0xA1, 0xFD, 0xA1, 0xFD, 0xA1, 0xFB, 0xA1, 0xFA, 0xA1, 0xFB, 0xA1, 0xFA,
    0x42, 0x41, 0x42, 0x41,
];
const MAX_DECRYPTED_BYTES: usize = 2 * 1024 * 1024;

/// CaiGamer 目录源, 仅执行用户主动入库时的一次 AppId 查询.
#[derive(Debug, Clone)]
pub struct CaigamerCatalogProvider {
    options: WinHttpGetOptions,
    url_template: &'static str,
}

impl CaigamerCatalogProvider {
    pub const fn new(options: WinHttpGetOptions) -> Self {
        Self {
            options,
            url_template: DEFAULT_URL_TEMPLATE,
        }
    }

    /// 测试用: 指向本地假服务器.
    #[cfg(test)]
    pub(crate) fn with_url(options: WinHttpGetOptions, url_template: &'static str) -> Self {
        Self {
            options,
            url_template,
        }
    }

    fn fetch_outcome(&self, app_id: AppId) -> CatalogResult<CatalogFetchOutcome> {
        let fields = self.fetch_fields(app_id)?;
        let key_text = fields
            .get("Key")
            .ok_or_else(|| provider_error("missing Key field", ProviderErrorKind::NotFound))?;
        let appinfo_text = fields
            .get("appinfo")
            .ok_or_else(|| provider_error("missing appinfo field", ProviderErrorKind::NotFound))?;

        let mut bundle = parse_appinfo(app_id, appinfo_text)?;
        let mut keys = HashMap::new();
        collect_vdf_keys(PROVIDER, key_text, &mut keys)?;
        let declared = bundle
            .app_depots
            .get(&app_id)
            .map(|depots| depots.iter().copied().collect::<HashSet<_>>())
            .unwrap_or_default();
        bundle.depot_keys = keys
            .into_iter()
            .filter(|(depot_id, _)| declared.contains(depot_id))
            .collect();
        if let Some(config) = fields.get("config") {
            if let Some(token) = find_access_token(config)? {
                // app_token 为 0 表示上游也没收录, 不当作有效 token.
                if token != 0 {
                    bundle.access_tokens.insert(app_id, token);
                }
            }
        }

        let bundle = validate_bundle(app_id, bundle)?;
        Ok(CatalogFetchOutcome {
            bundle,
            source: PROVIDER.to_owned(),
            trace: vec![CatalogTraceEntry {
                provider: PROVIDER.to_owned(),
                outcome: CatalogTraceOutcome::Hit,
            }],
            manifest_blobs: Vec::new(),
        })
    }

    /// 请求 + RC4 解密 + Python dict 解析 (fetch_outcome 与补全接口共用).
    fn fetch_fields(
        &self,
        app_id: AppId,
    ) -> CatalogResult<std::collections::HashMap<String, String>> {
        let url = self
            .url_template
            .replacen("{app_id}", &app_id.to_string(), 1);
        let response = winhttp_get(&url, self.options).map_err(map_http_error)?;
        if !(200..300).contains(&response.status) {
            let kind = if response.status == 404 {
                ProviderErrorKind::NotFound
            } else {
                ProviderErrorKind::Rejected
            };
            return Err(provider_error(
                format!("HTTP status {}", response.status),
                kind,
            ));
        }

        let decrypted = rc4(RC4_KEY, &response.body);
        if decrypted.len() > MAX_DECRYPTED_BYTES {
            return Err(CatalogError::PayloadTooLarge {
                actual: decrypted.len(),
                limit: MAX_DECRYPTED_BYTES,
            });
        }
        let text = std::str::from_utf8(&decrypted).map_err(|_| {
            provider_error(
                "decrypted response is not UTF-8",
                ProviderErrorKind::Rejected,
            )
        })?;
        parse_python_string_dict(text)
    }

    /// 补全用: 一次请求拿全部 depot keys (不过滤 declared, 覆盖共享 depot)
    /// 与 config 里的 app_token (0 视为未收录).
    pub(crate) fn fetch_for_enrich(&self, app_id: AppId) -> CatalogResult<CaigamerEnrich> {
        let fields = self.fetch_fields(app_id)?;
        let mut depot_keys = HashMap::new();
        if let Some(key_text) = fields.get("Key") {
            collect_vdf_keys(PROVIDER, key_text, &mut depot_keys)?;
        }
        let access_token = fields
            .get("config")
            .and_then(|config| find_access_token(config).ok().flatten())
            .filter(|&token| token != 0);
        Ok(CaigamerEnrich {
            depot_keys,
            access_token,
        })
    }
}

/// 一次 caigamer 请求取回的补全数据 (key 全量 + token).
#[derive(Debug, Clone, Default)]
pub(crate) struct CaigamerEnrich {
    /// 全部 Key 字段解析出的 depot keys, 不过滤 declared.
    pub depot_keys: HashMap<DepotId, String>,
    /// config 字段里的 app_token; 0 或缺失为 None.
    pub access_token: Option<u64>,
}

impl CatalogProvider for CaigamerCatalogProvider {
    fn id(&self) -> &str {
        PROVIDER
    }

    fn fetch(&self, app_id: AppId) -> CatalogResult<CatalogBundle> {
        self.fetch_outcome(app_id).map(|outcome| outcome.bundle)
    }

    fn fetch_with_trace(&self, app_id: AppId) -> CatalogResult<CatalogFetchOutcome> {
        self.fetch_outcome(app_id)
    }
}

/// Community 成功路径上的 best-effort 补全 (不覆盖已有 key).
impl CatalogEnricher for CaigamerCatalogProvider {
    fn id(&self) -> &str {
        PROVIDER
    }

    fn enrich(&self, ctx: &mut EnrichContext<'_>) {
        let missing_keys: Vec<DepotId> = ctx
            .bundle
            .app_depots
            .values()
            .flatten()
            .copied()
            .filter(|depot_id| !ctx.bundle.depot_keys.contains_key(depot_id))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        // 与 missing_download_data 对齐: requires_token=false 不补;
        // token 0 视为未收录 (上游用 0 占位).
        let need_token = ctx.bundle.requires_token != Some(false)
            && ctx
                .bundle
                .access_tokens
                .get(&ctx.app_id)
                .copied()
                .unwrap_or(0)
                == 0;
        if missing_keys.is_empty() && !need_token {
            return;
        }
        let enrich = match self.fetch_for_enrich(ctx.app_id) {
            Ok(enrich) => enrich,
            Err(error) => {
                let kind = classify_provider_err(&error);
                if !missing_keys.is_empty() {
                    ctx.trace.push(CatalogTraceEntry {
                        provider: "community:caigamer_key".to_owned(),
                        outcome: CatalogTraceOutcome::Failed(kind),
                    });
                }
                if need_token {
                    ctx.trace.push(CatalogTraceEntry {
                        provider: "community:caigamer_token".to_owned(),
                        outcome: CatalogTraceOutcome::Failed(kind),
                    });
                }
                return;
            }
        };
        if !missing_keys.is_empty() {
            let mut filled = 0;
            for depot_id in &missing_keys {
                if let Some(key) = enrich.depot_keys.get(depot_id) {
                    if !ctx.bundle.depot_keys.contains_key(depot_id) {
                        ctx.bundle.depot_keys.insert(*depot_id, key.clone());
                        filled += 1;
                    }
                }
            }
            if filled > 0 {
                ctx.trace.push(CatalogTraceEntry {
                    provider: "community:caigamer_key".to_owned(),
                    outcome: CatalogTraceOutcome::Hit,
                });
            } else {
                ctx.trace.push(CatalogTraceEntry {
                    provider: "community:caigamer_key".to_owned(),
                    outcome: CatalogTraceOutcome::Failed(ProviderErrorKind::NotFound),
                });
            }
        }
        if need_token {
            match enrich.access_token {
                Some(token) => {
                    ctx.bundle.access_tokens.insert(ctx.app_id, token);
                    ctx.trace.push(CatalogTraceEntry {
                        provider: "community:caigamer_token".to_owned(),
                        outcome: CatalogTraceOutcome::Hit,
                    });
                }
                None => ctx.trace.push(CatalogTraceEntry {
                    provider: "community:caigamer_token".to_owned(),
                    outcome: CatalogTraceOutcome::Failed(ProviderErrorKind::NotFound),
                }),
            }
        }
    }
}

fn classify_provider_err(error: &CatalogError) -> ProviderErrorKind {
    match error {
        CatalogError::Provider { kind, .. } => *kind,
        CatalogError::RequestedAppMissing(_) => ProviderErrorKind::NotFound,
        _ => ProviderErrorKind::Rejected,
    }
}

fn map_http_error(error: HttpError) -> CatalogError {
    match error {
        HttpError::Timeout { .. } => {
            provider_error("HTTP request failed", ProviderErrorKind::Timeout)
        }
        HttpError::ResponseTooLarge { actual, limit } => {
            CatalogError::PayloadTooLarge { actual, limit }
        }
        HttpError::RequestTooLarge { .. }
        | HttpError::InvalidUrl(_)
        | HttpError::InvalidOptions(_)
        | HttpError::Windows { .. } => {
            provider_error("HTTP request failed", ProviderErrorKind::Unavailable)
        }
    }
}

fn provider_error(detail: impl Into<String>, kind: ProviderErrorKind) -> CatalogError {
    CatalogError::Provider {
        provider: PROVIDER.to_owned(),
        kind,
        detail: detail.into(),
    }
}

/// RC4 流加密 (对称), 测试用 `pub(crate)` 构造加密 payload.
pub(crate) fn rc4(key: &[u8], input: &[u8]) -> Vec<u8> {
    let mut state = [0u8; 256];
    for (index, value) in state.iter_mut().enumerate() {
        *value = index as u8;
    }
    let mut j = 0usize;
    for i in 0..256 {
        j = (j + state[i] as usize + key[i % key.len()] as usize) & 0xff;
        state.swap(i, j);
    }

    let mut i = 0usize;
    j = 0;
    let mut output = Vec::with_capacity(input.len());
    for &byte in input {
        i = (i + 1) & 0xff;
        j = (j + state[i] as usize) & 0xff;
        state.swap(i, j);
        let index = (state[i] as usize + state[j] as usize) & 0xff;
        output.push(byte ^ state[index]);
    }
    output
}

fn parse_python_string_dict(input: &str) -> CatalogResult<HashMap<String, String>> {
    let mut parser = PythonDictParser::new(input.as_bytes());
    parser.parse()
}

struct PythonDictParser<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> PythonDictParser<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn parse(&mut self) -> CatalogResult<HashMap<String, String>> {
        self.ws();
        self.expect(b'{')?;
        let mut values = HashMap::new();
        loop {
            self.ws();
            if self.consume(b'}') {
                break;
            }
            let key = self.string()?;
            self.ws();
            self.expect(b':')?;
            self.ws();
            let value = self.string()?;
            values.insert(key, value);
            self.ws();
            if !self.consume(b'}') {
                self.expect(b',')?;
            } else {
                break;
            }
        }
        self.ws();
        if self.offset != self.bytes.len() {
            return Err(provider_error(
                "trailing Python literal data",
                ProviderErrorKind::Rejected,
            ));
        }
        Ok(values)
    }

    fn string(&mut self) -> CatalogResult<String> {
        let quote = *self.bytes.get(self.offset).ok_or_else(|| {
            provider_error("unterminated Python string", ProviderErrorKind::Rejected)
        })?;
        if quote != b'\'' && quote != b'"' {
            return Err(provider_error(
                "Python literal is not a string map",
                ProviderErrorKind::Rejected,
            ));
        }
        self.offset += 1;
        let mut value = Vec::new();
        while let Some(&byte) = self.bytes.get(self.offset) {
            self.offset += 1;
            if byte == quote {
                return String::from_utf8(value).map_err(|_| {
                    provider_error("Python string is not UTF-8", ProviderErrorKind::Rejected)
                });
            }
            if byte != b'\\' {
                value.push(byte);
                continue;
            }
            let escaped = *self.bytes.get(self.offset).ok_or_else(|| {
                provider_error("unterminated Python escape", ProviderErrorKind::Rejected)
            })?;
            self.offset += 1;
            match escaped {
                b'n' => value.push(b'\n'),
                b'r' => value.push(b'\r'),
                b't' => value.push(b'\t'),
                b'a' => value.push(0x07),
                b'b' => value.push(0x08),
                b'f' => value.push(0x0c),
                b'v' => value.push(0x0b),
                b'\\' => value.push(b'\\'),
                b'\'' => value.push(b'\''),
                b'"' => value.push(b'"'),
                b'x' => {
                    let codepoint = self.hex_digits(2)?;
                    Self::push_codepoint(&mut value, codepoint)?;
                }
                b'u' => {
                    let codepoint = self.hex_digits(4)?;
                    Self::push_codepoint(&mut value, codepoint)?;
                }
                b'U' => {
                    let codepoint = self.hex_digits(8)?;
                    Self::push_codepoint(&mut value, codepoint)?;
                }
                other => {
                    // 未知转义不应悄悄吞掉反斜杠, 否则嵌套 JSON 会被改写.
                    value.push(b'\\');
                    value.push(other);
                }
            }
        }
        Err(provider_error(
            "unterminated Python string",
            ProviderErrorKind::Rejected,
        ))
    }

    fn hex_digits(&mut self, count: usize) -> CatalogResult<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            let digit = self.bytes.get(self.offset).and_then(|byte| match byte {
                b'0'..=b'9' => Some(u32::from(*byte - b'0')),
                b'a'..=b'f' => Some(u32::from(*byte - b'a' + 10)),
                b'A'..=b'F' => Some(u32::from(*byte - b'A' + 10)),
                _ => None,
            });
            let Some(digit) = digit else {
                return Err(provider_error(
                    "invalid Python unicode escape",
                    ProviderErrorKind::Rejected,
                ));
            };
            self.offset += 1;
            value = (value << 4) | digit;
        }
        Ok(value)
    }

    fn push_codepoint(output: &mut Vec<u8>, codepoint: u32) -> CatalogResult<()> {
        let Some(character) = char::from_u32(codepoint) else {
            return Err(provider_error(
                "invalid Python unicode codepoint",
                ProviderErrorKind::Rejected,
            ));
        };
        let mut encoded = [0u8; 4];
        output.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        Ok(())
    }

    fn ws(&mut self) {
        while self
            .bytes
            .get(self.offset)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.offset += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> CatalogResult<()> {
        if self.consume(expected) {
            Ok(())
        } else {
            Err(provider_error(
                "invalid Python literal",
                ProviderErrorKind::Rejected,
            ))
        }
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.bytes.get(self.offset).copied() == Some(expected) {
            self.offset += 1;
            true
        } else {
            false
        }
    }
}

/// 从扁平 appinfo 文本中提取 `"depots" {...}` 段 (按括号配对).
///
/// CaiGames 的 appinfo 是标准 Steam appinfo 格式: 根级并列多个键
/// (`"appid"`, `"common"`, `"depots"`, ...), 而 keyvalues_parser 只接受
/// 单个根 pair, 所以先切出 depots 对象再解析.
fn extract_depots_section(text: &str) -> &str {
    let Some(marker) = text.find("\"depots\"") else {
        return "";
    };
    let Some(open) = text[marker..].find('{') else {
        return "";
    };
    let start = marker + open;
    let mut depth = 0i32;
    for (offset, byte) in text[start..].char_indices() {
        match byte {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + offset + 1;
                    return &text[marker..end];
                }
            }
            _ => {}
        }
    }
    ""
}

fn parse_appinfo(app_id: AppId, text: &str) -> CatalogResult<CatalogBundle> {
    let depots = extract_depots_section(text);
    let parsed = keyvalues_parser::parse(depots)
        .map_err(|_| provider_error("invalid appinfo VDF", ProviderErrorKind::Rejected))?;
    let mut bundle = CatalogBundle {
        apps: vec![app_id],
        ..CatalogBundle::default()
    };
    collect_manifests(parsed.key.as_ref(), &parsed.value, &mut bundle, app_id, 0)?;
    if bundle.app_depots.get(&app_id).is_none_or(Vec::is_empty) {
        return Err(provider_error(
            "no public depot manifests",
            ProviderErrorKind::NotFound,
        ));
    }
    Ok(bundle)
}

fn collect_manifests(
    key: &str,
    value: &keyvalues_parser::Value<'_>,
    bundle: &mut CatalogBundle,
    app_id: AppId,
    depth: usize,
) -> CatalogResult<()> {
    if depth > 16 {
        return Err(provider_error(
            "appinfo VDF is too deep",
            ProviderErrorKind::Rejected,
        ));
    }
    let Some(object) = value.get_obj() else {
        return Ok(());
    };
    if key.eq_ignore_ascii_case("depots") {
        for (depot_text, values) in object.iter() {
            let Ok(depot_id) = depot_text.parse::<DepotId>() else {
                continue;
            };
            if depot_id == 0 {
                return Err(CatalogError::ZeroDepotId);
            }
            for depot_value in values {
                let Some(depot) = depot_value.get_obj() else {
                    continue;
                };
                let Some(manifest) = find_child_object(depot, "manifests") else {
                    continue;
                };
                let Some(public) = find_child_object(manifest, "public") else {
                    continue;
                };
                let Some(gid) = find_child_string(public, "gid") else {
                    continue;
                };
                let gid = parse_u64(gid, format!("depots[{depot_id}].manifest.gid"))?;
                if gid == 0 {
                    continue;
                }
                let size = find_child_string(public, "size")
                    .map(|value| parse_u64(value, format!("depots[{depot_id}].manifest.size")))
                    .transpose()?
                    .unwrap_or(0);
                let entry = ManifestOverride {
                    manifest_gid: gid,
                    size,
                };
                if let Some(existing) = bundle.manifests.get(&depot_id) {
                    if existing != &entry {
                        return Err(CatalogError::ConflictingDepot(depot_id));
                    }
                } else {
                    bundle.manifests.insert(depot_id, entry);
                    bundle.app_depots.entry(app_id).or_default().push(depot_id);
                }
            }
        }
    }
    for (child_key, values) in object.iter() {
        for child in values {
            collect_manifests(child_key.as_ref(), child, bundle, app_id, depth + 1)?;
        }
    }
    Ok(())
}

fn find_child_object<'a>(
    object: &'a keyvalues_parser::Obj<'a>,
    wanted: &str,
) -> Option<&'a keyvalues_parser::Obj<'a>> {
    object.iter().find_map(|(key, values)| {
        key.eq_ignore_ascii_case(wanted)
            .then(|| values.iter().find_map(|value| value.get_obj()))
            .flatten()
    })
}

fn find_child_string<'a>(object: &'a keyvalues_parser::Obj<'a>, wanted: &str) -> Option<&'a str> {
    object.iter().find_map(|(key, values)| {
        key.eq_ignore_ascii_case(wanted)
            .then(|| values.iter().find_map(|value| value.get_str()))
            .flatten()
    })
}

fn parse_u64(value: &str, field: String) -> CatalogResult<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CatalogError::InvalidDecimalU64 { field });
    }
    value
        .parse()
        .map_err(|_| CatalogError::InvalidDecimalU64 { field })
}

fn find_access_token(config: &str) -> CatalogResult<Option<u64>> {
    let value: Value = serde_json::from_str(config)
        .map_err(|_| provider_error("invalid config JSON", ProviderErrorKind::Rejected))?;
    Ok(find_token_value(&value))
}

fn find_token_value(value: &Value) -> Option<u64> {
    match value {
        Value::Object(object) => object.iter().find_map(|(key, value)| {
            let normalized = key
                .chars()
                .filter(|ch| *ch != '_' && *ch != '-')
                .flat_map(char::to_lowercase)
                .collect::<String>();
            if matches!(normalized.as_str(), "accesstoken" | "apptoken") {
                parse_json_u64(value)
            } else {
                find_token_value(value)
            }
        }),
        Value::Array(values) => values.iter().find_map(find_token_value),
        _ => None,
    }
}

fn parse_json_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_template_points_at_caigamer() {
        // 上游只有 HTTP 可达 (HTTPS 无有效证书), 见模块注释.
        assert!(
            DEFAULT_URL_TEMPLATE.starts_with("http://auth1.caigamer.cn/GetAppinfo/"),
            "{DEFAULT_URL_TEMPLATE}"
        );
    }

    #[test]
    fn parses_flat_steam_appinfo_without_root_object() {
        // 真实 CaiGames appinfo 是标准 Steam 格式: 根级并列多键, 无外层 {}.
        // 必须能提取 depots 段并解析出 public manifest.
        let flat = r#""appid" "1129580"
"common"
{
"name" "Medieval Dynasty"
}
"depots"
{
"228988"
{
"config"
{
"oslist" "windows"
}
"depotfromapp" "228980"
}
"1129581"
{
"manifests"
{
"public"
{
"gid" "7565454548429046356"
"size" "20835598749"
"download" "13671716240"
}
}
}
}
"_missing_token" "False"
"#;
        let bundle = parse_appinfo(1129580, flat).unwrap();
        assert_eq!(bundle.app_depots[&1129580], vec![1129581]);
        assert_eq!(
            bundle.manifests[&1129581].manifest_gid,
            7565454548429046356u64
        );
        // 无 public manifest 的 depot (228988) 不进入列表.
        assert!(!bundle.app_depots[&1129580].contains(&228988));
    }

    #[test]
    fn extract_depots_section_handles_missing_or_malformed() {
        assert_eq!(extract_depots_section("no depots here"), "");
        assert_eq!(extract_depots_section("\"depots\" no brace"), "");
        assert_eq!(extract_depots_section("\"depots\" { \"43\" { }"), "");
        let ok = extract_depots_section("\"a\" \"b\"\n\"depots\"\n{\n\"43\"\n{\n}\n}\n\"tail\"");
        assert!(ok.starts_with("\"depots\""));
        assert!(ok.ends_with('}'));
        assert!(ok.contains("43"));
    }

    #[test]
    fn rc4_round_trip() {
        let input = b"hello";
        assert_eq!(rc4(RC4_KEY, &rc4(RC4_KEY, input)), input);
    }

    #[test]
    fn parses_python_string_dict() {
        let parsed = parse_python_string_dict("{'Key': '中\\'文', 'config': '{}'}").unwrap();
        assert_eq!(parsed["Key"], "中'文");
        assert_eq!(parsed["config"], "{}");
    }

    #[test]
    fn decodes_python_unicode_escapes_without_dropping_unknown_ones() {
        let parsed = parse_python_string_dict(r#"{'value': '\u4e2d\x21 a\/b'}"#).unwrap();

        assert_eq!(parsed["value"], "中! a\\/b");
    }

    #[test]
    fn rejects_invalid_python_unicode_escape() {
        assert!(parse_python_string_dict(r#"{'value': '\u12'}"#).is_err());
    }

    #[test]
    fn rejects_python_dict_without_separator() {
        assert!(parse_python_string_dict("{'Key': 'value' 'config': '{}'}").is_err());
    }

    #[test]
    fn parses_appinfo_manifest() {
        let vdf = r#""appinfo" { "depots" { "43" { "manifests" { "public" { "gid" "99" "size" "10" } } } } }"#;
        let bundle = parse_appinfo(42, vdf).unwrap();
        assert_eq!(bundle.app_depots[&42], vec![43]);
        assert_eq!(bundle.manifests[&43].manifest_gid, 99);
    }

    #[test]
    fn diag_test_payload_round_trip() {
        // 复现 community 测试里构造的 payload, 验证 parser 能吃.
        let appinfo_vdf = r#""appinfo" { "depots" { "43" { "manifests" { "public" { "gid" "99" "size" "100" } } } } }"#;
        let plain = format!(
            "{{'Key': '{}', 'appinfo': '{}', 'config': '{{\"appid\": 42, \"app_token\": \"777\"}}'}}",
            "cd".repeat(32),
            appinfo_vdf
        );
        eprintln!("PLAIN: {plain:?}");
        let fields = parse_python_string_dict(&plain).unwrap();
        assert_eq!(fields["Key"], "cd".repeat(32));
        assert_eq!(fields["config"], r#"{"appid": 42, "app_token": "777"}"#);
        let token = find_access_token(&fields["config"]).unwrap();
        assert_eq!(token, Some(777));
    }
}
