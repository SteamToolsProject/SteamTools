//! Community Catalog 的内置多源聚合.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;
use stt_core::{AppId, CatalogBundle, DepotId, ManifestOverride};
use stt_platform::{
    winhttp_get, winhttp_request, HttpError, HttpMethod, WinHttpGetOptions, WinHttpRequestOptions,
};
use zip::ZipArchive;

use crate::{
    validate_bundle, CatalogError, CatalogFetchOutcome, CatalogLimits, CatalogProvider,
    CatalogResult, CatalogTraceEntry, CatalogTraceOutcome, ProviderErrorKind,
};

const APP_ID_PLACEHOLDER: &str = "{app_id}";
const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
const DEPOT_KEYS_FILE: &str = "depotkeys.json";
const APP_TOKENS_FILE: &str = "appaccesstokens.json";
const MAX_ARCHIVE_BYTES: usize = 32 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4096;
const MAX_ARCHIVE_ENTRY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ARCHIVE_EXTRACTED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE_NAME_BYTES: usize = 4096;
const MAX_VDF_DEPTH: usize = 16;
const MAX_VDF_NODES: usize = 8192;

#[derive(Debug, Clone)]
struct HttpMetadataSource {
    id: &'static str,
    url_template: &'static str,
    headers: Vec<(String, String)>,
    timeout_cap_ms: Option<u32>,
}

#[derive(Debug, Clone)]
struct ArchiveSource {
    id: &'static str,
    url_template: &'static str,
}

impl ArchiveSource {
    fn url_for(&self, app_id: AppId) -> String {
        self.url_template
            .replacen(APP_ID_PLACEHOLDER, &app_id.to_string(), 1)
    }
}

#[derive(Debug, Default)]
struct ArchiveCatalogData {
    depot_keys: HashMap<DepotId, String>,
    manifests: HashMap<DepotId, ManifestOverride>,
}

#[derive(Debug, Clone, Copy)]
struct ArchiveLimits {
    max_entries: usize,
    max_entry_bytes: u64,
    max_extracted_bytes: u64,
    max_name_bytes: usize,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_ARCHIVE_ENTRIES,
            max_entry_bytes: MAX_ARCHIVE_ENTRY_BYTES,
            max_extracted_bytes: MAX_ARCHIVE_EXTRACTED_BYTES,
            max_name_bytes: MAX_ARCHIVE_NAME_BYTES,
        }
    }
}

impl HttpMetadataSource {
    fn url_for(&self, app_id: AppId) -> String {
        self.url_template
            .replacen(APP_ID_PLACEHOLDER, &app_id.to_string(), 1)
    }
}

/// 内置社区源. 元数据、key 和 token 分能力获取后统一校验.
#[derive(Debug, Clone)]
pub struct CommunityCatalogProvider {
    cache_dir: PathBuf,
    options: WinHttpGetOptions,
    metadata_sources: Vec<HttpMetadataSource>,
    archive_sources: Vec<ArchiveSource>,
    max_snapshot_bytes: usize,
    max_archive_bytes: usize,
}

