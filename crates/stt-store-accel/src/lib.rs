//! 受限 Steam 网页访问运行时: PAC, CONNECT 和固定 IP DNS 解析.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream,
    UdpSocket,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use serde::{Deserialize, Serialize};
use stt_config::{HostConfig, StoreAccelEgress, StoreAccelSection};
use thiserror::Error;
use webpki_roots::TLS_SERVER_ROOTS;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::Networking::WinInet::{
    InternetSetOptionW, INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE,
};

const SETTINGS_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings";
const SETTINGS_VALUES: [&str; 4] = ["AutoConfigURL", "AutoDetect", "ProxyEnable", "ProxyServer"];
const PAC_PATH: &str = "/steamtools-store-accel.pac";
const STOP_PATH: &str = "/__steamtools__/stop";
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_CONNECT_PRELUDE_BYTES: usize = 64 * 1024;
const MAX_DNS_RESPONSE_BYTES: usize = 4 * 1024;
const DNS_CACHE_TTL_CAP: Duration = Duration::from_secs(300);
const CANDIDATE_COOLDOWN: Duration = Duration::from_secs(30);
const CANDIDATE_HEALTH_TTL: Duration = Duration::from_secs(60);
const MAX_DNS_CANDIDATES: usize = 8;
const MAX_DNS_CACHE_ENTRIES: usize = 512;
const MAX_HEALTH_ENTRIES: usize = 1_024;
const MAX_PROBE_HEADER_BYTES: usize = 8 * 1024;
const LOOP_SLEEP: Duration = Duration::from_millis(20);
static DNS_QUERY_ID: AtomicU16 = AtomicU16::new(1);

#[derive(Debug, Error)]
pub enum StoreAccelError {
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    #[error("配置: {0}")]
    Config(#[from] stt_config::ConfigError),
    #[error("快照: {0}")]
    Snapshot(#[from] serde_json::Error),
    #[error("无效请求: {0}")]
    InvalidRequest(String),
    #[error("DNS: {0}")]
    Dns(String),
    #[error("TLS 探测: {0}")]
    Tls(String),
    #[error("系统 PAC: {0}")]
    SystemProxy(String),
}

pub type Result<T> = std::result::Result<T, StoreAccelError>;

#[derive(Debug, Clone)]
struct HelperConfig {
    listen_port: u16,
    egress: EgressConfig,
    dns_timeout: Duration,
    connect_timeout: Duration,
    max_connections: usize,
}

#[derive(Debug, Clone, Copy)]
enum EgressConfig {
    DirectDns(SocketAddrV4),
    LocalCdn {
        resolver: SocketAddrV4,
        clash_fallback: Option<SocketAddrV4>,
    },
    HttpConnect(SocketAddrV4),
}

impl TryFrom<&StoreAccelSection> for HelperConfig {
    type Error = StoreAccelError;

    fn try_from(value: &StoreAccelSection) -> Result<Self> {
        value.validate()?;
        let egress = match value.egress {
            StoreAccelEgress::Disabled => {
                return Err(StoreAccelError::InvalidRequest(
                    "store_accel.egress 未配置".into(),
                ));
            }
            StoreAccelEgress::DirectDns => {
                EgressConfig::DirectDns(value.resolver.parse::<SocketAddrV4>().map_err(|_| {
                    StoreAccelError::InvalidRequest("store_accel.resolver 不是 IPv4:port".into())
                })?)
            }
            StoreAccelEgress::LocalCdn => EgressConfig::LocalCdn {
                resolver: value.resolver.parse::<SocketAddrV4>().map_err(|_| {
                    StoreAccelError::InvalidRequest("store_accel.resolver 不是 IPv4:port".into())
                })?,
                clash_fallback: (!value.clash_fallback.is_empty())
                    .then(|| {
                        value.clash_fallback.parse::<SocketAddrV4>().map_err(|_| {
                            StoreAccelError::InvalidRequest(
                                "store_accel.clash_fallback 不是 IPv4:port".into(),
                            )
                        })
                    })
                    .transpose()?,
            },
            StoreAccelEgress::HttpConnect => {
                EgressConfig::HttpConnect(value.upstream.parse::<SocketAddrV4>().map_err(|_| {
                    StoreAccelError::InvalidRequest("store_accel.upstream 不是 IPv4:port".into())
                })?)
            }
        };
        Ok(Self {
            listen_port: value.listen_port,
            egress,
            dns_timeout: Duration::from_millis(u64::from(value.dns_timeout_ms)),
            connect_timeout: Duration::from_millis(u64::from(value.connect_timeout_ms)),
            max_connections: value.max_connections,
        })
    }
}

/// 在线程中运行 DLL 内的商店访问代理. 监听成功后才改 Windows PAC 设置.
pub fn run(steam_root: &Path, stop: Arc<AtomicBool>) -> Result<()> {
    let host = HostConfig::load_from_steam_root(steam_root)?;
    let config = HelperConfig::try_from(&host.store_accel)?;
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, config.listen_port))?;
    listener.set_nonblocking(true)?;

