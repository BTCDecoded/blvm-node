//! Outbound HTTP broker. Modules never open sockets. The node fetches GET URLs
//! that the operator listed under `[module_network.<id>]`.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
#[cfg(feature = "module-http")]
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::module::module_name_from_id;

/// Design 19.2: `timeout_ms` capped at 15000.
pub const TIMEOUT_CAP_MS: u32 = 15_000;
/// Design 19.2: response body capped at 4 MiB.
pub const BODY_CAP_BYTES: usize = 4 * 1024 * 1024;
/// Design 19.2: per-module cap of 10 requests per second.
pub const MAX_REQUESTS_PER_SECOND: usize = 10;
/// Design 19.2: 4 requests in flight per module.
pub const MAX_IN_FLIGHT: u32 = 4;
/// Design 19.2: follow at most 3 redirects.
pub const MAX_REDIRECTS: u32 = 3;

/// Operator allow-list plus the optional node-wide proxy.
#[derive(Debug, Clone, Default)]
pub struct HttpPolicy {
    /// Module id or module name → exact origins (`scheme://host:port`).
    pub allow: HashMap<String, Vec<String>>,
    /// `[network] http_proxy`, for example `socks5h://127.0.0.1:9050`.
    pub proxy: Option<String>,
}

/// scheme, host, and port. Default ports are filled in so the match is exact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpOrigin {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl HttpOrigin {
    pub fn display(&self) -> String {
        format!("{}://{}:{}", self.scheme, self.host, self.port)
    }
}

#[derive(Debug, Clone)]
pub struct HttpBrokerError {
    pub message: String,
    pub origin: Option<String>,
    pub status: Option<u16>,
}

impl HttpBrokerError {
    fn new(origin: Option<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            origin,
            status: None,
        }
    }
}

#[derive(Debug)]
struct Slot {
    recent: VecDeque<Instant>,
    in_flight: u32,
}

/// Node-side GET broker.
#[derive(Debug)]
pub struct HttpBroker {
    policy: HttpPolicy,
    limits: Mutex<HashMap<String, Slot>>,
}

impl Default for HttpBroker {
    fn default() -> Self {
        Self::new(HttpPolicy::default())
    }
}