impl CommunityCatalogProvider {
    /// 创建内置社区源. `cache_dir` 下可放 key/token JSON 快照.
    pub fn new(cache_dir: impl Into<PathBuf>, options: WinHttpGetOptions) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            options,
            metadata_sources: built_in_metadata_sources(),
            archive_sources: built_in_archive_sources(),
            max_snapshot_bytes: MAX_SNAPSHOT_BYTES,
            max_archive_bytes: MAX_ARCHIVE_BYTES,
        }
    }

    fn fetch_outcome(&self, app_id: AppId) -> CatalogResult<CatalogFetchOutcome> {
        let mut trace =
            Vec::with_capacity(self.metadata_sources.len() + self.archive_sources.len() + 4);
        let metadata = self
            .fetch_metadata(app_id, &mut trace)
            .or_else(|| self.fetch_archive_metadata(app_id, &mut trace));
        let Some((mut bundle, source)) = metadata else {
            return Err(CatalogError::ChainExhausted { trace });
        };

        match self.enrich_depot_keys(&mut bundle) {
            Ok(_) => trace.push(hit_trace("community:key_snapshot")),
            Err(error) => trace.push(failed_trace("community:key_snapshot", &error)),
        }
        if !missing_depot_keys(&bundle).is_empty() {
            self.enrich_archive_keys(app_id, &mut bundle, &mut trace);
        }
        let missing = missing_depot_keys(&bundle);
        if !missing.is_empty() {
            trace.push(CatalogTraceEntry {
                provider: "community:key_completeness".to_owned(),
                outcome: CatalogTraceOutcome::Failed(ProviderErrorKind::NotFound),
            });
            return Err(CatalogError::ChainExhausted { trace });
        }

        match self.enrich_access_token(app_id, &mut bundle) {
            Ok(true) => trace.push(hit_trace("community:token_snapshot")),
            Ok(false) => trace.push(CatalogTraceEntry {
                provider: "community:token_snapshot".to_owned(),
                outcome: CatalogTraceOutcome::Failed(ProviderErrorKind::NotFound),
            }),
            Err(error) => trace.push(failed_trace("community:token_snapshot", &error)),
        }

        let bundle = validate_bundle(app_id, bundle)?;
        Ok(CatalogFetchOutcome {
            bundle,
            source: source.to_owned(),
            trace,
        })
    }

    fn fetch_metadata(
        &self,
        app_id: AppId,
        trace: &mut Vec<CatalogTraceEntry>,
    ) -> Option<(CatalogBundle, &'static str)> {
        for source in &self.metadata_sources {
            match fetch_http_metadata(source, app_id, self.options) {
                Ok(bundle) => {
                    trace.push(hit_trace(source.id));
                    return Some((bundle, source.id));
                }
                Err(error) => trace.push(failed_trace(source.id, &error)),
            }
        }
        None
    }

    fn fetch_archive_metadata(
        &self,
        app_id: AppId,
        trace: &mut Vec<CatalogTraceEntry>,
    ) -> Option<(CatalogBundle, &'static str)> {
        for source in &self.archive_sources {
            match self.fetch_archive(source, app_id) {
                Ok(data) if !data.manifests.is_empty() => {
                    let depot_ids = data.manifests.keys().copied().collect::<Vec<_>>();
                    let bundle = CatalogBundle {
                        apps: vec![app_id],
                        app_depots: HashMap::from([(app_id, depot_ids)]),
                        depot_keys: data.depot_keys,
                        manifests: data.manifests,
                        ..CatalogBundle::default()
                    };
                    match validate_bundle(app_id, bundle) {
                        Ok(bundle) => {
                            trace.push(hit_trace(source.id));
                            return Some((bundle, source.id));
                        }
                        Err(error) => trace.push(failed_trace(source.id, &error)),
                    }
                }
                Ok(_) => trace.push(CatalogTraceEntry {
                    provider: source.id.to_owned(),
                    outcome: CatalogTraceOutcome::Failed(ProviderErrorKind::NotFound),
                }),
                Err(error) => trace.push(failed_trace(source.id, &error)),
            }
        }
        None
    }

    fn enrich_depot_keys(&self, bundle: &mut CatalogBundle) -> CatalogResult<usize> {
        let depot_ids = declared_depot_ids(bundle);
        let path = self.cache_dir.join(DEPOT_KEYS_FILE);
        let keys = read_selected_strings(
            &path,
            &depot_ids,
            self.max_snapshot_bytes,
            "community:key_snapshot",
        )?;

        let mut inserted = 0;
        for (depot_id, key) in keys {
            bundle.depot_keys.insert(depot_id, key);
            inserted += 1;
        }
        Ok(inserted)
    }

    fn enrich_archive_keys(
        &self,
        app_id: AppId,
        bundle: &mut CatalogBundle,
        trace: &mut Vec<CatalogTraceEntry>,
    ) {
        for source in &self.archive_sources {
            let data = match self.fetch_archive(source, app_id) {
                Ok(data) => data,
                Err(error) => {
                    trace.push(failed_trace(source.id, &error));
                    continue;
                }
            };
            match merge_archive_keys(bundle, data.depot_keys) {
                Ok(0) => trace.push(CatalogTraceEntry {
                    provider: source.id.to_owned(),
                    outcome: CatalogTraceOutcome::Failed(ProviderErrorKind::NotFound),
                }),
                Ok(_) => trace.push(hit_trace(source.id)),
                Err(error) => {
                    trace.push(failed_trace(source.id, &error));
                    continue;
                }
            }
            if missing_depot_keys(bundle).is_empty() {
                return;
            }
        }
    }

    fn fetch_archive(
        &self,
        source: &ArchiveSource,
        app_id: AppId,
    ) -> CatalogResult<ArchiveCatalogData> {
        let options = WinHttpGetOptions {
            timeouts: self.options.timeouts,
            max_body_bytes: self.max_archive_bytes,
        };
        let response = winhttp_get(&source.url_for(app_id), options)
            .map_err(|error| map_http_error(source.id, error))?;
        if !(200..300).contains(&response.status) {
            let kind = if response.status == 404 {
                ProviderErrorKind::NotFound
            } else {
                ProviderErrorKind::Rejected
            };
            return Err(provider_error(
                source.id,
                kind,
                format!("HTTP status {}", response.status),
            ));
        }
        parse_archive(source.id, &response.body, ArchiveLimits::default())
    }

    fn enrich_access_token(
        &self,
        app_id: AppId,
        bundle: &mut CatalogBundle,
    ) -> CatalogResult<bool> {
        let path = self.cache_dir.join(APP_TOKENS_FILE);
        let selected = read_selected_strings(
            &path,
            &HashSet::from([app_id]),
            self.max_snapshot_bytes,
            "community:token_snapshot",
        )?;
        let Some(value) = selected.get(&app_id) else {
            return Ok(false);
        };
        let token = parse_u64(value, format!("apps[{app_id}].access_token"))?;
        if token == 0 {
            return Ok(false);
        }
        bundle.access_tokens.insert(app_id, token);
        Ok(true)
    }

    #[cfg(test)]
    fn with_test_sources(
        cache_dir: PathBuf,
        options: WinHttpGetOptions,
        metadata_sources: Vec<HttpMetadataSource>,
        archive_sources: Vec<ArchiveSource>,
        max_snapshot_bytes: usize,
        max_archive_bytes: usize,
    ) -> Self {
        Self {
            cache_dir,
            options,
            metadata_sources,
            archive_sources,
            max_snapshot_bytes,
            max_archive_bytes,
        }
    }
}