    let pac_url = format!("http://127.0.0.1:{}{PAC_PATH}", config.listen_port);
    let snapshot_path = steam_root
        .join("steamtools")
        .join("store-accel-system-proxy.json");
    let mut system_proxy = SystemProxyGuard::install(&snapshot_path, pac_url)?;
    let serve_result = serve(listener, config, stop, steam_root);
    let restore_result = system_proxy.restore();
    match (serve_result, restore_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// 向当前 DLL 内运行时发送本机停止请求. 失败不会改任何系统设置.
pub fn request_stop(port: u16) -> Result<()> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(
        format!(
            "POST {STOP_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    )?;
    let mut response = [0_u8; 128];
    let read = stream.read(&mut response)?;
    let text = std::str::from_utf8(&response[..read]).unwrap_or_default();
    if text.starts_with("HTTP/1.1 204") {
        Ok(())
    } else {
        Err(StoreAccelError::InvalidRequest(
            "helper 未确认停止请求".into(),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostClass {
    StoreDynamic,
    CommunityDynamic,
    StoreStatic,
    WorkshopStatic,
}

impl HostClass {
    const fn label(self) -> &'static str {
        match self {
            Self::StoreDynamic => "store_dynamic",
            Self::CommunityDynamic => "community_dynamic",
            Self::StoreStatic => "store_static",
            Self::WorkshopStatic => "workshop_static",
        }
    }
}

fn classify_host(host: &str) -> Option<HostClass> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if matches!(
        host.as_str(),
        "steamcommunity-a.akamaihd.net"
            | "steamuserimages-a.akamaihd.net"
            | "steamusercontent-a.akamaihd.net"
    ) {
        return Some(HostClass::WorkshopStatic);
    }
    if matches!(
        host.as_str(),
        "steamcdn-a.akamaihd.net" | "steamstore-a.akamaihd.net" | "steamvideo-a.akamaihd.net"
    ) {
        return Some(HostClass::StoreStatic);
    }
    if host == "steamcommunity.com" || host.ends_with(".steamcommunity.com") {
        return Some(HostClass::CommunityDynamic);
    }
    if host == "steampowered.com" || host.ends_with(".steampowered.com") {
        return Some(HostClass::StoreDynamic);
    }
    if host == "steamstatic.com" || host.ends_with(".steamstatic.com") {
        return Some(HostClass::StoreStatic);
    }
    if host == "steamusercontent.com" || host.ends_with(".steamusercontent.com") {
        return Some(HostClass::WorkshopStatic);
    }
    None
}

/// PAC 只把已验证的 Steam 网页域名交给本机运行时.
pub fn pac_script(port: u16) -> String {
    format!(
        "function FindProxyForURL(url, host) {{\n\
         host = host.toLowerCase();\n\
         if (dnsDomainIs(host, '.steampowered.com') || host == 'steampowered.com' ||\n\
             dnsDomainIs(host, '.steamcommunity.com') || host == 'steamcommunity.com' ||\n\
             dnsDomainIs(host, '.steamstatic.com') || host == 'steamstatic.com' ||\n\
             dnsDomainIs(host, '.steamusercontent.com') || host == 'steamusercontent.com' ||\n\
             host == 'steamcommunity-a.akamaihd.net' ||\n\
             host == 'steamuserimages-a.akamaihd.net' ||\n\
             host == 'steamusercontent-a.akamaihd.net' ||\n\
             host == 'steamcdn-a.akamaihd.net' ||\n\
             host == 'steamstore-a.akamaihd.net' ||\n\
             host == 'steamvideo-a.akamaihd.net') {{\n\
             return 'PROXY 127.0.0.1:{port}; DIRECT';\n\
         }}\n\
         return 'DIRECT';\n\
         }}\n"
    )
}

/// 和 PAC 相同的收敛规则, 防止手工连接把运行时变成通用代理.
pub fn is_allowed_host(host: &str) -> bool {
    classify_host(host).is_some()
}

fn serve(
    listener: TcpListener,
    config: HelperConfig,
    stop: Arc<AtomicBool>,
    steam_root: &Path,
) -> Result<()> {
    let state = Arc::new(ServerState {
        egress: Egress::from_config(config.egress, config.dns_timeout),
        connect_timeout: config.connect_timeout,
        max_connections: config.max_connections,
        active_connections: Arc::new(AtomicUsize::new(0)),
        running: AtomicBool::new(true),
        stop,
        request_logger: RequestLogger::new(steam_root),
    });

    while state.running.load(Ordering::Acquire) && !state.stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let Some(permit) = ConnectionPermit::try_acquire(&state) else {
                    let _ = write_response(stream, "503 Service Unavailable", "busy", "text/plain");
                    continue;
                };
                let state = Arc::clone(&state);
                let spawn = thread::Builder::new()
                    .name("stt-store-accel-conn".into())
                    .spawn(move || {
                        let _permit = permit;
                        if let Err(error) = handle_connection(stream, &state) {
                            eprintln!("store_accel connection_error={error}");
                        }
                    });
                let _ = spawn;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(LOOP_SLEEP),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

struct ServerState {
    egress: Egress,
    connect_timeout: Duration,
    max_connections: usize,
    active_connections: Arc<AtomicUsize>,
    running: AtomicBool,
    stop: Arc<AtomicBool>,
    request_logger: RequestLogger,
}

const REQUEST_LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
struct RequestLogger {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl RequestLogger {
    fn new(steam_root: &Path) -> Self {
        Self {
            path: steam_root.join("steamtools").join("store-accel.log"),
            lock: Arc::new(Mutex::new(())),
        }
    }

    fn connection(
        &self,
        endpoint: &Endpoint,
        route: &'static str,
        result: &'static str,
        stage: &'static str,
    ) {
        let Some(class) = classify_host(&endpoint.host) else {
            return;
        };
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let line = format!(
            "ts_unix_ms={timestamp} event=proxy_request host={} class={} route={route} result={result} stage={stage}",
            endpoint.host,
            class.label(),
        );
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if fs::metadata(&self.path).is_ok_and(|metadata| metadata.len() >= REQUEST_LOG_MAX_BYTES) {
            let rotated = self.path.with_extension("log.1");
            let _ = fs::remove_file(&rotated);
            let _ = fs::rename(&self.path, rotated);
        }
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    LocalDirect,
    ClashFallback,
    UserConnect,
}

impl Route {
    const fn label(self) -> &'static str {
        match self {
            Self::LocalDirect => "local_direct",
            Self::ClashFallback => "clash_fallback",
            Self::UserConnect => "user_connect",
        }
    }
}

struct ConnectedEndpoint {
    stream: TcpStream,
    route: Route,
    candidate: Option<IpAddr>,
}

#[derive(Debug)]
enum Egress {
    DirectDns(DnsResolver),
    LocalCdn {
        resolver: DnsResolver,
        clash_fallback: Option<SocketAddrV4>,
    },
    HttpConnect(SocketAddrV4),
}

impl Egress {
    fn from_config(config: EgressConfig, dns_timeout: Duration) -> Self {
        match config {
            EgressConfig::DirectDns(resolver) => {
                Self::DirectDns(DnsResolver::new(resolver, dns_timeout))
            }
            EgressConfig::LocalCdn {
                resolver,
                clash_fallback,
            } => Self::LocalCdn {
                resolver: DnsResolver::new(resolver, dns_timeout),
                clash_fallback,
            },
            EgressConfig::HttpConnect(upstream) => Self::HttpConnect(upstream),
        }
    }
}

struct ConnectionPermit {
    active_connections: Arc<AtomicUsize>,
}

impl ConnectionPermit {
    fn try_acquire(state: &Arc<ServerState>) -> Option<Self> {
        let active = &state.active_connections;
        loop {
            let current = active.load(Ordering::Acquire);
            if current >= state.max_connections {
                return None;
            }
            if active
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(Self {
                    active_connections: Arc::clone(&state.active_connections),
                });
            }
        }
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.active_connections.fetch_sub(1, Ordering::AcqRel);
    }
}

fn handle_connection(mut client: TcpStream, state: &Arc<ServerState>) -> Result<()> {
    // TcpListener 的非阻塞标志会传给 accept 的 socket; 隧道复制必须阻塞等待 TLS 数据.
    client.set_nonblocking(false)?;
    client.set_read_timeout(Some(Duration::from_secs(15)))?;
    let header = read_header(&mut client)?;
    match parse_request(&header)? {
        ProxyRequest::Pac => {
            let pac = pac_script(local_port(&client)?);
            write_response(client, "200 OK", &pac, "application/x-ns-proxy-autoconfig")
        }
        ProxyRequest::Stop => {
            state.running.store(false, Ordering::Release);
            write_response(client, "204 No Content", "", "text/plain")
        }
        ProxyRequest::Connect(endpoint) => {
            let mut connected = match connect_endpoint(state, &endpoint) {
                Ok(connected) => connected,
                Err(error) => {
                    state.request_logger.connection(
                        &endpoint,
                        "degraded",
                        "failed",
                        error_stage(&error),
                    );
                    return Err(error);
                }
            };
            state.request_logger.connection(
                &endpoint,
                connected.route.label(),
                "connected",
                "connect",
            );
            client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
            client.set_read_timeout(Some(state.connect_timeout))?;
            let result = match prime_connect_tunnel(&mut client, &mut connected, &endpoint, state) {
                Ok(()) => {
                    client.set_read_timeout(None)?;
                    connected.stream.set_read_timeout(None)?;
                    connected.stream.set_write_timeout(None)?;
                    tunnel(client, &mut connected.stream)
                }
                Err(error) => Err(error),
            };
            if let Err(error) = &result {
                state.request_logger.connection(
                    &endpoint,
                    connected.route.label(),
                    "failed",
                    "tunnel",
                );
                let _ = error;
            }
            result
        }
        ProxyRequest::Http(forward) => {
            let mut connected = match connect_endpoint(state, &forward.endpoint) {
                Ok(connected) => connected,
                Err(error) => {
                    state.request_logger.connection(
                        &forward.endpoint,
                        "degraded",
                        "failed",
                        error_stage(&error),
                    );
                    return Err(error);
                }
            };
            if let Err(error) = connected.stream.write_all(&forward.header) {
                state.request_logger.connection(
                    &forward.endpoint,
                    connected.route.label(),
                    "failed",
                    "forward_headers",
                );
                return Err(error.into());
            }
            state.request_logger.connection(
                &forward.endpoint,
                connected.route.label(),
                "connected",
                "connect",
            );
            client.set_read_timeout(None)?;
            let result = tunnel(client, &mut connected.stream);
            if let Err(error) = &result {
                state.request_logger.connection(
                    &forward.endpoint,
                    connected.route.label(),
                    "failed",
                    "tunnel",
                );
                let _ = error;
            }
            result
        }
    }
}

fn local_port(stream: &TcpStream) -> Result<u16> {
    Ok(stream.local_addr()?.port())
}

fn connect_endpoint(state: &ServerState, endpoint: &Endpoint) -> Result<ConnectedEndpoint> {
    let Some(_class) = classify_host(&endpoint.host) else {
        return Err(StoreAccelError::InvalidRequest(
            "目标不在 Steam allowlist".into(),
        ));
    };
    match &state.egress {
        Egress::DirectDns(resolver) => {
            let addresses = resolver.resolve(&endpoint.host)?;
            connect_raw_candidates(addresses, endpoint.port, state.connect_timeout).map(|stream| {
                ConnectedEndpoint {
                    stream,
                    route: Route::LocalDirect,
                    candidate: None,
                }
            })
        }
        Egress::LocalCdn {
            resolver,
            clash_fallback,
        } => match connect_local_candidates(resolver, endpoint, state.connect_timeout) {
            Ok((stream, candidate)) => Ok(ConnectedEndpoint {
                stream,
                route: Route::LocalDirect,
                candidate: Some(candidate),
            }),
            Err(local_error) => {
                let Some(clash_fallback) = clash_fallback else {
                    return Err(local_error);
                };
                match connect_via_http_upstream(*clash_fallback, state.connect_timeout, endpoint) {
                    Ok(stream) => Ok(ConnectedEndpoint {
                        stream,
                        route: Route::ClashFallback,
                        candidate: None,
                    }),
                    Err(clash_error) => Err(StoreAccelError::InvalidRequest(format!(
                        "本地候选与 Clash 回退均失败: {clash_error}"
                    ))),
                }
            }
        },
        Egress::HttpConnect(upstream) => {
            connect_via_http_upstream(*upstream, state.connect_timeout, endpoint).map(|stream| {
                ConnectedEndpoint {
                    stream,
                    route: Route::UserConnect,
                    candidate: None,
                }
            })
        }
    }
}

const fn error_stage(error: &StoreAccelError) -> &'static str {
    match error {
        StoreAccelError::Io(_) => "tcp",
        StoreAccelError::Config(_) => "config",
        StoreAccelError::Snapshot(_) => "snapshot",
        StoreAccelError::InvalidRequest(_) => "proxy_or_egress",
        StoreAccelError::Dns(_) => "dns",
        StoreAccelError::Tls(_) => "tls_probe",
        StoreAccelError::SystemProxy(_) => "system_proxy",
    }
}

fn connect_raw_candidates(
    addresses: Vec<IpAddr>,
    port: u16,
    timeout: Duration,
) -> Result<TcpStream> {
    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&SocketAddr::new(address, port), timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .map(StoreAccelError::Io)
        .unwrap_or_else(|| StoreAccelError::Dns("解析结果为空".into())))
}

fn connect_local_candidates(
    resolver: &DnsResolver,
    endpoint: &Endpoint,
    timeout: Duration,
) -> Result<(TcpStream, IpAddr)> {
    let addresses = resolver
        .resolve(&endpoint.host)?
        .into_iter()
        .filter(|address| !resolver.is_cooling_down(&endpoint.host, *address))
        .collect::<Vec<_>>();
    let mut candidates = Vec::with_capacity(addresses.len());
    let mut last_error = None;
    for address in addresses {
        if endpoint.port == 443 && resolver.should_probe(&endpoint.host, address) {
            let started = Instant::now();
            match probe_https_candidate(address, &endpoint.host, timeout) {
                Ok(()) => {
                    let latency = started.elapsed();
                    resolver.mark_healthy_with_latency(&endpoint.host, address, latency);
                    candidates.push((address, Some(latency)));
                }
                Err(error) => {
                    resolver.mark_failed(&endpoint.host, address);
                    last_error = Some(error);
                    continue;
                }
            }
        } else {
            candidates.push((address, resolver.candidate_latency(&endpoint.host, address)));
        }
    }
    candidates.sort_by_key(|(_, latency)| latency.unwrap_or(Duration::MAX));
    for (address, _) in candidates {
        match TcpStream::connect_timeout(&SocketAddr::new(address, endpoint.port), timeout) {
            Ok(stream) => {
                resolver.mark_healthy(&endpoint.host, address);
                return Ok((stream, address));
            }
            Err(error) => {
                resolver.mark_failed(&endpoint.host, address);
                last_error = Some(StoreAccelError::Io(error));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| StoreAccelError::Dns("没有可用的候选地址".into())))
}

fn connect_via_http_upstream(
    upstream: SocketAddrV4,
    timeout: Duration,
    endpoint: &Endpoint,
) -> Result<TcpStream> {
    let mut stream = TcpStream::connect_timeout(&SocketAddr::V4(upstream), timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(
        format!(
            "CONNECT {}:{} HTTP/1.1\r\nHost: {}:{}\r\nProxy-Connection: keep-alive\r\n\r\n",
            endpoint.host, endpoint.port, endpoint.host, endpoint.port
        )
        .as_bytes(),
    )?;
    let response = read_header(&mut stream)?;
    if !is_successful_connect_response(&response) {
        return Err(StoreAccelError::InvalidRequest(
            "上游 HTTP CONNECT 未接受目标".into(),
        ));
    }
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    Ok(stream)
}

fn clash_fallback_address(state: &ServerState) -> Option<SocketAddrV4> {
    match &state.egress {
        Egress::LocalCdn {
            clash_fallback: Some(address),
            ..
        } => Some(*address),
        _ => None,
    }
}

/// CONNECT 已经向客户端确认后, 先验证首个 TLS 数据能得到上游响应.
/// 本地 TCP 成功但目标边缘黑洞时, 将未解析的首包重放到用户 Clash.
fn prime_connect_tunnel(
    client: &mut TcpStream,
    connected: &mut ConnectedEndpoint,
    endpoint: &Endpoint,
    state: &ServerState,
) -> Result<()> {
    let prelude = read_connect_prelude(client)?;
    match prime_upstream(&mut connected.stream, &prelude, state.connect_timeout) {
        Ok(response) => {
            client.write_all(&response)?;
            Ok(())
        }
        Err(local_error) if connected.route == Route::LocalDirect => {
            if let (Egress::LocalCdn { resolver, .. }, Some(candidate)) =
                (&state.egress, connected.candidate)
            {
                resolver.mark_failed(&endpoint.host, candidate);
            }
            if let Some(upstream) = clash_fallback_address(state) {
                let mut fallback = ConnectedEndpoint {
                    stream: connect_via_http_upstream(upstream, state.connect_timeout, endpoint)
                        .map_err(|error| {
                            StoreAccelError::InvalidRequest(format!(
                                "本地 tunnel 与 Clash 回退均失败: {local_error}; {error}"
                            ))
                        })?,
                    route: Route::ClashFallback,
                    candidate: None,
                };
                let response =
                    prime_upstream(&mut fallback.stream, &prelude, state.connect_timeout).map_err(
                        |error| {
                            StoreAccelError::InvalidRequest(format!(
                                "本地 tunnel 与 Clash 回退均失败: {local_error}; {error}"
                            ))
                        },
                    )?;
                client.write_all(&response)?;
                state.request_logger.connection(
                    endpoint,
                    fallback.route.label(),
                    "connected",
                    "tunnel_fallback",
                );
                *connected = fallback;
                return Ok(());
            }
            Err(local_error)
        }
        Err(error) => Err(error),
    }
}

fn read_connect_prelude(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut header = [0_u8; 5];
    stream.read_exact(&mut header)?;
    let length = if matches!(header[0], 0x14..=0x17) {
        usize::from(u16::from_be_bytes([header[3], header[4]]))
    } else {
        0
    };
    let total = 5_usize
        .checked_add(length)
        .filter(|total| *total <= MAX_CONNECT_PRELUDE_BYTES)
        .ok_or_else(|| StoreAccelError::InvalidRequest("CONNECT 首包超过上限".into()))?;
    let mut prelude = Vec::with_capacity(total);
    prelude.extend_from_slice(&header);
    if length != 0 {
        let mut body = vec![0_u8; length];
        stream.read_exact(&mut body)?;
        prelude.extend_from_slice(&body);
    }
    Ok(prelude)
}

fn prime_upstream(stream: &mut TcpStream, prelude: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(prelude)?;
    let mut response = vec![0_u8; MAX_CONNECT_PRELUDE_BYTES];
    let read = stream.read(&mut response)?;
    if read == 0 {
        return Err(StoreAccelError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "上游在 CONNECT 首包后提前关闭",
        )));
    }
    response.truncate(read);
    Ok(response)
}

fn is_successful_connect_response(response: &[u8]) -> bool {
    std::str::from_utf8(response)
        .ok()
        .and_then(|text| text.lines().next())
        .is_some_and(|line| line.starts_with("HTTP/1.1 2") || line.starts_with("HTTP/1.0 2"))
}

fn tunnel(mut client: TcpStream, upstream: &mut TcpStream) -> Result<()> {
    let mut client_from_upstream = client.try_clone()?;
    let mut upstream_for_client = upstream.try_clone()?;
    let downstream = thread::Builder::new()
        .name("stt-store-accel-downstream".into())
        .spawn(move || {
            let _ = io::copy(&mut upstream_for_client, &mut client_from_upstream);
            let _ = client_from_upstream.shutdown(Shutdown::Both);
        })?;

    let copy_result = io::copy(&mut client, upstream);
    let _ = upstream.shutdown(Shutdown::Write);
    match downstream.join() {
        Ok(()) | Err(_) => {}
    }
    copy_result.map(|_| ()).map_err(StoreAccelError::Io)
}

fn read_header(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut header = Vec::with_capacity(512);
    let mut byte = [0_u8; 1];
    while header.len() < MAX_HEADER_BYTES {
        let read = stream.read(&mut byte)?;
        if read == 0 {
            return Err(StoreAccelError::InvalidRequest("请求头提前结束".into()));
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            return Ok(header);
        }
    }
    Err(StoreAccelError::InvalidRequest("请求头超过上限".into()))
}

#[derive(Debug)]
enum ProxyRequest {
    Pac,
    Stop,
    Connect(Endpoint),
    Http(ForwardRequest),
}

#[derive(Debug)]
struct Endpoint {
    host: String,
    port: u16,
}

#[derive(Debug)]
struct ForwardRequest {
    endpoint: Endpoint,
    header: Vec<u8>,
}

fn parse_request(header: &[u8]) -> Result<ProxyRequest> {
    let text = std::str::from_utf8(header)
        .map_err(|_| StoreAccelError::InvalidRequest("请求头不是 UTF-8".into()))?;
    let Some(first_line_end) = text.find("\r\n") else {
        return Err(StoreAccelError::InvalidRequest("缺少请求行".into()));
    };
    let mut parts = text[..first_line_end].split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| StoreAccelError::InvalidRequest("缺少请求方法".into()))?;
    let target = parts
        .next()
        .ok_or_else(|| StoreAccelError::InvalidRequest("缺少请求目标".into()))?;
    let version = parts
        .next()
        .ok_or_else(|| StoreAccelError::InvalidRequest("缺少 HTTP 版本".into()))?;
    if parts.next().is_some() || !version.starts_with("HTTP/") {
        return Err(StoreAccelError::InvalidRequest("无效请求行".into()));
    }

    match (method, target) {
        ("GET", PAC_PATH) => Ok(ProxyRequest::Pac),
        ("POST", STOP_PATH) => Ok(ProxyRequest::Stop),
        ("CONNECT", authority) => Ok(ProxyRequest::Connect(parse_authority(
            authority,
            None,
            Some(443),
        )?)),
        (_, url) if url.starts_with("http://") => {
            parse_http_forward(method, url, version, header, first_line_end)
        }
        _ => Err(StoreAccelError::InvalidRequest(
            "只支持 CONNECT 和 http 绝对 URL".into(),
        )),
    }
}

fn parse_http_forward(
    method: &str,
    url: &str,
    version: &str,
    header: &[u8],
    first_line_end: usize,
) -> Result<ProxyRequest> {
    let authority_and_path = &url["http://".len()..];
    let (authority, path) = authority_and_path.split_once('/').map_or_else(
        || (authority_and_path, "/".to_owned()),
        |(authority, path)| (authority, format!("/{path}")),
    );
    let endpoint = parse_authority(authority, Some(80), Some(80))?;
    validate_forward_host(header, first_line_end, &endpoint)?;
    let mut forwarded = format!("{method} {path} {version}\r\n").into_bytes();
    forwarded.extend_from_slice(&header[first_line_end + 2..]);
    Ok(ProxyRequest::Http(ForwardRequest {
        endpoint,
        header: forwarded,
    }))
}

fn parse_authority(
    authority: &str,
    default_port: Option<u16>,
    required_port: Option<u16>,
) -> Result<Endpoint> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, parse_port(port)?),
        None => (
            authority,
            default_port
                .ok_or_else(|| StoreAccelError::InvalidRequest("CONNECT 缺少端口".into()))?,
        ),
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if !valid_domain_name(&host) || host.parse::<Ipv4Addr>().is_ok() {
        return Err(StoreAccelError::InvalidRequest("无效目标域名".into()));
    }
    if required_port.is_some_and(|required| port != required) {
        return Err(StoreAccelError::InvalidRequest(
            "目标端口不在网页范围".into(),
        ));
    }
    Ok(Endpoint { host, port })
}

fn validate_forward_host(header: &[u8], first_line_end: usize, endpoint: &Endpoint) -> Result<()> {
    let text = std::str::from_utf8(header)
        .map_err(|_| StoreAccelError::InvalidRequest("请求头不是 UTF-8".into()))?;
    let mut found = false;
    for line in text[first_line_end + 2..].split("\r\n") {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(StoreAccelError::InvalidRequest("HTTP 头格式无效".into()));
        };
        if !name.eq_ignore_ascii_case("host") {
            continue;
        }
        if found {
            return Err(StoreAccelError::InvalidRequest(
                "HTTP 请求包含重复 Host 头".into(),
            ));
        }
        let forwarded = parse_authority(value.trim(), Some(80), Some(80))?;
        if forwarded.host != endpoint.host || forwarded.port != endpoint.port {
            return Err(StoreAccelError::InvalidRequest(
                "HTTP Host 与目标不一致".into(),
            ));
        }
        found = true;
    }
    if found {
        Ok(())
    } else {
        Err(StoreAccelError::InvalidRequest(
            "HTTP 转发缺少 Host 头".into(),
        ))
    }
}

