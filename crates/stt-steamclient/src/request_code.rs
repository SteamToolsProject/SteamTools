//! Manifest request code 的 service-method frame 与有界 job 状态.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use stt_core::DepotId;
use stt_metadata::PatternStore;

use crate::manifest_code::ManifestCodeRequest;
use crate::wire::{encode_varint, parse_field, WireValue};
use crate::{DownloadCapability, DownloadKitReport};

const BINARY_OPCODE: u32 = 2;
const PROTO_FLAG: u32 = 0x8000_0000;
const SERVICE_METHOD_REQUEST: u32 = 151;
const SERVICE_METHOD_RESPONSE: u32 = 147;
const FRAME_HEADER_SIZE: usize = 8;
const MAX_PROTO_HEADER_SIZE: usize = 1024;
const MAX_BODY_SIZE: usize = 65_536;
const TARGET_JOB_NAME: &[u8] = b"ContentServerDirectory.GetManifestRequestCode#1";
const ERESULT_OK: u64 = 1;
const MAX_JOBS: usize = 64;
const JOB_TTL: Duration = Duration::from_secs(15);

static JOBS: OnceLock<Mutex<ManifestCodeJobTable>> = OnceLock::new();
static WORKER: OnceLock<SyncSender<ManifestCodeResolveWork>> = OnceLock::new();
static DEPOTS: OnceLock<RwLock<HashSet<DepotId>>> = OnceLock::new();
static CALLS: AtomicU64 = AtomicU64::new(0);
static SUBMITTED: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static COMPLETED: AtomicU64 = AtomicU64::new(0);
static PATCHED: AtomicU64 = AtomicU64::new(0);

/// 从发送帧提取的解析任务.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestCodeJob {
    pub job_id: u64,
    pub request: ManifestCodeRequest,
}

/// 防止同一 job ID 的旧 worker 结果写入新任务.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestCodeJobTicket {
    job_id: u64,
    generation: u64,
}