impl CatalogProvider for CommunityCatalogProvider {
    fn id(&self) -> &str {
        "community"
    }

    fn fetch(&self, app_id: AppId) -> CatalogResult<CatalogBundle> {
        self.fetch_outcome(app_id).map(|outcome| outcome.bundle)
    }

    fn fetch_with_trace(&self, app_id: AppId) -> CatalogResult<CatalogFetchOutcome> {
        self.fetch_outcome(app_id)
    }
}

fn built_in_metadata_sources() -> Vec<HttpMetadataSource> {
    vec![
        HttpMetadataSource {
            id: "community:steamcmd",
            url_template: "https://api.steamcmd.net/v1/info/{app_id}",
            headers: Vec::new(),
            timeout_cap_ms: None,
        },
        HttpMetadataSource {
            id: "community:ddxnb",
            url_template: "https://steam.ddxnb.cn/v1/info/{app_id}",
            headers: Vec::new(),
            timeout_cap_ms: Some(1_500),
        },
        HttpMetadataSource {
            id: "community:caigames",
            url_template: "https://api.9178666.xyz/cmd/{app_id}",
            headers: vec![
                ("X-Client-Auth".to_owned(), "CaiGames-pvzcxw".to_owned()),
                ("Accept".to_owned(), "application/json".to_owned()),
            ],
            timeout_cap_ms: Some(1_500),
        },
    ]
}

fn built_in_archive_sources() -> Vec<ArchiveSource> {
    vec![
        ArchiveSource {
            id: "community:satisl",
            url_template: "https://codeload.github.com/Satisl/MAU/zip/refs/heads/{app_id}",
        },
        ArchiveSource {
            id: "community:tymolu",
            url_template:
                "https://codeload.github.com/tymolu233/ManifestAutoUpdate/zip/refs/heads/{app_id}",
        },
        ArchiveSource {
            id: "community:auiowu",
            url_template:
                "https://codeload.github.com/Auiowu/ManifestAutoUpdate/zip/refs/heads/{app_id}",
        },
    ]
}

fn fetch_http_metadata(
    source: &HttpMetadataSource,
    app_id: AppId,
    options: WinHttpGetOptions,
) -> CatalogResult<CatalogBundle> {
    let mut timeouts = options.timeouts;
    if let Some(cap) = source.timeout_cap_ms {
        timeouts.resolve_ms = timeouts.resolve_ms.min(cap);
        timeouts.connect_ms = timeouts.connect_ms.min(cap);
        timeouts.send_ms = timeouts.send_ms.min(cap);
        timeouts.receive_ms = timeouts.receive_ms.min(cap);
    }
    let request_options = WinHttpRequestOptions {
        timeouts,
        max_request_body_bytes: 0,
        max_response_body_bytes: options.max_body_bytes,
    };
    let response = winhttp_request(
        HttpMethod::Get,
        &source.url_for(app_id),
        &source.headers,
        &[],
        request_options,
    )
    .map_err(|error| map_http_error(source.id, error))?;
    if !(200..300).contains(&response.status) {
        let kind = if response.status == 404 {
            ProviderErrorKind::NotFound
        } else {
            ProviderErrorKind::Rejected
        };
        return Err(provider_error(
            source.id,
            kind,
            format!("HTTP status {}", response.status),
        ));
    }
    parse_steamcmd_style_response(app_id, &response.body)
}

fn parse_steamcmd_style_response(app_id: AppId, body: &[u8]) -> CatalogResult<CatalogBundle> {
    let limit = CatalogLimits::default().max_wire_bytes;
    if body.len() > limit {
        return Err(CatalogError::PayloadTooLarge {
            actual: body.len(),
            limit,
        });
    }

    let root: Value = serde_json::from_slice(body)?;
    let app_key = app_id.to_string();
    let app_data = root
        .get("data")
        .and_then(|data| data.get(&app_key))
        .or_else(|| root.get(&app_key))
        .or_else(|| root.get("depots").is_some().then_some(&root))
        .ok_or(CatalogError::RequestedAppMissing(app_id))?;
    let depots = app_data
        .get("depots")
        .and_then(Value::as_object)
        .ok_or(CatalogError::RequestedAppMissing(app_id))?;

    let mut bundle = CatalogBundle {
        apps: vec![app_id],
        ..CatalogBundle::default()
    };
    let mut depot_ids = Vec::new();
    for (depot_text, depot_value) in depots {
        if !depot_text.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let depot_id =
            depot_text
                .parse::<DepotId>()
                .map_err(|_| CatalogError::InvalidDecimalU64 {
                    field: format!("depots[{depot_text}].depot_id"),
                })?;
        if depot_id == 0 {
            return Err(CatalogError::ZeroDepotId);
        }
        let Some(public) = depot_value
            .get("manifests")
            .and_then(|manifests| manifests.get("public"))
        else {
            continue;
        };
        let (gid_value, size_value) = match public {
            Value::Object(public) => (public.get("gid"), public.get("download")),
            Value::String(_) | Value::Number(_) => (Some(public), None),
            _ => continue,
        };
        let Some(gid_value) = gid_value else {
            continue;
        };
        let gid = parse_json_nonzero_u64(gid_value, format!("depots[{depot_id}].manifest.gid"))?;
        let size = size_value.map_or(Ok(0), |value| {
            parse_json_u64(value, format!("depots[{depot_id}].manifest.size"))
        })?;
        depot_ids.push(depot_id);
        bundle.manifests.insert(
            depot_id,
            ManifestOverride {
                manifest_gid: gid,
                size,
            },
        );
    }
    if depot_ids.is_empty() {
        return Err(provider_error(
            "community:metadata",
            ProviderErrorKind::NotFound,
            "no public depot manifests",
        ));
    }
    bundle.app_depots.insert(app_id, depot_ids);
    validate_bundle(app_id, bundle)
}