fn parse_port(value: &str) -> Result<u16> {
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| StoreAccelError::InvalidRequest("无效目标端口".into()))
}

fn valid_domain_name(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'-'
        })
}

fn write_response(
    mut stream: TcpStream,
    status: &str,
    body: &str,
    content_type: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    Ok(())
}

#[derive(Debug)]
struct DnsResolver {
    resolver: SocketAddrV4,
    timeout: Duration,
    cache: Mutex<HashMap<(String, DnsRecordType), CachedDnsAnswer>>,
    health: Mutex<HashMap<(String, IpAddr), CandidateHealth>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DnsRecordType {
    A,
    Aaaa,
}

impl DnsRecordType {
    const fn number(self) -> u16 {
        match self {
            Self::A => 1,
            Self::Aaaa => 28,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::Aaaa => "AAAA",
        }
    }
}

#[derive(Debug, Clone)]
struct CachedDnsAnswer {
    addresses: Vec<IpAddr>,
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy, Default)]
struct CandidateHealth {
    healthy_until: Option<Instant>,
    failed_until: Option<Instant>,
    latency: Option<Duration>,
}

impl DnsResolver {
    fn new(resolver: SocketAddrV4, timeout: Duration) -> Self {
        Self {
            resolver,
            timeout,
            cache: Mutex::new(HashMap::new()),
            health: Mutex::new(HashMap::new()),
        }
    }