impl HttpBroker {
    pub fn new(policy: HttpPolicy) -> Self {
        Self {
            policy,
            limits: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_policy(&mut self, policy: HttpPolicy) {
        self.policy = policy;
    }

    pub fn allow_list_for(&self, module_id: &str) -> Vec<String> {
        if let Some(list) = self.policy.allow.get(module_id) {
            return list.clone();
        }
        let name = module_name_from_id(module_id);
        self.policy.allow.get(name).cloned().unwrap_or_default()
    }

    /// Reserve one of the 10/s and 4-in-flight slots. Call [`Self::end`] after the request.
    pub fn begin(&self, module_id: &str, now: Instant) -> Result<(), HttpBrokerError> {
        let mut limits = self.limits.lock().unwrap_or_else(|e| e.into_inner());
        let slot = limits.entry(module_id.to_string()).or_insert_with(|| Slot {
            recent: VecDeque::new(),
            in_flight: 0,
        });
        while slot
            .recent
            .front()
            .is_some_and(|stamp| now.duration_since(*stamp) >= Duration::from_secs(1))
        {
            slot.recent.pop_front();
        }
        if slot.recent.len() >= MAX_REQUESTS_PER_SECOND {
            return Err(HttpBrokerError::new(
                None,
                "http broker rate limit: 10 requests per second",
            ));
        }
        if slot.in_flight >= MAX_IN_FLIGHT {
            return Err(HttpBrokerError::new(
                None,
                "http broker limit: 4 requests in flight",
            ));
        }
        slot.recent.push_back(now);
        slot.in_flight += 1;
        Ok(())
    }

    pub fn end(&self, module_id: &str) {
        let mut limits = self.limits.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = limits.get_mut(module_id) {
            slot.in_flight = slot.in_flight.saturating_sub(1);
        }
    }

    pub async fn execute(
        &self,
        module_id: &str,
        url: &str,
        accept: Option<&str>,
        timeout_ms: u32,
    ) -> Result<Fetched, HttpBrokerError> {
        let _flight = Flight::begin(self, module_id)?;
        self.fetch(module_id, url, accept, timeout_ms).await
    }

    /// POST JSON through the same allow-list as GET. Redirects are not followed.
    pub async fn execute_post(
        &self,
        module_id: &str,
        url: &str,
        content_type: &str,
        body: &[u8],
        authorization: Option<&str>,
        timeout_ms: u32,
    ) -> Result<Fetched, HttpBrokerError> {
        let _flight = Flight::begin(self, module_id)?;
        if body.len() > BODY_CAP_BYTES {
            return Err(HttpBrokerError::new(None, "request body exceeds 4 MiB"));
        }
        let origin = check_url(url, &self.allow_list_for(module_id))?;
        #[cfg(not(feature = "module-http"))]
        {
            let _ = (content_type, timeout_ms, origin, authorization);
            Err(HttpBrokerError::new(
                Some(
                    parse_origin(url)
                        .map(|item| item.display())
                        .unwrap_or_default(),
                ),
                "http broker requires the module-http feature",
            ))
        }
        #[cfg(feature = "module-http")]
        {
            let response = send_post(
                &self.policy,
                url,
                &origin,
                content_type,
                body,
                authorization,
                timeout_ms,
            )
            .await?;
            let content_type = response.content_type.clone();
            if !content_type_allowed(content_type.as_deref()) {
                return Err(HttpBrokerError::new(
                    Some(origin.display()),
                    format!(
                        "content type is not json or text: {}",
                        content_type.unwrap_or_else(|| "missing".to_string())
                    ),
                ));
            }
            Ok(Fetched {
                origin: origin.display(),
                status: response.status,
                content_type,
                body: response.body,
            })
        }
    }

    async fn fetch(
        &self,
        module_id: &str,
        url: &str,
        accept: Option<&str>,
        timeout_ms: u32,
    ) -> Result<Fetched, HttpBrokerError> {
        let allow = self.allow_list_for(module_id);
        #[cfg(not(feature = "module-http"))]
        {
            let current = url.to_string();
            let redirects = 0u32;
            let origin = check_url(&current, &allow)?;
            let _ = (accept, timeout_ms, redirects, origin);
            Err(HttpBrokerError::new(
                Some(
                    parse_origin(&current)
                        .map(|o| o.display())
                        .unwrap_or_default(),
                ),
                "http broker requires the module-http feature",
            ))
        }
        #[cfg(feature = "module-http")]
        {
            let mut current = url.to_string();
            let mut redirects = 0u32;
            loop {
                let origin = check_url(&current, &allow)?;
                let response =
                    send_get(&self.policy, &current, &origin, accept, timeout_ms).await?;
                if is_redirect(response.status) {
                    if redirects >= MAX_REDIRECTS {
                        return Err(HttpBrokerError::new(
                            Some(origin.display()),
                            "http broker redirect limit: 3",
                        ));
                    }
                    let Some(location) = response.location else {
                        return Err(HttpBrokerError::new(
                            Some(origin.display()),
                            "redirect had no Location",
                        ));
                    };
                    current = resolve_redirect(&current, &location)
                        .map_err(|message| HttpBrokerError::new(Some(origin.display()), message))?;
                    redirects += 1;
                    continue;
                }
                let content_type = response.content_type.clone();
                if !content_type_allowed(content_type.as_deref()) {
                    return Err(HttpBrokerError::new(
                        Some(origin.display()),
                        format!(
                            "content type is not json or text: {}",
                            content_type.unwrap_or_else(|| "missing".to_string())
                        ),
                    ));
                }
                return Ok(Fetched {
                    origin: origin.display(),
                    status: response.status,
                    content_type,
                    body: response.body,
                });
            }
        }
    }
}

struct Flight<'a> {
    broker: &'a HttpBroker,
    module_id: String,
}

impl<'a> Flight<'a> {
    fn begin(broker: &'a HttpBroker, module_id: &str) -> Result<Self, HttpBrokerError> {
        broker.begin(module_id, Instant::now())?;
        Ok(Self {
            broker,
            module_id: module_id.to_string(),
        })
    }
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        self.broker.end(&self.module_id);
    }
}