fn parse_json_nonzero_u64(value: &Value, field: String) -> CatalogResult<u64> {
    let parsed = parse_json_u64(value, field.clone())?;
    if parsed == 0 {
        return Err(CatalogError::InvalidDecimalU64 { field });
    }
    Ok(parsed)
}

fn parse_json_u64(value: &Value, field: String) -> CatalogResult<u64> {
    match value {
        Value::String(value) => parse_u64(value, field),
        Value::Number(value) => value
            .as_u64()
            .ok_or(CatalogError::InvalidDecimalU64 { field }),
        _ => Err(CatalogError::InvalidDecimalU64 { field }),
    }
}

fn parse_nonzero_u64(value: &str, field: String) -> CatalogResult<u64> {
    let parsed = parse_u64(value, field.clone())?;
    if parsed == 0 {
        return Err(CatalogError::InvalidDecimalU64 { field });
    }
    Ok(parsed)
}

fn parse_u64(value: &str, field: String) -> CatalogResult<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CatalogError::InvalidDecimalU64 { field });
    }
    value
        .parse()
        .map_err(|_| CatalogError::InvalidDecimalU64 { field })
}

fn declared_depot_ids(bundle: &CatalogBundle) -> HashSet<DepotId> {
    bundle.app_depots.values().flatten().copied().collect()
}

fn missing_depot_keys(bundle: &CatalogBundle) -> Vec<DepotId> {
    let mut missing = declared_depot_ids(bundle)
        .into_iter()
        .filter(|depot_id| !bundle.depot_keys.contains_key(depot_id))
        .collect::<Vec<_>>();
    missing.sort_unstable();
    missing
}

fn merge_archive_keys(
    bundle: &mut CatalogBundle,
    keys: HashMap<DepotId, String>,
) -> CatalogResult<usize> {
    let declared = declared_depot_ids(bundle);
    for (&depot_id, key) in &keys {
        if !declared.contains(&depot_id) {
            continue;
        }
        if let Some(existing) = bundle.depot_keys.get(&depot_id) {
            if !existing.eq_ignore_ascii_case(key) {
                return Err(CatalogError::ConflictingDepot(depot_id));
            }
        }
    }

    let mut inserted = 0;
    for (depot_id, key) in keys {
        if declared.contains(&depot_id) && !bundle.depot_keys.contains_key(&depot_id) {
            bundle.depot_keys.insert(depot_id, key);
            inserted += 1;
        }
    }
    Ok(inserted)
}