    fn resolve(&self, host: &str) -> Result<Vec<IpAddr>> {
        let mut addresses = Vec::new();
        let mut query_error = None;
        for record_type in [DnsRecordType::A, DnsRecordType::Aaaa] {
            let answer_addresses = match self.cached(host, record_type) {
                Some(answer) => answer.addresses,
                None => match self.query(host, record_type) {
                    Ok(answer) => {
                        self.cache(host, record_type, &answer);
                        answer.addresses
                    }
                    Err(error) => {
                        query_error = Some(error);
                        continue;
                    }
                },
            };
            addresses.extend(answer_addresses);
        }
        addresses.sort_unstable();
        addresses.dedup();
        limit_dns_candidates(&mut addresses);
        if addresses.is_empty() {
            return Err(
                query_error.unwrap_or_else(|| StoreAccelError::Dns("A/AAAA 记录均为空".into()))
            );
        }
        Ok(addresses)
    }

    fn cached(&self, host: &str, record_type: DnsRecordType) -> Option<CachedDnsAnswer> {
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache
            .get(&(host.to_owned(), record_type))
            .filter(|entry| entry.expires_at > Instant::now())
            .cloned()
    }

    fn cache(&self, host: &str, record_type: DnsRecordType, answer: &DnsAnswer) {
        let ttl = answer
            .ttl
            .min(DNS_CACHE_TTL_CAP)
            .max(Duration::from_secs(1));
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() >= MAX_DNS_CACHE_ENTRIES {
            if let Some(key) = cache.keys().next().cloned() {
                cache.remove(&key);
            }
        }
        cache.insert(
            (host.to_owned(), record_type),
            CachedDnsAnswer {
                addresses: answer.addresses.clone(),
                expires_at: Instant::now() + ttl,
            },
        );
    }