/// 发送 hook 只构造工作项; resolver 必须在后台消费.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestCodeResolveWork {
    pub request: ManifestCodeRequest,
    ticket: ManifestCodeJobTicket,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ManifestCodeDepotSnapshotReport {
    pub accepted: usize,
    pub rejected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestCodeRegister {
    Registered(ManifestCodeJobTicket),
    CapacityReached,
    InvalidJobId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestCodeCompletion {
    Completed,
    InvalidCode,
    UnknownOrStale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestCodeResponseRewrite {
    Passthrough,
    Rewritten { packet: Vec<u8>, job_id: u64 },
}

#[derive(Debug, Clone, Copy)]
struct JobEntry {
    generation: u64,
    inserted_at: Instant,
    request_code: Option<u64>,
}

/// 请求与后台结果之间的有界关联表.
pub struct ManifestCodeJobTable {
    entries: HashMap<u64, JobEntry>,
    max_entries: usize,
    ttl: Duration,
    next_generation: u64,
}

impl ManifestCodeJobTable {
    pub fn new(max_entries: usize, ttl: Duration) -> Option<Self> {
        if max_entries == 0 || ttl.is_zero() {
            return None;
        }
        Some(Self {
            entries: HashMap::with_capacity(max_entries),
            max_entries,
            ttl,
            next_generation: 0,
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 登记新任务. 同一 job ID 会换代, 旧 ticket 随即失效.
    pub fn register(&mut self, job_id: u64, now: Instant) -> ManifestCodeRegister {
        if valid_job_id(job_id).is_none() {
            return ManifestCodeRegister::InvalidJobId;
        }
        self.reap_expired(now);
        if !self.entries.contains_key(&job_id) && self.entries.len() >= self.max_entries {
            return ManifestCodeRegister::CapacityReached;
        }

        self.next_generation = self.next_generation.wrapping_add(1);
        if self.next_generation == 0 {
            self.next_generation = 1;
        }
        let ticket = ManifestCodeJobTicket {
            job_id,
            generation: self.next_generation,
        };
        self.entries.insert(
            job_id,
            JobEntry {
                generation: ticket.generation,
                inserted_at: now,
                request_code: None,
            },
        );
        ManifestCodeRegister::Registered(ticket)
    }

    /// 后台 resolver 只可完成仍存活且 generation 相同的任务.
    pub fn complete(
        &mut self,
        ticket: ManifestCodeJobTicket,
        request_code: u64,
        now: Instant,
    ) -> ManifestCodeCompletion {
        if request_code == 0 {
            return ManifestCodeCompletion::InvalidCode;
        }
        if self
            .entries
            .get(&ticket.job_id)
            .is_some_and(|entry| self.is_expired(*entry, now))
        {
            self.entries.remove(&ticket.job_id);
            return ManifestCodeCompletion::UnknownOrStale;
        }
        let Some(entry) = self.entries.get_mut(&ticket.job_id) else {
            return ManifestCodeCompletion::UnknownOrStale;
        };
        if entry.generation != ticket.generation {
            return ManifestCodeCompletion::UnknownOrStale;
        }
        entry.request_code = Some(request_code);
        ManifestCodeCompletion::Completed
    }

    /// resolver 失败后主动释放任务容量.
    pub fn cancel(&mut self, ticket: ManifestCodeJobTicket) -> bool {
        if self
            .entries
            .get(&ticket.job_id)
            .is_some_and(|entry| entry.generation == ticket.generation)
        {
            self.entries.remove(&ticket.job_id);
            true
        } else {
            false
        }
    }

    pub fn reap_expired(&mut self, now: Instant) -> usize {
        let before = self.entries.len();
        let ttl = self.ttl;
        self.entries
            .retain(|_, entry| now.saturating_duration_since(entry.inserted_at) < ttl);
        before - self.entries.len()
    }

    fn take_ready(&mut self, job_id: u64, now: Instant) -> Option<u64> {
        let entry = self.entries.remove(&job_id)?;
        (!self.is_expired(entry, now))
            .then_some(entry.request_code)
            .flatten()
    }

    fn is_expired(&self, entry: JobEntry, now: Instant) -> bool {
        now.saturating_duration_since(entry.inserted_at) >= self.ttl
    }
}

fn runtime_jobs() -> &'static Mutex<ManifestCodeJobTable> {
    JOBS.get_or_init(|| {
        Mutex::new(
            ManifestCodeJobTable::new(MAX_JOBS, JOB_TTL)
                .expect("request code job limits are nonzero"),
        )
    })
}

fn configured_depots() -> &'static RwLock<HashSet<DepotId>> {
    DEPOTS.get_or_init(|| RwLock::new(HashSet::new()))
}

/// 注册唯一的有界后台队列发送端.
pub fn register_manifest_code_worker(sender: SyncSender<ManifestCodeResolveWork>) -> bool {
    WORKER.set(sender).is_ok()
}

/// 替换 request-code 能力允许处理的 depot 快照.
pub fn replace_manifest_code_depots(
    depots: impl IntoIterator<Item = DepotId>,
) -> ManifestCodeDepotSnapshotReport {
    let mut accepted = HashSet::new();
    let mut report = ManifestCodeDepotSnapshotReport::default();
    for depot_id in depots {
        if depot_id == 0 {
            report.rejected += 1;
        } else if accepted.insert(depot_id) {
            report.accepted += 1;
        }
    }
    let mut guard = configured_depots()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = accepted;
    report
}

pub fn manifest_code_hook_stats() -> (u64, u64, u64, u64, u64) {
    (
        CALLS.load(Ordering::Relaxed),
        SUBMITTED.load(Ordering::Relaxed),
        DROPPED.load(Ordering::Relaxed),
        COMPLETED.load(Ordering::Relaxed),
        PATCHED.load(Ordering::Relaxed),
    )
}

pub fn is_manifest_code_send_hook_attached() -> bool {
    crate::net_send::is_consumer_attached(DownloadCapability::RequestCode)
}

/// 只安装共用发送入口; RecvPkt 验证并挂上前 host 不应调用.
pub fn try_install_manifest_code_send_hook(
    report: &mut DownloadKitReport,
    patterns: &PatternStore,
) {
    crate::net_send::try_install_consumer(
        report,
        patterns,
        DownloadCapability::RequestCode,
        "manifest request code send",
    );
}

/// 后台 worker 成功时写入完成态. code 正文不会进入诊断状态.
pub fn complete_manifest_code_work(
    work: ManifestCodeResolveWork,
    request_code: u64,
) -> ManifestCodeCompletion {
    let mut jobs = runtime_jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let outcome = jobs.complete(work.ticket, request_code, Instant::now());
    if outcome == ManifestCodeCompletion::Completed {
        COMPLETED.fetch_add(1, Ordering::Relaxed);
    }
    outcome
}

/// 后台 worker 失败时立即释放容量.
pub fn cancel_manifest_code_work(work: ManifestCodeResolveWork) -> bool {
    let mut jobs = runtime_jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    jobs.cancel(work.ticket)
}

pub(crate) fn submit_manifest_code_frame(opcode: u32, packet: &[u8]) {
    CALLS.fetch_add(1, Ordering::Relaxed);
    let Some(job) = inspect_manifest_code_request_frame(opcode, packet) else {
        return;
    };
    let Some(depot_id) = job.request.depot_id else {
        return;
    };
    if !configured_depots()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&depot_id)
    {
        return;
    }
    let Some(worker) = WORKER.get() else {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    };

    let mut jobs = runtime_jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ManifestCodeRegister::Registered(ticket) = jobs.register(job.job_id, Instant::now()) else {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let work = ManifestCodeResolveWork {
        request: job.request,
        ticket,
    };
    match worker.try_send(work) {
        Ok(()) => {
            SUBMITTED.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Full(work) | TrySendError::Disconnected(work)) => {
            jobs.cancel(work.ticket);
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub fn rewrite_manifest_code_runtime_response(
    opcode: u32,
    packet: &[u8],
) -> ManifestCodeResponseRewrite {
    let mut jobs = runtime_jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rewrite = rewrite_manifest_code_response_frame(opcode, packet, &mut jobs, Instant::now());
    if matches!(rewrite, ManifestCodeResponseRewrite::Rewritten { .. }) {
        PATCHED.fetch_add(1, Ordering::Relaxed);
    }
    rewrite
}

/// 只识别目标 EMsg 151, 不修改发送帧.
pub fn inspect_manifest_code_request_frame(opcode: u32, packet: &[u8]) -> Option<ManifestCodeJob> {
    let frame = unpack_frame(opcode, packet, SERVICE_METHOD_REQUEST)?;
    let header = parse_service_header(frame.header)?;
    if header.target_job_name != Some(TARGET_JOB_NAME) {
        return None;
    }
    let job_id = valid_job_id(header.job_id_source?)?;
    let request = parse_request(frame.body)?;
    Some(ManifestCodeJob { job_id, request })
}

/// 只消费已完成结果; pending、失败、过期和畸形响应完整透传.
pub fn rewrite_manifest_code_response_frame(
    opcode: u32,
    packet: &[u8],
    jobs: &mut ManifestCodeJobTable,
    now: Instant,
) -> ManifestCodeResponseRewrite {
    let Some(frame) = unpack_frame(opcode, packet, SERVICE_METHOD_RESPONSE) else {
        return ManifestCodeResponseRewrite::Passthrough;
    };
    let Some(header) = parse_service_header(frame.header) else {
        return ManifestCodeResponseRewrite::Passthrough;
    };
    if header.target_job_name != Some(TARGET_JOB_NAME) {
        return ManifestCodeResponseRewrite::Passthrough;
    }
    let Some(job_id) = header.job_id_target.and_then(valid_job_id) else {
        return ManifestCodeResponseRewrite::Passthrough;
    };

    let Some(rewritten_header) = replace_varint_field(frame.header, 13, ERESULT_OK) else {
        return ManifestCodeResponseRewrite::Passthrough;
    };
    if !validate_wire(frame.body) {
        return ManifestCodeResponseRewrite::Passthrough;
    }
    let Some(request_code) = jobs.take_ready(job_id, now) else {
        return ManifestCodeResponseRewrite::Passthrough;
    };
    let Some(rewritten_body) = replace_varint_field(frame.body, 1, request_code) else {
        return ManifestCodeResponseRewrite::Passthrough;
    };
    if rewritten_header.len() > MAX_PROTO_HEADER_SIZE || rewritten_body.len() > MAX_BODY_SIZE {
        return ManifestCodeResponseRewrite::Passthrough;
    }

    let Some(new_size) = FRAME_HEADER_SIZE
        .checked_add(rewritten_header.len())
        .and_then(|size| size.checked_add(rewritten_body.len()))
    else {
        return ManifestCodeResponseRewrite::Passthrough;
    };
    let mut rewritten = Vec::with_capacity(new_size);
    rewritten.extend_from_slice(&(PROTO_FLAG | SERVICE_METHOD_RESPONSE).to_le_bytes());
    rewritten.extend_from_slice(&(rewritten_header.len() as u32).to_le_bytes());
    rewritten.extend_from_slice(&rewritten_header);
    rewritten.extend_from_slice(&rewritten_body);
    ManifestCodeResponseRewrite::Rewritten {
        packet: rewritten,
        job_id,
    }
}

#[derive(Debug, Clone, Copy)]
struct ProtoFrame<'a> {
    header: &'a [u8],
    body: &'a [u8],
}

fn unpack_frame(opcode: u32, packet: &[u8], expected_message: u32) -> Option<ProtoFrame<'_>> {
    if opcode != BINARY_OPCODE || packet.len() < FRAME_HEADER_SIZE {
        return None;
    }
    let message = read_u32_le(packet, 0)?;
    if message & PROTO_FLAG == 0 || message & !PROTO_FLAG != expected_message {
        return None;
    }
    let header_size = usize::try_from(read_u32_le(packet, 4)?).ok()?;
    if header_size > MAX_PROTO_HEADER_SIZE {
        return None;
    }
    let body_offset = FRAME_HEADER_SIZE.checked_add(header_size)?;
    if body_offset > packet.len() || packet.len() - body_offset > MAX_BODY_SIZE {
        return None;
    }
    Some(ProtoFrame {
        header: &packet[FRAME_HEADER_SIZE..body_offset],
        body: &packet[body_offset..],
    })
}

#[derive(Default)]
struct ServiceHeader<'a> {
    job_id_source: Option<u64>,
    job_id_target: Option<u64>,
    target_job_name: Option<&'a [u8]>,
}

fn parse_service_header(header: &[u8]) -> Option<ServiceHeader<'_>> {
    let mut parsed = ServiceHeader::default();
    let mut cursor = 0;
    while cursor < header.len() {
        let field = parse_field(header, &mut cursor)?;
        match (field.number, field.wire_type, field.value) {
            (10, 1, _) => parsed.job_id_source = read_fixed_u64(header, field.tag_end),
            (11, 1, _) => parsed.job_id_target = read_fixed_u64(header, field.tag_end),
            (12, 2, WireValue::Bytes(value)) => parsed.target_job_name = Some(value),
            _ => {}
        }
    }
    Some(parsed)
}

fn parse_request(body: &[u8]) -> Option<ManifestCodeRequest> {
    let mut app_id = None;
    let mut depot_id = None;
    let mut manifest_gid = None;
    let mut cursor = 0;
    while cursor < body.len() {
        let field = parse_field(body, &mut cursor)?;
        match (field.number, field.wire_type, field.value) {
            (1, 0, WireValue::Varint(value)) => {
                let value = u32::try_from(value).ok()?;
                app_id = (value != 0).then_some(value);
            }
            (2, 0, WireValue::Varint(value)) => depot_id = u32::try_from(value).ok(),
            (3, 0, WireValue::Varint(value)) => manifest_gid = Some(value),
            _ => {}
        }
    }
    Some(ManifestCodeRequest {
        app_id,
        depot_id: depot_id.filter(|value| *value != 0),
        manifest_gid: manifest_gid.filter(|value| *value != 0)?,
    })
    .filter(|request| request.depot_id.is_some())
}

fn replace_varint_field(input: &[u8], number: u32, value: u64) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len() + 11);
    let mut cursor = 0;
    while cursor < input.len() {
        let field = parse_field(input, &mut cursor)?;
        if field.number != number || field.wire_type != 0 {
            output.extend_from_slice(&input[field.start..field.end]);
        }
    }
    encode_varint(u64::from(number) << 3, &mut output);
    encode_varint(value, &mut output);
    Some(output)
}

fn validate_wire(input: &[u8]) -> bool {
    let mut cursor = 0;
    while cursor < input.len() {
        if parse_field(input, &mut cursor).is_none() {
            return false;
        }
    }
    true
}

fn valid_job_id(job_id: u64) -> Option<u64> {
    (job_id != 0 && job_id != u64::MAX).then_some(job_id)
}

fn read_u32_le(input: &[u8], offset: usize) -> Option<u32> {
    let bytes = input.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn read_fixed_u64(input: &[u8], offset: usize) -> Option<u64> {
    let bytes = input.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn field_varint(number: u32, value: u64) -> Vec<u8> {
        let mut output = Vec::new();
        encode_varint(u64::from(number) << 3, &mut output);
        encode_varint(value, &mut output);
        output
    }

    fn field_fixed64(number: u32, value: u64) -> Vec<u8> {
        let mut output = Vec::new();
        encode_varint((u64::from(number) << 3) | 1, &mut output);
        output.extend_from_slice(&value.to_le_bytes());
        output
    }

    fn field_bytes(number: u32, value: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        encode_varint((u64::from(number) << 3) | 2, &mut output);
        encode_varint(value.len() as u64, &mut output);
        output.extend_from_slice(value);
        output
    }

    fn service_header(job_field: u32, job_id: u64, eresult: Option<u64>) -> Vec<u8> {
        let mut header = field_varint(3, 7);
        header.extend(field_fixed64(job_field, job_id));
        header.extend(field_bytes(12, TARGET_JOB_NAME));
        if let Some(eresult) = eresult {
            header.extend(field_varint(13, eresult));
        }
        header
    }

    fn frame(message: u32, header: &[u8], body: &[u8]) -> Vec<u8> {
        let mut packet = Vec::new();
        packet.extend_from_slice(&(PROTO_FLAG | message).to_le_bytes());
        packet.extend_from_slice(&(header.len() as u32).to_le_bytes());
        packet.extend_from_slice(header);
        packet.extend_from_slice(body);
        packet
    }

    fn request_frame(job_id: u64) -> Vec<u8> {
        let mut body = field_varint(1, 10);
        body.extend(field_varint(2, 20));
        body.extend(field_varint(3, 30));
        body.extend(field_bytes(9, b"unknown"));
        frame(
            SERVICE_METHOD_REQUEST,
            &service_header(10, job_id, None),
            &body,
        )
    }

    #[test]
    fn extracts_target_request_and_job_source() {
        let job = inspect_manifest_code_request_frame(2, &request_frame(40)).unwrap();

        assert_eq!(job.job_id, 40);
        assert_eq!(
            job.request,
            ManifestCodeRequest {
                app_id: Some(10),
                depot_id: Some(20),
                manifest_gid: 30,
            }
        );
    }

    #[test]
    fn rejects_unrelated_or_malformed_request_frames() {
        let header = service_header(10, 40, None);
        let missing_manifest = field_varint(2, 20);
        let mut wrong_target = field_fixed64(10, 40);
        wrong_target.extend(field_bytes(12, b"Other.Method#1"));

        assert!(inspect_manifest_code_request_frame(1, &request_frame(40)).is_none());
        assert!(inspect_manifest_code_request_frame(
            2,
            &frame(SERVICE_METHOD_RESPONSE, &header, &[])
        )
        .is_none());
        assert!(inspect_manifest_code_request_frame(
            2,
            &frame(SERVICE_METHOD_REQUEST, &wrong_target, &[])
        )
        .is_none());
        assert!(inspect_manifest_code_request_frame(
            2,
            &frame(SERVICE_METHOD_REQUEST, &header, &missing_manifest)
        )
        .is_none());
        assert!(inspect_manifest_code_request_frame(2, &[0; 7]).is_none());
    }

    #[test]
    fn job_table_is_bounded_and_reaps_expired_entries() {
        let start = Instant::now();
        let mut jobs = ManifestCodeJobTable::new(1, Duration::from_secs(5)).unwrap();
        let first = jobs.register(1, start);

        assert!(matches!(first, ManifestCodeRegister::Registered(_)));
        assert_eq!(
            jobs.register(2, start),
            ManifestCodeRegister::CapacityReached
        );
        assert_eq!(jobs.reap_expired(start + Duration::from_secs(5)), 1);
        assert!(matches!(
            jobs.register(2, start + Duration::from_secs(5)),
            ManifestCodeRegister::Registered(_)
        ));
    }

    #[test]
    fn job_table_rejects_invalid_ids_and_cancels_matching_ticket() {
        let now = Instant::now();
        let mut jobs = ManifestCodeJobTable::new(1, Duration::from_secs(5)).unwrap();

        assert_eq!(jobs.register(0, now), ManifestCodeRegister::InvalidJobId);
        assert_eq!(
            jobs.register(u64::MAX, now),
            ManifestCodeRegister::InvalidJobId
        );
        let ManifestCodeRegister::Registered(ticket) = jobs.register(1, now) else {
            unreachable!();
        };
        assert_eq!(
            jobs.complete(ticket, 0, now),
            ManifestCodeCompletion::InvalidCode
        );
        assert!(jobs.cancel(ticket));
        assert!(!jobs.cancel(ticket));
        assert!(jobs.is_empty());
    }

    #[test]
    fn replaced_job_rejects_old_worker_result() {
        let now = Instant::now();
        let mut jobs = ManifestCodeJobTable::new(2, Duration::from_secs(5)).unwrap();
        let ManifestCodeRegister::Registered(old) = jobs.register(1, now) else {
            unreachable!();
        };
        let ManifestCodeRegister::Registered(new) = jobs.register(1, now) else {
            unreachable!();
        };

        assert_eq!(
            jobs.complete(old, 10, now),
            ManifestCodeCompletion::UnknownOrStale
        );
        assert_eq!(
            jobs.complete(new, 11, now),
            ManifestCodeCompletion::Completed
        );
    }

    #[test]
    fn completed_response_replaces_only_target_fields() {
        let now = Instant::now();
        let mut jobs = ManifestCodeJobTable::new(2, Duration::from_secs(5)).unwrap();
        let ManifestCodeRegister::Registered(ticket) = jobs.register(40, now) else {
            unreachable!();
        };
        assert_eq!(
            jobs.complete(ticket, 99, now),
            ManifestCodeCompletion::Completed
        );

        let header = service_header(11, 40, Some(2));
        let mut body = field_varint(1, 5);
        body.extend(field_bytes(9, b"unknown"));
        let packet = frame(SERVICE_METHOD_RESPONSE, &header, &body);
        let result = rewrite_manifest_code_response_frame(2, &packet, &mut jobs, now);

        let mut expected_header = field_varint(3, 7);
        expected_header.extend(field_fixed64(11, 40));
        expected_header.extend(field_bytes(12, TARGET_JOB_NAME));
        expected_header.extend(field_varint(13, 1));
        let mut expected_body = field_bytes(9, b"unknown");
        expected_body.extend(field_varint(1, 99));
        assert_eq!(
            result,
            ManifestCodeResponseRewrite::Rewritten {
                packet: frame(SERVICE_METHOD_RESPONSE, &expected_header, &expected_body),
                job_id: 40,
            }
        );
        assert!(jobs.is_empty());
    }

    #[test]
    fn pending_or_expired_result_preserves_steam_response() {
        let now = Instant::now();
        let mut jobs = ManifestCodeJobTable::new(2, Duration::from_secs(5)).unwrap();
        let _ = jobs.register(40, now);
        let packet = frame(
            SERVICE_METHOD_RESPONSE,
            &service_header(11, 40, Some(2)),
            &field_varint(1, 5),
        );

        assert_eq!(
            rewrite_manifest_code_response_frame(2, &packet, &mut jobs, now),
            ManifestCodeResponseRewrite::Passthrough
        );
        assert!(jobs.is_empty());

        let ManifestCodeRegister::Registered(ticket) = jobs.register(40, now) else {
            unreachable!();
        };
        assert_eq!(
            jobs.complete(ticket, 99, now + Duration::from_secs(5)),
            ManifestCodeCompletion::UnknownOrStale
        );
    }

    #[test]
    fn invalid_response_does_not_consume_ready_result() {
        let now = Instant::now();
        let mut jobs = ManifestCodeJobTable::new(2, Duration::from_secs(5)).unwrap();
        let ManifestCodeRegister::Registered(ticket) = jobs.register(40, now) else {
            unreachable!();
        };
        assert_eq!(
            jobs.complete(ticket, 99, now),
            ManifestCodeCompletion::Completed
        );
        let malformed_body = [0x08, 0x80];
        let packet = frame(
            SERVICE_METHOD_RESPONSE,
            &service_header(11, 40, Some(2)),
            &malformed_body,
        );

        assert_eq!(
            rewrite_manifest_code_response_frame(2, &packet, &mut jobs, now),
            ManifestCodeResponseRewrite::Passthrough
        );
        assert_eq!(jobs.len(), 1);
    }

    #[test]
    fn runtime_submits_only_configured_depot_and_accepts_worker_result() {
        let (sender, receiver) = mpsc::sync_channel(1);
        assert!(register_manifest_code_worker(sender));
        assert_eq!(
            replace_manifest_code_depots([0, 20, 20]),
            ManifestCodeDepotSnapshotReport {
                accepted: 1,
                rejected: 1,
            }
        );

        submit_manifest_code_frame(2, &request_frame(40));
        let work = receiver.recv().unwrap();
        assert_eq!(work.request.depot_id, Some(20));
        assert_eq!(
            complete_manifest_code_work(work, 99),
            ManifestCodeCompletion::Completed
        );

        let response = frame(
            SERVICE_METHOD_RESPONSE,
            &service_header(11, 40, Some(2)),
            &field_varint(1, 5),
        );
        assert!(matches!(
            rewrite_manifest_code_runtime_response(2, &response),
            ManifestCodeResponseRewrite::Rewritten { job_id: 40, .. }
        ));
    }
}