fn parse_archive(
    provider: &'static str,
    body: &[u8],
    limits: ArchiveLimits,
) -> CatalogResult<ArchiveCatalogData> {
    let cursor = Cursor::new(body);
    let mut archive = ZipArchive::new(cursor).map_err(|_| {
        provider_error(provider, ProviderErrorKind::Rejected, "invalid ZIP archive")
    })?;
    if archive.len() > limits.max_entries {
        return Err(CatalogError::TooManyEntries {
            field: "archive_entries",
            actual: archive.len(),
            limit: limits.max_entries,
        });
    }

    let mut result = ArchiveCatalogData::default();
    let mut total_size = 0u64;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(|_| {
            provider_error(provider, ProviderErrorKind::Rejected, "invalid ZIP entry")
        })?;
        total_size = total_size.checked_add(file.size()).ok_or_else(|| {
            provider_error(
                provider,
                ProviderErrorKind::Rejected,
                "ZIP extracted size overflow",
            )
        })?;
        if total_size > limits.max_extracted_bytes {
            return Err(CatalogError::PayloadTooLarge {
                actual: usize::try_from(total_size).unwrap_or(usize::MAX),
                limit: usize::try_from(limits.max_extracted_bytes).unwrap_or(usize::MAX),
            });
        }
        let name = file.name();
        if name.len() > limits.max_name_bytes {
            return Err(provider_error(
                provider,
                ProviderErrorKind::Rejected,
                "ZIP entry name is too long",
            ));
        }
        let file_name = name.rsplit(['/', '\\']).next().unwrap_or(name);
        if let Some((depot_id, manifest)) = parse_manifest_file_name(file_name)? {
            if let Some(existing) = result.manifests.get(&depot_id) {
                if existing != &manifest {
                    return Err(CatalogError::ConflictingDepot(depot_id));
                }
            } else {
                result.manifests.insert(depot_id, manifest);
            }
        }

        let lower_name = file_name.to_ascii_lowercase();
        let kind = if matches!(lower_name.as_str(), "key.vdf" | "config.vdf") {
            Some("vdf")
        } else if lower_name.ends_with(".lua") {
            Some("lua")
        } else {
            None
        };
        let Some(kind) = kind else {
            continue;
        };
        if file.size() > limits.max_entry_bytes {
            return Err(CatalogError::PayloadTooLarge {
                actual: usize::try_from(file.size()).unwrap_or(usize::MAX),
                limit: usize::try_from(limits.max_entry_bytes).unwrap_or(usize::MAX),
            });
        }
        let mut content = Vec::with_capacity(
            usize::try_from(file.size()).unwrap_or(limits.max_entry_bytes as usize),
        );
        file.by_ref()
            .take(limits.max_entry_bytes.saturating_add(1))
            .read_to_end(&mut content)
            .map_err(|_| {
                provider_error(
                    provider,
                    ProviderErrorKind::Rejected,
                    "ZIP entry could not be read",
                )
            })?;
        if content.len() as u64 > limits.max_entry_bytes {
            return Err(CatalogError::PayloadTooLarge {
                actual: content.len(),
                limit: usize::try_from(limits.max_entry_bytes).unwrap_or(usize::MAX),
            });
        }
        let text = std::str::from_utf8(&content).map_err(|_| {
            provider_error(
                provider,
                ProviderErrorKind::Rejected,
                "key metadata is not UTF-8",
            )
        })?;
        if kind == "vdf" {
            collect_vdf_keys(provider, text, &mut result.depot_keys)?;
        } else {
            collect_lua_keys(text, &mut result.depot_keys)?;
        }
    }
    Ok(result)
}

fn parse_manifest_file_name(file_name: &str) -> CatalogResult<Option<(DepotId, ManifestOverride)>> {
    let Some(stem) = file_name.strip_suffix(".manifest") else {
        return Ok(None);
    };
    let Some((depot, gid)) = stem.split_once('_') else {
        return Ok(None);
    };
    if depot.is_empty()
        || gid.is_empty()
        || gid.contains('_')
        || !depot.bytes().all(|byte| byte.is_ascii_digit())
        || !gid.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Ok(None);
    }
    let depot_id = depot
        .parse::<DepotId>()
        .map_err(|_| CatalogError::InvalidDecimalU64 {
            field: "archive.manifest.depot_id".to_owned(),
        })?;
    if depot_id == 0 {
        return Err(CatalogError::ZeroDepotId);
    }
    let manifest_gid = parse_nonzero_u64(gid, format!("depots[{depot_id}].manifest.gid"))?;
    Ok(Some((
        depot_id,
        ManifestOverride {
            manifest_gid,
            size: 0,
        },
    )))
}

pub(crate) fn collect_vdf_keys(
    provider: &'static str,
    text: &str,
    keys: &mut HashMap<DepotId, String>,
) -> CatalogResult<()> {
    let parsed = keyvalues_parser::parse(text).map_err(|_| {
        provider_error(
            provider,
            ProviderErrorKind::Rejected,
            "invalid VDF key metadata",
        )
    })?;
    let mut nodes = 0;
    visit_vdf_pair(
        provider,
        parsed.key.as_ref(),
        &parsed.value,
        0,
        &mut nodes,
        keys,
    )
}

fn visit_vdf_pair(
    provider: &'static str,
    key: &str,
    value: &keyvalues_parser::Value<'_>,
    depth: usize,
    nodes: &mut usize,
    keys: &mut HashMap<DepotId, String>,
) -> CatalogResult<()> {
    *nodes = nodes.saturating_add(1);
    if *nodes > MAX_VDF_NODES || depth > MAX_VDF_DEPTH {
        return Err(provider_error(
            provider,
            ProviderErrorKind::Rejected,
            "VDF structure exceeds limits",
        ));
    }
    let Some(object) = value.get_obj() else {
        return Ok(());
    };
    if key.eq_ignore_ascii_case("depots") {
        collect_depots_object(object, keys)?;
    }
    for (child_key, child_values) in object.iter() {
        for child_value in child_values {
            visit_vdf_pair(
                provider,
                child_key.as_ref(),
                child_value,
                depth + 1,
                nodes,
                keys,
            )?;
        }
    }
    Ok(())
}