    fn query(&self, host: &str, record_type: DnsRecordType) -> Result<DnsAnswer> {
        let id = DNS_QUERY_ID.fetch_add(1, Ordering::Relaxed);
        let request = build_dns_query(id, host, record_type)?;
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))?;
        socket.connect(self.resolver)?;
        socket.set_read_timeout(Some(self.timeout))?;
        socket.set_write_timeout(Some(self.timeout))?;
        socket.send(&request)?;
        let mut response = [0_u8; MAX_DNS_RESPONSE_BYTES];
        let length = socket.recv(&mut response)?;
        parse_dns_response(id, record_type, &response[..length])
    }

    fn is_cooling_down(&self, host: &str, address: IpAddr) -> bool {
        self.health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(host.to_owned(), address))
            .and_then(|state| state.failed_until)
            .is_some_and(|until| until > Instant::now())
    }

    fn should_probe(&self, host: &str, address: IpAddr) -> bool {
        let state = self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(host.to_owned(), address))
            .copied()
            .unwrap_or_default();
        let now = Instant::now();
        state.failed_until.is_none_or(|until| until <= now)
            && state.healthy_until.is_none_or(|until| until <= now)
    }

    fn mark_healthy(&self, host: &str, address: IpAddr) {
        self.mark_healthy_with_latency(host, address, self.candidate_latency(host, address));
    }

    fn mark_healthy_with_latency(
        &self,
        host: &str,
        address: IpAddr,
        latency: impl Into<Option<Duration>>,
    ) {
        let mut health = self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (host.to_owned(), address);
        if !health.contains_key(&key) && health.len() >= MAX_HEALTH_ENTRIES {
            if let Some(key) = health.keys().next().cloned() {
                health.remove(&key);
            }
        }
        health.insert(
            key,
            CandidateHealth {
                healthy_until: Some(Instant::now() + CANDIDATE_HEALTH_TTL),
                failed_until: None,
                latency: latency.into(),
            },
        );
    }

    fn candidate_latency(&self, host: &str, address: IpAddr) -> Option<Duration> {
        self.health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(host.to_owned(), address))
            .and_then(|state| state.latency)
    }

    fn mark_failed(&self, host: &str, address: IpAddr) {
        let mut health = self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (host.to_owned(), address);
        if !health.contains_key(&key) && health.len() >= MAX_HEALTH_ENTRIES {
            if let Some(key) = health.keys().next().cloned() {
                health.remove(&key);
            }
        }
        health.insert(
            key,
            CandidateHealth {
                healthy_until: None,
                failed_until: Some(Instant::now() + CANDIDATE_COOLDOWN),
                latency: None,
            },
        );
    }
}

fn limit_dns_candidates(addresses: &mut Vec<IpAddr>) {
    let per_family = MAX_DNS_CANDIDATES / 2;
    let mut limited = addresses
        .iter()
        .copied()
        .filter(|address| address.is_ipv4())
        .take(per_family)
        .collect::<Vec<_>>();
    limited.extend(
        addresses
            .iter()
            .copied()
            .filter(|address| address.is_ipv6())
            .take(per_family),
    );
    if limited.len() < MAX_DNS_CANDIDATES {
        let remaining = MAX_DNS_CANDIDATES - limited.len();
        let extra = addresses
            .iter()
            .copied()
            .filter(|address| !limited.contains(address))
            .take(remaining)
            .collect::<Vec<_>>();
        limited.extend(extra);
    }
    *addresses = limited;
}