#[derive(Debug, Clone)]
pub struct Fetched {
    pub origin: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

pub fn capped_timeout_ms(timeout_ms: u32) -> u32 {
    if timeout_ms == 0 {
        TIMEOUT_CAP_MS
    } else {
        timeout_ms.min(TIMEOUT_CAP_MS)
    }
}

/// Allow-list, scheme, and private-address rules from design 19.2.
pub fn check_url(url: &str, allow: &[String]) -> Result<HttpOrigin, HttpBrokerError> {
    let origin = parse_origin(url).map_err(|message| HttpBrokerError::new(None, message))?;
    let listed = allow.iter().any(|entry| {
        parse_origin(entry)
            .map(|allowed| allowed == origin)
            .unwrap_or(false)
    });
    let private_host = host_is_loopback_link_local_or_private(&origin.host);
    if !listed {
        if private_host {
            return Err(HttpBrokerError::new(
                Some(origin.display()),
                "private address refused",
            ));
        }
        return Err(HttpBrokerError::new(
            Some(origin.display()),
            "origin not allowed",
        ));
    }
    if origin.scheme == "http" && !private_host {
        return Err(HttpBrokerError::new(
            Some(origin.display()),
            "http is only allowed for a listed loopback or private host",
        ));
    }
    if origin.scheme != "https" && origin.scheme != "http" {
        return Err(HttpBrokerError::new(
            Some(origin.display()),
            "only https, or http for a listed loopback or private host",
        ));
    }
    Ok(origin)
}

pub fn parse_origin(raw: &str) -> Result<HttpOrigin, String> {
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or_else(|| "url needs a scheme".to_string())?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err("only http and https".to_string());
    }
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() {
        return Err("missing host".to_string());
    }
    let default_port = if scheme == "https" { 443 } else { 80 };
    let (host, port) = split_host_port(authority, default_port)?;
    if host.is_empty() {
        return Err("missing host".to_string());
    }
    Ok(HttpOrigin {
        scheme,
        host: host.to_ascii_lowercase(),
        port,
    })
}

fn split_host_port(authority: &str, default_port: u16) -> Result<(String, u16), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| "bad ipv6 host".to_string())?;
        let port = if let Some(port) = after.strip_prefix(':') {
            if port.is_empty() {
                return Err("bad port".to_string());
            }
            port.parse().map_err(|_| "bad port".to_string())?
        } else if after.is_empty() {
            default_port
        } else {
            return Err("bad ipv6 host".to_string());
        };
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            let port = port.parse().map_err(|_| "bad port".to_string())?;
            Ok((host.to_string(), port))
        }
        _ => Ok((authority.to_string(), default_port)),
    }
}

pub fn host_is_loopback_link_local_or_private(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => ip_is_loopback_link_local_or_private(ip),
        Err(_) => false,
    }
}

pub fn ip_is_loopback_link_local_or_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let shared = ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64;
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || shared
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return ip_is_loopback_link_local_or_private(IpAddr::V4(v4));
            }
            ip.is_loopback()
                || ip.is_unicast_link_local()
                || ip.is_unique_local()
                || ip.is_unspecified()
        }
    }
}