fn collect_depots_object(
    object: &keyvalues_parser::Obj<'_>,
    keys: &mut HashMap<DepotId, String>,
) -> CatalogResult<()> {
    for (depot_text, values) in object.iter() {
        let Ok(depot_id) = depot_text.parse::<DepotId>() else {
            continue;
        };
        if depot_id == 0 {
            return Err(CatalogError::ZeroDepotId);
        }
        for value in values {
            let Some(depot) = value.get_obj() else {
                continue;
            };
            for (name, candidates) in depot.iter() {
                if !name.eq_ignore_ascii_case("DecryptionKey") {
                    continue;
                }
                for candidate in candidates {
                    if let Some(key) = candidate.get_str() {
                        insert_archive_key(keys, depot_id, key)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn collect_lua_keys(text: &str, keys: &mut HashMap<DepotId, String>) -> CatalogResult<()> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("--") {
            continue;
        }
        let Some(arguments) = line.strip_prefix("addappid(") else {
            continue;
        };
        let Some(arguments) = arguments.split_once(')').map(|(arguments, _)| arguments) else {
            continue;
        };
        let mut arguments = arguments.split(',').map(str::trim);
        let Some(depot_text) = arguments.next() else {
            continue;
        };
        let Some(_) = arguments.next() else {
            continue;
        };
        let Some(key_text) = arguments.next() else {
            continue;
        };
        if arguments.next().is_some() {
            continue;
        }
        let Ok(depot_id) = depot_text.parse::<DepotId>() else {
            continue;
        };
        let key = key_text
            .strip_prefix('"')
            .and_then(|key| key.strip_suffix('"'))
            .or_else(|| {
                key_text
                    .strip_prefix('\'')
                    .and_then(|key| key.strip_suffix('\''))
            });
        if let Some(key) = key {
            insert_archive_key(keys, depot_id, key)?;
        }
    }
    Ok(())
}

fn insert_archive_key(
    keys: &mut HashMap<DepotId, String>,
    depot_id: DepotId,
    key: &str,
) -> CatalogResult<()> {
    if depot_id == 0 {
        return Err(CatalogError::ZeroDepotId);
    }
    if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CatalogError::InvalidDepotKey { depot_id });
    }
    match keys.get(&depot_id) {
        Some(existing) if !existing.eq_ignore_ascii_case(key) => {
            Err(CatalogError::ConflictingDepot(depot_id))
        }
        Some(_) => Ok(()),
        None => {
            keys.insert(depot_id, key.to_owned());
            Ok(())
        }
    }
}

fn read_selected_strings(
    path: &Path,
    wanted: &HashSet<u32>,
    limit: usize,
    provider: &'static str,
) -> CatalogResult<HashMap<u32, String>> {
    let file = File::open(path).map_err(|_| {
        provider_error(
            provider,
            ProviderErrorKind::Unavailable,
            "snapshot file is unavailable",
        )
    })?;
    let read_limit = limit.saturating_add(1) as u64;
    let mut body = Vec::new();
    file.take(read_limit).read_to_end(&mut body).map_err(|_| {
        provider_error(
            provider,
            ProviderErrorKind::Unavailable,
            "snapshot file could not be read",
        )
    })?;
    if body.len() > limit {
        return Err(CatalogError::PayloadTooLarge {
            actual: body.len(),
            limit,
        });
    }

    let mut deserializer = serde_json::Deserializer::from_slice(&body);
    let selected = SelectedStringsSeed { wanted }.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(selected)
}

struct SelectedStringsSeed<'a> {
    wanted: &'a HashSet<u32>,
}

impl<'de> DeserializeSeed<'de> for SelectedStringsSeed<'_> {
    type Value = HashMap<u32, String>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(SelectedStringsVisitor {
            wanted: self.wanted,
        })
    }
}

struct SelectedStringsVisitor<'a> {
    wanted: &'a HashSet<u32>,
}

impl<'de> Visitor<'de> for SelectedStringsVisitor<'_> {
    type Value = HashMap<u32, String>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a snapshot object keyed by decimal Steam IDs")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut selected = HashMap::with_capacity(self.wanted.len());
        while let Some(key) = map.next_key::<String>()? {
            let id = key.parse::<u32>().ok();
            if let Some(id) = id.filter(|id| self.wanted.contains(id)) {
                let value = map.next_value::<String>()?;
                if selected.insert(id, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate selected Steam ID"));
                }
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selected)
    }
}

fn map_http_error(provider: &'static str, error: HttpError) -> CatalogError {
    match error {
        HttpError::Timeout { .. } => {
            provider_error(provider, ProviderErrorKind::Timeout, error.to_string())
        }
        HttpError::ResponseTooLarge { actual, limit } => {
            CatalogError::PayloadTooLarge { actual, limit }
        }
        HttpError::RequestTooLarge { .. }
        | HttpError::InvalidUrl(_)
        | HttpError::InvalidOptions(_)
        | HttpError::Windows { .. } => {
            provider_error(provider, ProviderErrorKind::Unavailable, error.to_string())
        }
    }
}

fn provider_error(
    provider: &'static str,
    kind: ProviderErrorKind,
    detail: impl Into<String>,
) -> CatalogError {
    CatalogError::Provider {
        provider: provider.to_owned(),
        kind,
        detail: detail.into(),
    }
}

fn failed_trace(provider: &str, error: &CatalogError) -> CatalogTraceEntry {
    CatalogTraceEntry {
        provider: provider.to_owned(),
        outcome: CatalogTraceOutcome::Failed(crate::chain::classify_error(error)),
    }
}