#[derive(Debug)]
struct DnsAnswer {
    addresses: Vec<IpAddr>,
    ttl: Duration,
}

fn build_dns_query(id: u16, host: &str, record_type: DnsRecordType) -> Result<Vec<u8>> {
    let mut query = Vec::with_capacity(64);
    query.extend_from_slice(&id.to_be_bytes());
    query.extend_from_slice(&0x0100_u16.to_be_bytes());
    query.extend_from_slice(&1_u16.to_be_bytes());
    query.extend_from_slice(&0_u16.to_be_bytes());
    query.extend_from_slice(&0_u16.to_be_bytes());
    query.extend_from_slice(&0_u16.to_be_bytes());
    for label in host.split('.') {
        let length = u8::try_from(label.len())
            .ok()
            .filter(|length| *length != 0 && *length <= 63)
            .ok_or_else(|| StoreAccelError::Dns("域名标签长度无效".into()))?;
        query.push(length);
        query.extend_from_slice(label.as_bytes());
    }
    query.push(0);
    query.extend_from_slice(&record_type.number().to_be_bytes());
    query.extend_from_slice(&1_u16.to_be_bytes());
    Ok(query)
}

fn parse_dns_response(id: u16, expected_type: DnsRecordType, response: &[u8]) -> Result<DnsAnswer> {
    if response.len() < 12 {
        return Err(StoreAccelError::Dns("响应头不足".into()));
    }
    if u16::from_be_bytes([response[0], response[1]]) != id {
        return Err(StoreAccelError::Dns("响应 ID 不匹配".into()));
    }
    let flags = u16::from_be_bytes([response[2], response[3]]);
    if flags & 0x8000 == 0 || flags & 0x000F != 0 {
        return Err(StoreAccelError::Dns("解析器返回错误".into()));
    }
    let questions = usize::from(u16::from_be_bytes([response[4], response[5]]));
    let answers = usize::from(u16::from_be_bytes([response[6], response[7]]));
    let mut cursor = 12;
    for _ in 0..questions {
        skip_dns_name(response, &mut cursor)?;
        advance_dns(response, &mut cursor, 4)?;
    }

    let mut addresses = Vec::new();
    let mut min_ttl = None;
    for _ in 0..answers {
        skip_dns_name(response, &mut cursor)?;
        let answer_type = read_u16(response, &mut cursor)?;
        let class = read_u16(response, &mut cursor)?;
        let ttl = read_u32(response, &mut cursor)?;
        let length = usize::from(read_u16(response, &mut cursor)?);
        let data_start = cursor;
        advance_dns(response, &mut cursor, length)?;
        if answer_type == expected_type.number() && class == 1 {
            let address = match expected_type {
                DnsRecordType::A if length == 4 => IpAddr::V4(Ipv4Addr::new(
                    response[data_start],
                    response[data_start + 1],
                    response[data_start + 2],
                    response[data_start + 3],
                )),
                DnsRecordType::Aaaa if length == 16 => {
                    let mut bytes = [0_u8; 16];
                    bytes.copy_from_slice(&response[data_start..data_start + 16]);
                    IpAddr::V6(Ipv6Addr::from(bytes))
                }
                _ => continue,
            };
            addresses.push(address);
            min_ttl = Some(min_ttl.map_or(ttl, |current: u32| current.min(ttl)));
        }
    }
    if addresses.is_empty() {
        return Err(StoreAccelError::Dns(format!(
            "没有 {} 记录",
            expected_type.label()
        )));
    }
    Ok(DnsAnswer {
        addresses,
        ttl: Duration::from_secs(u64::from(min_ttl.unwrap_or(1))),
    })
}

fn skip_dns_name(input: &[u8], cursor: &mut usize) -> Result<()> {
    loop {
        let Some(&length) = input.get(*cursor) else {
            return Err(StoreAccelError::Dns("DNS 名称越界".into()));
        };
        if length == 0 {
            *cursor += 1;
            return Ok(());
        }
        if length & 0xC0 == 0xC0 {
            advance_dns(input, cursor, 2)?;
            return Ok(());
        }
        if length & 0xC0 != 0 {
            return Err(StoreAccelError::Dns("DNS 名称编码无效".into()));
        }
        advance_dns(input, cursor, usize::from(length) + 1)?;
    }
}

fn read_u16(input: &[u8], cursor: &mut usize) -> Result<u16> {
    let start = *cursor;
    advance_dns(input, cursor, 2)?;
    Ok(u16::from_be_bytes([input[start], input[start + 1]]))
}

fn read_u32(input: &[u8], cursor: &mut usize) -> Result<u32> {
    let start = *cursor;
    advance_dns(input, cursor, 4)?;
    Ok(u32::from_be_bytes([
        input[start],
        input[start + 1],
        input[start + 2],
        input[start + 3],
    ]))
}

fn advance_dns(input: &[u8], cursor: &mut usize, length: usize) -> Result<()> {
    let end = cursor
        .checked_add(length)
        .filter(|end| *end <= input.len())
        .ok_or_else(|| StoreAccelError::Dns("DNS 响应越界".into()))?;
    *cursor = end;
    Ok(())
}

static TLS_PROBE_CONFIG: OnceLock<std::result::Result<Arc<ClientConfig>, String>> = OnceLock::new();

fn tls_probe_config() -> Result<Arc<ClientConfig>> {
    match TLS_PROBE_CONFIG.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        roots.extend(TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Arc::new(config))
    }) {
        Ok(config) => Ok(Arc::clone(config)),
        Err(error) => Err(StoreAccelError::Tls(error.clone())),
    }
}