fn content_type_allowed(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let base = value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    base == "application/json" || base.ends_with("+json") || base.starts_with("text/")
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn resolve_redirect(current: &str, location: &str) -> Result<String, String> {
    if location.starts_with("https://") || location.starts_with("http://") {
        return Ok(location.to_string());
    }
    let origin = parse_origin(current)?;
    if let Some(path) = location.strip_prefix('/') {
        return Ok(format!(
            "{}://{}:{}/{}",
            origin.scheme, origin.host, origin.port, path
        ));
    }
    Err("redirect Location is not an http url".to_string())
}

#[cfg(feature = "module-http")]
struct Sent {
    status: u16,
    content_type: Option<String>,
    location: Option<String>,
    body: Vec<u8>,
}

#[cfg(feature = "module-http")]
async fn send_get(
    policy: &HttpPolicy,
    url: &str,
    origin: &HttpOrigin,
    accept: Option<&str>,
    timeout_ms: u32,
) -> Result<Sent, HttpBrokerError> {
    let ip = resolve_connect_address(policy, origin).await?;
    let timeout = Duration::from_millis(u64::from(capped_timeout_ms(timeout_ms)));
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout);
    if let Some(proxy) = policy.proxy.as_deref() {
        let proxy = reqwest::Proxy::all(proxy).map_err(|err| {
            HttpBrokerError::new(Some(origin.display()), format!("http proxy: {err}"))
        })?;
        builder = builder.proxy(proxy);
    }
    if let Some(ip) = ip {
        builder = builder.resolve(&origin.host, SocketAddr::new(ip, origin.port));
    }
    let client = builder.build().map_err(|err| {
        HttpBrokerError::new(Some(origin.display()), format!("http client: {err}"))
    })?;
    let mut request = client.get(url).timeout(timeout);
    if let Some(accept) = accept {
        request = request.header(reqwest::header::ACCEPT, accept);
    }
    let response = request.send().await.map_err(|err| {
        HttpBrokerError::new(
            Some(origin.display()),
            format!("http request failed: {err}"),
        )
    })?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if is_redirect(status) {
        return Ok(Sent {
            status,
            content_type,
            location,
            body: Vec::new(),
        });
    }
    let mut body = Vec::new();
    let mut stream = response;
    while let Some(chunk) = stream
        .chunk()
        .await
        .map_err(|err| HttpBrokerError::new(Some(origin.display()), format!("http body: {err}")))?
    {
        if body.len().saturating_add(chunk.len()) > BODY_CAP_BYTES {
            return Err(HttpBrokerError::new(
                Some(origin.display()),
                "response body exceeds 4 MiB",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Sent {
        status,
        content_type,
        location,
        body,
    })
}

#[cfg(feature = "module-http")]
async fn send_post(
    policy: &HttpPolicy,
    url: &str,
    origin: &HttpOrigin,
    content_type: &str,
    body: &[u8],
    authorization: Option<&str>,
    timeout_ms: u32,
) -> Result<Sent, HttpBrokerError> {
    let ip = resolve_connect_address(policy, origin).await?;
    let timeout = Duration::from_millis(u64::from(capped_timeout_ms(timeout_ms)));
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout);
    if let Some(proxy) = policy.proxy.as_deref() {
        let proxy = reqwest::Proxy::all(proxy).map_err(|err| {
            HttpBrokerError::new(Some(origin.display()), format!("http proxy: {err}"))
        })?;
        builder = builder.proxy(proxy);
    }
    if let Some(ip) = ip {
        builder = builder.resolve(&origin.host, SocketAddr::new(ip, origin.port));
    }
    let client = builder.build().map_err(|err| {
        HttpBrokerError::new(Some(origin.display()), format!("http client: {err}"))
    })?;
    let mut request = client
        .post(url)
        .timeout(timeout)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(body.to_vec());
    if let Some(authorization) = authorization {
        request = request.header(reqwest::header::AUTHORIZATION, authorization);
    }
    let response = request.send().await.map_err(|err| {
        HttpBrokerError::new(
            Some(origin.display()),
            format!("http request failed: {err}"),
        )
    })?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut response_body = Vec::new();
    let mut stream = response;
    while let Some(chunk) = stream
        .chunk()
        .await
        .map_err(|err| HttpBrokerError::new(Some(origin.display()), format!("http body: {err}")))?
    {
        if response_body.len().saturating_add(chunk.len()) > BODY_CAP_BYTES {
            return Err(HttpBrokerError::new(
                Some(origin.display()),
                "response body exceeds 4 MiB",
            ));
        }
        response_body.extend_from_slice(&chunk);
    }
    Ok(Sent {
        status,
        content_type,
        location: None,
        body: response_body,
    })
}

/// True when the proxy resolves hostnames itself (`socks5h`, `socks4a`).
#[cfg(feature = "module-http")]
fn proxy_resolves_hostnames(policy: &HttpPolicy) -> bool {
    policy.proxy.as_deref().is_some_and(|proxy| {
        let scheme = proxy.split_once("://").map_or("", |(scheme, _)| scheme);
        scheme.eq_ignore_ascii_case("socks5h") || scheme.eq_ignore_ascii_case("socks4a")
    })
}

/// Address to pin for the connection. `None` leaves the hostname to the proxy.
/// A looked-up address must pass the same loopback/link-local/private rule as
/// the listed host, and the client connects to that exact address.
#[cfg(feature = "module-http")]
async fn resolve_connect_address(
    policy: &HttpPolicy,
    origin: &HttpOrigin,
) -> Result<Option<IpAddr>, HttpBrokerError> {
    if let Ok(ip) = origin.host.parse::<IpAddr>() {
        return Ok(Some(ip));
    }
    if proxy_resolves_hostnames(policy) {
        return Ok(None);
    }
    let ip = lookup_first(origin).await?;
    if ip_is_loopback_link_local_or_private(ip)
        && !host_is_loopback_link_local_or_private(&origin.host)
    {
        return Err(HttpBrokerError::new(
            Some(origin.display()),
            "private address refused",
        ));
    }
    Ok(Some(ip))
}

#[cfg(feature = "module-http")]
async fn lookup_first(origin: &HttpOrigin) -> Result<IpAddr, HttpBrokerError> {
    #[cfg(test)]
    if let Some(ip) = lookup_stub::take(&origin.host) {
        return Ok(ip);
    }
    let mut addrs = tokio::net::lookup_host((origin.host.as_str(), origin.port))
        .await
        .map_err(|err| {
            HttpBrokerError::new(Some(origin.display()), format!("dns lookup failed: {err}"))
        })?;
    addrs.next().map(|addr| addr.ip()).ok_or_else(|| {
        HttpBrokerError::new(Some(origin.display()), "dns lookup returned no address")
    })
}

/// Test-only lookup override, per thread, so tests never use live DNS.
#[cfg(all(test, feature = "module-http"))]
mod lookup_stub {
    use std::cell::RefCell;
    use std::net::IpAddr;

    thread_local! {
        static STUB: RefCell<Option<(String, IpAddr)>> = const { RefCell::new(None) };
        static CALLS: RefCell<u32> = const { RefCell::new(0) };
    }

    pub fn set(host: &str, ip: IpAddr) {
        STUB.with(|stub| *stub.borrow_mut() = Some((host.to_string(), ip)));
    }

    pub fn calls() -> u32 {
        CALLS.with(|calls| *calls.borrow())
    }

    pub fn take(host: &str) -> Option<IpAddr> {
        CALLS.with(|calls| *calls.borrow_mut() += 1);
        STUB.with(|stub| {
            stub.borrow()
                .as_ref()
                .filter(|(name, _)| name == host)
                .map(|(_, ip)| *ip)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_origin_fills_default_ports() {
        let https = parse_origin("https://mempool.space/api/v1/fees/recommended").unwrap();
        assert_eq!(https.port, 443);
        let listed = parse_origin("https://mempool.space").unwrap();
        assert_eq!(https, listed);
        let other = parse_origin("https://mempool.space:8443").unwrap();
        assert_ne!(https, other);
    }

    #[test]
    fn unlisted_private_and_public_origins_differ() {
        let allow = ["http://127.0.0.1:9"];
        let private = check_url("http://10.1.2.3/secret", &allow.map(|s| s.to_string()));
        assert!(private.unwrap_err().message.contains("private address"));
        let public = check_url(
            "https://not-listed.example/x",
            &allow.map(|s| s.to_string()),
        );
        assert!(public.unwrap_err().message.contains("not allowed"));
    }

    #[test]
    fn listed_http_requires_a_loopback_or_private_host() {
        let allow = ["http://example.com".to_string()];
        let err = check_url("http://example.com/x", &allow).unwrap_err();
        assert!(err.message.contains("loopback or private"));
        check_url("http://127.0.0.1:9/", &["http://127.0.0.1:9".to_string()]).unwrap();
        check_url("http://10.1.2.3/", &["http://10.1.2.3".to_string()]).unwrap();
    }

    #[test]
    fn resolved_address_ranges() {
        let internal = [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "0.0.0.0",
            "100.64.0.1",
            "100.127.255.254",
            "::",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:10.0.0.1",
            "::ffff:100.64.0.1",
        ];
        for ip in internal {
            assert!(
                ip_is_loopback_link_local_or_private(ip.parse().unwrap()),
                "{ip}"
            );
        }
        for ip in [
            "1.1.1.1",
            "100.128.0.1",
            "100.63.255.255",
            "2606:4700::1111",
            "::ffff:1.1.1.1",
        ] {
            assert!(
                !ip_is_loopback_link_local_or_private(ip.parse().unwrap()),
                "{ip}"
            );
        }
        assert!(host_is_loopback_link_local_or_private("localhost"));
        assert!(host_is_loopback_link_local_or_private("127.0.0.1"));
        assert!(host_is_loopback_link_local_or_private("::ffff:127.0.0.1"));
        assert!(!host_is_loopback_link_local_or_private("example.com"));
    }

    #[test]
    fn timeout_is_capped_at_the_design_limit() {
        assert_eq!(capped_timeout_ms(20_000), 15_000);
        assert_eq!(capped_timeout_ms(1000), 1000);
    }

    #[test]
    fn rate_and_inflight_caps() {
        let broker = HttpBroker::default();
        let now = Instant::now();
        for _ in 0..MAX_REQUESTS_PER_SECOND {
            broker.begin("mod", now).unwrap();
            broker.end("mod");
        }
        assert!(broker.begin("mod", now).unwrap_err().message.contains("10"));
        let broker = HttpBroker::default();
        for _ in 0..MAX_IN_FLIGHT {
            broker.begin("mod", now).unwrap();
            broker.end("mod");
        }
        for _ in 0..MAX_IN_FLIGHT {
            broker.begin("mod", now).unwrap();
        }
        assert!(
            broker
                .begin("mod", now)
                .unwrap_err()
                .message
                .contains("in flight")
        );
    }

    #[cfg(feature = "module-http")]
    mod resolve {
        use super::super::*;
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        fn listener() -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
            let hit = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&hit);
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let handle = std::thread::spawn(move || {
                for _ in 0..10 {
                    if let Ok((mut sock, _)) = listener.accept() {
                        flag.store(true, Ordering::SeqCst);
                        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
            (port, hit, handle)
        }

        fn broker(origin: &str, proxy: Option<&str>) -> HttpBroker {
            let mut allow = HashMap::new();
            allow.insert("mod".to_string(), vec![origin.to_string()]);
            HttpBroker::new(HttpPolicy {
                allow,
                proxy: proxy.map(str::to_string),
            })
        }

        #[tokio::test]
        async fn listed_hostname_resolving_internal_is_refused() {
            for ip in ["127.0.0.1", "169.254.169.254", "::ffff:127.0.0.1"] {
                let (port, hit, handle) = listener();
                let origin = format!("https://feeds.example:{port}");
                lookup_stub::set("feeds.example", ip.parse().unwrap());
                let err = broker(&origin, None)
                    .execute("mod", &format!("{origin}/x"), None, 1000)
                    .await
                    .unwrap_err();
                assert!(
                    err.message.contains("private address"),
                    "{ip}: {}",
                    err.message
                );
                handle.join().unwrap();
                assert!(!hit.load(Ordering::SeqCst), "{ip}");
            }
        }

        #[tokio::test]
        async fn listed_internal_literal_and_localhost_still_resolve() {
            let origin = HttpOrigin {
                scheme: "http".to_string(),
                host: "localhost".to_string(),
                port: 9,
            };
            lookup_stub::set("localhost", "127.0.0.1".parse().unwrap());
            let ip = resolve_connect_address(&HttpPolicy::default(), &origin)
                .await
                .unwrap();
            assert_eq!(ip, Some("127.0.0.1".parse().unwrap()));
            let origin = HttpOrigin {
                host: "10.1.2.3".to_string(),
                ..origin
            };
            let ip = resolve_connect_address(&HttpPolicy::default(), &origin)
                .await
                .unwrap();
            assert_eq!(ip, Some("10.1.2.3".parse().unwrap()));
        }

        #[tokio::test]
        async fn hostname_proxy_skips_local_lookup() {
            let origin = HttpOrigin {
                scheme: "https".to_string(),
                host: "feeds.example".to_string(),
                port: 443,
            };
            lookup_stub::set("feeds.example", "127.0.0.1".parse().unwrap());
            for proxy in ["socks5h://127.0.0.1:9050", "socks4a://127.0.0.1:9050"] {
                let policy = HttpPolicy {
                    allow: HashMap::new(),
                    proxy: Some(proxy.to_string()),
                };
                let before = lookup_stub::calls();
                let ip = resolve_connect_address(&policy, &origin).await.unwrap();
                assert_eq!(ip, None, "{proxy}");
                assert_eq!(lookup_stub::calls(), before, "{proxy}");
            }
            let policy = HttpPolicy {
                allow: HashMap::new(),
                proxy: Some("socks5://127.0.0.1:9050".to_string()),
            };
            let err = resolve_connect_address(&policy, &origin).await.unwrap_err();
            assert!(err.message.contains("private address"));
        }
    }
}