fn hit_trace(provider: &str) -> CatalogTraceEntry {
    CatalogTraceEntry {
        provider: provider.to_owned(),
        outcome: CatalogTraceOutcome::Hit,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use stt_platform::WinHttpTimeouts;
    use zip::write::SimpleFileOptions;

    use super::*;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "steamtools-community-test-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct FakeHttpServer {
        template: &'static str,
        thread: Option<JoinHandle<()>>,
    }

    impl FakeHttpServer {
        fn spawn(body: Vec<u8>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let template =
                Box::leak(format!("http://127.0.0.1:{port}/info/{{app_id}}").into_boxed_str());
            let thread = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                };
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(headers.as_bytes());
                let _ = stream.write_all(&body);
            });
            Self {
                template,
                thread: Some(thread),
            }
        }

        fn source(&self) -> HttpMetadataSource {
            HttpMetadataSource {
                id: "community:test",
                url_template: self.template,
                headers: Vec::new(),
                timeout_cap_ms: None,
            }
        }

        fn archive_source(&self) -> ArchiveSource {
            ArchiveSource {
                id: "community:test_archive",
                url_template: self.template,
            }
        }
    }

    impl Drop for FakeHttpServer {
        fn drop(&mut self) {
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }

    fn valid_body() -> Vec<u8> {
        br#"{"status":"success","data":{"42":{"depots":{"43":{"manifests":{"public":{"gid":"99","download":"100"}}}}}}}"#
            .to_vec()
    }

    fn options(max_body_bytes: usize) -> WinHttpGetOptions {
        WinHttpGetOptions {
            timeouts: WinHttpTimeouts {
                resolve_ms: 1_000,
                connect_ms: 1_000,
                send_ms: 1_000,
                receive_ms: 1_000,
            },
            max_body_bytes,
        }
    }

    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        for (name, body) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn steamcmd_parser_extracts_public_manifest() {
        let bundle = parse_steamcmd_style_response(42, &valid_body()).unwrap();

        assert_eq!(bundle.app_depots[&42], vec![43]);
        assert_eq!(bundle.manifests[&43].manifest_gid, 99);
        assert_eq!(bundle.manifests[&43].size, 100);
    }

    #[test]
    fn built_in_sources_follow_measured_hit_order() {
        let metadata = built_in_metadata_sources();
        let archives = built_in_archive_sources();

        assert_eq!(
            metadata.iter().map(|source| source.id).collect::<Vec<_>>(),
            [
                "community:steamcmd",
                "community:ddxnb",
                "community:caigames"
            ]
        );
        assert_eq!(
            archives.iter().map(|source| source.id).collect::<Vec<_>>(),
            ["community:satisl", "community:tymolu", "community:auiowu"]
        );
    }

    #[test]
    fn steamcmd_parser_rejects_missing_requested_app() {
        let body = br#"{"data":{"7":{"depots":{}}}}"#;

        let error = parse_steamcmd_style_response(42, body).unwrap_err();

        assert!(matches!(error, CatalogError::RequestedAppMissing(42)));
    }

    #[test]
    fn steamcmd_parser_rejects_zero_depot_id() {
        let body = br#"{"data":{"42":{"depots":{"0":{"manifests":{"public":{"gid":"99"}}}}}}}"#;

        let error = parse_steamcmd_style_response(42, body).unwrap_err();

        assert!(matches!(error, CatalogError::ZeroDepotId));
    }

    #[test]
    fn steamcmd_parser_rejects_invalid_gid() {
        let body = br#"{"data":{"42":{"depots":{"43":{"manifests":{"public":{"gid":"bad"}}}}}}}"#;

        let error = parse_steamcmd_style_response(42, body).unwrap_err();

        assert!(matches!(error, CatalogError::InvalidDecimalU64 { .. }));
    }

    #[test]
    fn steamcmd_parser_rejects_invalid_download_size() {
        let body = br#"{"data":{"42":{"depots":{"43":{"manifests":{"public":{"gid":"99","download":"bad"}}}}}}}"#;

        let error = parse_steamcmd_style_response(42, body).unwrap_err();

        assert!(matches!(error, CatalogError::InvalidDecimalU64 { .. }));
    }

    #[test]
    fn archive_parser_extracts_vdf_key_and_manifest_name() {
        let key = "ab".repeat(32);
        let vdf = format!("\"depots\"\n{{\n\"43\"\n{{\n\"DecryptionKey\" \"{key}\"\n}}\n}}");
        let archive = zip_bytes(&[
            ("nested/config.vdf", vdf.as_bytes()),
            ("nested/43_99.manifest", b"manifest"),
        ]);

        let data = parse_archive("community:test", &archive, ArchiveLimits::default()).unwrap();

        assert_eq!(data.depot_keys[&43], key);
        assert_eq!(data.manifests[&43].manifest_gid, 99);
    }

    #[test]
    fn archive_parser_extracts_lua_key_without_executing_script() {
        let key = "cd".repeat(32);
        let lua = format!("-- ignored\naddappid(43, 1, \"{key}\")\nerror('not executed')");
        let archive = zip_bytes(&[("42.lua", lua.as_bytes())]);

        let data = parse_archive("community:test", &archive, ArchiveLimits::default()).unwrap();

        assert_eq!(data.depot_keys[&43], key);
    }

    #[test]
    fn archive_parser_rejects_entry_over_limit() {
        let archive = zip_bytes(&[("42.lua", b"12345")]);
        let limits = ArchiveLimits {
            max_entry_bytes: 4,
            ..ArchiveLimits::default()
        };

        let error = parse_archive("community:test", &archive, limits).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::PayloadTooLarge { limit: 4, .. }
        ));
    }

    #[test]
    fn archive_parser_rejects_conflicting_manifest_ids() {
        let archive = zip_bytes(&[
            ("first/43_99.manifest", b"one"),
            ("second/43_100.manifest", b"two"),
        ]);

        let error =
            parse_archive("community:test", &archive, ArchiveLimits::default()).unwrap_err();

        assert!(matches!(error, CatalogError::ConflictingDepot(43)));
    }

    #[test]
    fn selected_snapshot_reader_ignores_unrequested_bad_values() {
        let dir = TestDir::new();
        let path = dir.0.join(DEPOT_KEYS_FILE);
        std::fs::write(&path, br#"{"43":"abab","44":{"bad":true}}"#).unwrap();

        let selected =
            read_selected_strings(&path, &HashSet::from([43]), 1024, "community:test").unwrap();

        assert_eq!(selected.get(&43).map(String::as_str), Some("abab"));
    }

    #[test]
    fn selected_snapshot_reader_rejects_oversized_file() {
        let dir = TestDir::new();
        let path = dir.0.join(DEPOT_KEYS_FILE);
        std::fs::write(&path, br#"{"43":"abab"}"#).unwrap();

        let error =
            read_selected_strings(&path, &HashSet::from([43]), 4, "community:test").unwrap_err();

        assert!(matches!(
            error,
            CatalogError::PayloadTooLarge { limit: 4, .. }
        ));
    }

    #[test]
    fn zero_token_is_treated_as_missing() {
        let dir = TestDir::new();
        std::fs::write(dir.0.join(APP_TOKENS_FILE), r#"{"42":"0"}"#).unwrap();
        let provider = CommunityCatalogProvider::with_test_sources(
            dir.0.clone(),
            options(1024),
            Vec::new(),
            Vec::new(),
            1024,
            1024,
        );
        let mut bundle = CatalogBundle::default();

        let found = provider.enrich_access_token(42, &mut bundle).unwrap();

        assert!(!found);
        assert!(bundle.access_tokens.is_empty());
    }

    #[test]
    fn community_fetch_merges_metadata_key_and_token_snapshots() {
        let _guard = crate::http_test_guard();
        let dir = TestDir::new();
        std::fs::write(
            dir.0.join(DEPOT_KEYS_FILE),
            format!(r#"{{"43":"{}"}}"#, "ab".repeat(32)),
        )
        .unwrap();
        std::fs::write(dir.0.join(APP_TOKENS_FILE), r#"{"42":"123"}"#).unwrap();
        let server = FakeHttpServer::spawn(valid_body());
        let provider = CommunityCatalogProvider::with_test_sources(
            dir.0.clone(),
            options(1024),
            vec![server.source()],
            Vec::new(),
            1024,
            1024,
        );

        let outcome = provider.fetch_with_trace(42).unwrap();

        assert_eq!(outcome.bundle.depot_keys[&43], "ab".repeat(32));
        assert_eq!(outcome.bundle.access_tokens[&42], 123);
        assert_eq!(outcome.source, "community:test");
    }

    #[test]
    fn community_fetch_fails_when_required_key_is_missing() {
        let _guard = crate::http_test_guard();
        let dir = TestDir::new();
        std::fs::write(dir.0.join(DEPOT_KEYS_FILE), "{}").unwrap();
        std::fs::write(dir.0.join(APP_TOKENS_FILE), "{}").unwrap();
        let server = FakeHttpServer::spawn(valid_body());
        let provider = CommunityCatalogProvider::with_test_sources(
            dir.0.clone(),
            options(1024),
            vec![server.source()],
            Vec::new(),
            1024,
            1024,
        );

        let error = provider.fetch(42).unwrap_err();

        assert!(matches!(error, CatalogError::ChainExhausted { .. }));
    }

    #[test]
    fn community_fetch_uses_archive_when_metadata_sources_miss() {
        let _guard = crate::http_test_guard();
        let dir = TestDir::new();
        std::fs::write(dir.0.join(DEPOT_KEYS_FILE), "{}").unwrap();
        std::fs::write(dir.0.join(APP_TOKENS_FILE), "{}").unwrap();
        let key = "ef".repeat(32);
        let vdf = format!("\"depots\"\n{{\n\"43\"\n{{\n\"DecryptionKey\" \"{key}\"\n}}\n}}");
        let archive = zip_bytes(&[
            ("config.vdf", vdf.as_bytes()),
            ("43_99.manifest", b"manifest"),
        ]);
        let server = FakeHttpServer::spawn(archive);
        let provider = CommunityCatalogProvider::with_test_sources(
            dir.0.clone(),
            options(1024),
            Vec::new(),
            vec![server.archive_source()],
            1024,
            1024 * 1024,
        );

        let outcome = provider.fetch_with_trace(42).unwrap();

        assert_eq!(outcome.bundle.depot_keys[&43], key);
        assert_eq!(outcome.bundle.manifests[&43].manifest_gid, 99);
        assert_eq!(outcome.source, "community:test_archive");
    }
}