fn probe_https_candidate(address: IpAddr, host: &str, timeout: Duration) -> Result<()> {
    let tcp = TcpStream::connect_timeout(&SocketAddr::new(address, 443), timeout)?;
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;
    let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|error| StoreAccelError::Tls(format!("SNI 无效: {error}")))?;
    let connection = ClientConnection::new(tls_probe_config()?, server_name)
        .map_err(|error| StoreAccelError::Tls(error.to_string()))?;
    let mut tls = StreamOwned::new(connection, tcp);
    tls.write_all(
        format!(
            "HEAD / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: SteamTools-StoreAccel-Probe/1\r\n\r\n"
        )
        .as_bytes(),
    )
    .map_err(|error| StoreAccelError::Tls(error.to_string()))?;
    let mut response = Vec::with_capacity(256);
    let mut byte = [0_u8; 1];
    while response.len() < MAX_PROBE_HEADER_BYTES {
        let read = tls
            .read(&mut byte)
            .map_err(|error| StoreAccelError::Tls(error.to_string()))?;
        if read == 0 {
            break;
        }
        response.push(byte[0]);
        if response.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let first_line = std::str::from_utf8(&response)
        .ok()
        .and_then(|text| text.lines().next())
        .unwrap_or_default();
    if !first_line.starts_with("HTTP/") {
        return Err(StoreAccelError::Tls("HTTPS 探测没有返回 HTTP 响应".into()));
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct SystemProxySnapshot {
    pac_url: String,
    values: Vec<RegistryValue>,
    #[serde(default)]
    owner_pid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct RegistryValue {
    name: String,
    kind: u32,
    data: Option<Vec<u8>>,
}

struct SystemProxyGuard {
    snapshot_path: PathBuf,
    pac_url: String,
    restored: bool,
}

impl SystemProxyGuard {
    fn install(snapshot_path: &Path, pac_url: String) -> Result<Self> {
        recover_stale_snapshot(snapshot_path)?;
        let snapshot = SystemProxySnapshot {
            pac_url: pac_url.clone(),
            values: capture_settings()?,
            owner_pid: std::process::id(),
        };
        write_snapshot(snapshot_path, &snapshot)?;
        if let Err(error) = apply_pac(&pac_url) {
            let _ = restore_snapshot(&snapshot);
            let _ = fs::remove_file(snapshot_path);
            return Err(error);
        }
        Ok(Self {
            snapshot_path: snapshot_path.to_path_buf(),
            pac_url,
            restored: false,
        })
    }

    fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        let snapshot = read_snapshot(&self.snapshot_path)?;
        if current_pac_url()? == Some(self.pac_url.clone())
            && (snapshot.owner_pid == 0 || snapshot.owner_pid == std::process::id())
        {
            restore_snapshot(&snapshot)?;
        }
        fs::remove_file(&self.snapshot_path)?;
        Ok(())
    }
}

fn recover_stale_snapshot(snapshot_path: &Path) -> Result<()> {
    recover_stale_snapshot_for(snapshot_path, None).map(|_| ())
}

fn recover_stale_snapshot_for(snapshot_path: &Path, owner_pid: Option<u32>) -> Result<bool> {
    if !snapshot_path.is_file() {
        return Ok(false);
    }
    let snapshot = read_snapshot(snapshot_path)?;
    if owner_pid.is_some_and(|owner| snapshot.owner_pid != owner) {
        return Ok(false);
    }
    if current_pac_url()? == Some(snapshot.pac_url.clone()) {
        restore_snapshot(&snapshot)?;
    }
    fs::remove_file(snapshot_path)?;
    Ok(true)
}

fn write_snapshot(path: &Path, snapshot: &SystemProxySnapshot) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| StoreAccelError::SystemProxy("系统 PAC 快照没有父目录".into()))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec(snapshot)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn read_snapshot(path: &Path) -> Result<SystemProxySnapshot> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn capture_settings() -> Result<Vec<RegistryValue>> {
    with_settings_key(KEY_QUERY_VALUE, |key| {
        SETTINGS_VALUES
            .iter()
            .map(|name| read_registry_value(key, name))
            .collect()
    })
}

fn apply_pac(pac_url: &str) -> Result<()> {
    with_settings_key(KEY_SET_VALUE, |key| {
        write_registry_value(
            key,
            &RegistryValue {
                name: "AutoConfigURL".into(),
                kind: REG_SZ.0,
                data: Some(utf16_bytes(pac_url)),
            },
        )
    })?;
    refresh_internet_settings()
}

fn restore_snapshot(snapshot: &SystemProxySnapshot) -> Result<()> {
    with_settings_key(KEY_SET_VALUE, |key| {
        for value in &snapshot.values {
            write_registry_value(key, value)?;
        }
        Ok(())
    })?;
    refresh_internet_settings()
}

fn current_pac_url() -> Result<Option<String>> {
    with_settings_key(KEY_QUERY_VALUE, |key| {
        let value = read_registry_value(key, "AutoConfigURL")?;
        Ok(value.data.as_deref().and_then(decode_utf16_bytes))
    })
}

fn with_settings_key<T>(access: REG_SAM_FLAGS, f: impl FnOnce(HKEY) -> Result<T>) -> Result<T> {
    let key_path = wide(SETTINGS_KEY);
    let mut key = HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            0,
            access,
            &mut key,
        )
    };
    check_registry_status(status)?;
    let result = f(key);
    let _ = unsafe { RegCloseKey(key) };
    result
}

fn read_registry_value(key: HKEY, name: &str) -> Result<RegistryValue> {
    let wide_name = wide(name);
    let mut kind = REG_VALUE_TYPE(0);
    let mut length = 0_u32;
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(wide_name.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut length),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(RegistryValue {
            name: name.into(),
            kind: 0,
            data: None,
        });
    }
    check_registry_status(status)?;
    let mut data = vec![0_u8; usize::try_from(length).unwrap_or(0)];
    let pointer = (!data.is_empty()).then_some(data.as_mut_ptr());
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(wide_name.as_ptr()),
            None,
            Some(&mut kind),
            pointer,
            Some(&mut length),
        )
    };
    check_registry_status(status)?;
    data.truncate(usize::try_from(length).unwrap_or(0));
    Ok(RegistryValue {
        name: name.into(),
        kind: kind.0,
        data: Some(data),
    })
}

fn write_registry_value(key: HKEY, value: &RegistryValue) -> Result<()> {
    let name = wide(&value.name);
    match &value.data {
        Some(data) => {
            let status = unsafe {
                RegSetValueExW(
                    key,
                    PCWSTR(name.as_ptr()),
                    0,
                    REG_VALUE_TYPE(value.kind),
                    Some(data),
                )
            };
            check_registry_status(status)
        }
        None => {
            let status = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) };
            if status == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                check_registry_status(status)
            }
        }
    }
}

fn refresh_internet_settings() -> Result<()> {
    unsafe { InternetSetOptionW(None, INTERNET_OPTION_SETTINGS_CHANGED, None, 0) }
        .map_err(|error| StoreAccelError::SystemProxy(error.to_string()))?;
    unsafe { InternetSetOptionW(None, INTERNET_OPTION_REFRESH, None, 0) }
        .map_err(|error| StoreAccelError::SystemProxy(error.to_string()))?;
    Ok(())
}

fn check_registry_status(status: windows::Win32::Foundation::WIN32_ERROR) -> Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(StoreAccelError::SystemProxy(
            io::Error::from_raw_os_error(status.0 as i32).to_string(),
        ))
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn utf16_bytes(value: &str) -> Vec<u8> {
    wide(value).into_iter().flat_map(u16::to_le_bytes).collect()
}

fn decode_utf16_bytes(value: &[u8]) -> Option<String> {
    if value.len() < 2 || !value.len().is_multiple_of(2) {
        return None;
    }
    let units = value
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&units)
        .ok()
        .map(|text| text.trim_end_matches('\0').to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_accepts_store_and_rejects_unrelated_hosts() {
        assert!(is_allowed_host("store.steampowered.com"));
        assert!(is_allowed_host("steamcommunity.com"));
        assert!(is_allowed_host("cdn.cloudflare.steamstatic.com"));
        assert!(!is_allowed_host("content1.steamcontent.com"));
        assert_eq!(
            classify_host("steamuserimages-a.akamaihd.net"),
            Some(HostClass::WorkshopStatic)
        );
        assert_eq!(
            classify_host("store.akamai.steamstatic.com"),
            Some(HostClass::StoreStatic)
        );
        assert!(!is_allowed_host("steampowered.com.example"));
        assert!(!is_allowed_host("example.com"));
    }

    #[test]
    fn pac_only_contains_loopback_for_steam_domains() {
        let pac = pac_script(18_942);
        assert!(pac.contains("PROXY 127.0.0.1:18942"));
        assert!(pac.contains("; DIRECT"));
        assert!(pac.contains(".steampowered.com"));
        assert!(!pac.contains("steamcontent.com"));
        assert!(pac.contains("return 'DIRECT'"));
    }

    #[test]
    fn connect_request_requires_allowed_hostname() {
        let request =
            parse_request(b"CONNECT store.steampowered.com:443 HTTP/1.1\r\n\r\n").unwrap();
        assert!(matches!(
            request,
            ProxyRequest::Connect(Endpoint { port: 443, .. })
        ));
        assert!(parse_request(b"CONNECT 1.1.1.1:443 HTTP/1.1\r\n\r\n").is_err());
        assert!(parse_request(b"CONNECT store.steampowered.com:80 HTTP/1.1\r\n\r\n").is_err());
        assert!(parse_request(b"CONNECT store.steampowered.com:444 HTTP/1.1\r\n\r\n").is_err());
    }

    #[test]
    fn connect_prelude_reads_one_tls_record_without_parsing_it() {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(&[0x16, 0x03, 0x03, 0x00, 0x03, 0x01, 0x02, 0x03])
                .unwrap();
        });

        let mut client = TcpStream::connect(address).unwrap();
        let prelude = read_connect_prelude(&mut client).unwrap();
        assert_eq!(
            prelude,
            vec![0x16, 0x03, 0x03, 0x00, 0x03, 0x01, 0x02, 0x03]
        );
        worker.join().unwrap();
    }

    #[test]
    fn prime_upstream_replays_prelude_and_returns_first_response() {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let prelude = b"client hello".to_vec();
        let expected = prelude.clone();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = vec![0_u8; expected.len()];
            stream.read_exact(&mut received).unwrap();
            assert_eq!(received, expected);
            stream.write_all(b"server hello").unwrap();
        });

        let mut upstream = TcpStream::connect(address).unwrap();
        let response = prime_upstream(&mut upstream, &prelude, Duration::from_secs(1)).unwrap();
        assert_eq!(response, b"server hello");
        worker.join().unwrap();
    }

    #[test]
    fn http_forward_requires_matching_host_and_port() {
        let valid = parse_request(
            b"GET http://store.steampowered.com/ HTTP/1.1\r\nHost: store.steampowered.com\r\n\r\n",
        )
        .unwrap();
        assert!(matches!(valid, ProxyRequest::Http(_)));
        assert!(parse_request(
            b"GET http://store.steampowered.com/ HTTP/1.1\r\nHost: steamcommunity.com\r\n\r\n"
        )
        .is_err());
        assert!(parse_request(
            b"GET http://store.steampowered.com:8080/ HTTP/1.1\r\nHost: store.steampowered.com:8080\r\n\r\n"
        )
        .is_err());
        assert!(parse_request(b"GET http://store.steampowered.com/ HTTP/1.1\r\n\r\n").is_err());
    }

    #[test]
    fn dns_parser_reads_compressed_a_record() {
        let response = [
            0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x05, b's',
            b't', b'o', b'r', b'e', 0x05, b's', b't', b'e', b'a', b'm', 0x03, b'c', b'o', b'm',
            0x00, 0x00, 0x01, 0x00, 0x01, 0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00,
            0x3c, 0x00, 0x04, 104, 85, 0, 101,
        ];
        let answer = parse_dns_response(0x1234, DnsRecordType::A, &response).unwrap();
        assert_eq!(
            answer.addresses,
            vec![IpAddr::V4(Ipv4Addr::new(104, 85, 0, 101))]
        );
        assert_eq!(answer.ttl, Duration::from_secs(60));
    }

    #[test]
    fn dns_query_can_request_aaaa_without_using_system_dns() {
        let query = build_dns_query(7, "store.steampowered.com", DnsRecordType::Aaaa).unwrap();
        assert!(query.ends_with(&[0, 28, 0, 1]));
    }

    #[test]
    fn dns_candidate_limit_keeps_both_ip_families() {
        let mut addresses = vec![
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 3)),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 4)),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 5)),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 6)),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ];
        limit_dns_candidates(&mut addresses);
        assert_eq!(addresses.len(), MAX_DNS_CANDIDATES);
        assert!(addresses.iter().any(IpAddr::is_ipv4));
        assert!(addresses.iter().any(IpAddr::is_ipv6));
    }

    #[test]
    fn candidate_health_cools_only_the_failed_ip() {
        let resolver = DnsResolver::new(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 53),
            Duration::from_secs(1),
        );
        let failed = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let healthy = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));
        resolver.mark_failed("store.steampowered.com", failed);
        assert!(resolver.is_cooling_down("store.steampowered.com", failed));
        assert!(!resolver.is_cooling_down("store.steampowered.com", healthy));
        resolver.mark_healthy("store.steampowered.com", healthy);
        assert!(!resolver.should_probe("store.steampowered.com", healthy));
    }

    #[test]
    fn failed_candidate_health_is_bounded() {
        let resolver = DnsResolver::new(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 53),
            Duration::from_secs(1),
        );
        for index in 0..=MAX_HEALTH_ENTRIES {
            let address = IpAddr::V4(Ipv4Addr::new(
                198,
                18,
                u8::try_from(index / 256).unwrap_or_default(),
                u8::try_from(index % 256).unwrap_or_default(),
            ));
            resolver.mark_failed("store.steampowered.com", address);
        }
        let health = resolver
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(health.len() <= MAX_HEALTH_ENTRIES);
    }

    #[test]
    fn local_cdn_parses_optional_clash_fallback() {
        let config = StoreAccelSection {
            egress: StoreAccelEgress::LocalCdn,
            resolver: "1.1.1.1:53".into(),
            clash_fallback: "127.0.0.1:7890".into(),
            ..StoreAccelSection::default()
        };
        let helper = HelperConfig::try_from(&config).unwrap();
        assert!(matches!(
            helper.egress,
            EgressConfig::LocalCdn {
                clash_fallback: Some(SocketAddrV4 { .. }),
                ..
            }
        ));
    }

    #[test]
    fn http_upstream_requires_successful_connect_response() {
        assert!(is_successful_connect_response(
            b"HTTP/1.1 200 Connection Established\r\n\r\n"
        ));
        assert!(!is_successful_connect_response(
            b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n"
        ));
    }

    #[test]
    fn http_upstream_sends_the_original_host_and_port() {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = match listener.local_addr().unwrap() {
            SocketAddr::V4(address) => address,
            SocketAddr::V6(_) => unreachable!(),
        };
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let header = read_header(&mut stream).unwrap();
            let text = String::from_utf8(header).unwrap();
            assert!(text.contains("CONNECT steamcommunity.com:443 HTTP/1.1"));
            assert!(text.contains("Host: steamcommunity.com:443"));
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .unwrap();
        });

        let endpoint = Endpoint {
            host: "steamcommunity.com".into(),
            port: 443,
        };
        let stream = connect_via_http_upstream(address, Duration::from_secs(1), &endpoint).unwrap();
        drop(stream);
        worker.join().unwrap();
    }

    #[test]
    fn helper_refuses_unconfigured_egress_before_installing_pac() {
        let config = StoreAccelSection::default();
        let error = HelperConfig::try_from(&config).unwrap_err();

        assert!(error.to_string().contains("未配置"));
    }
}
