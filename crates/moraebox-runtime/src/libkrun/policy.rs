//! Fail-closed Ethernet egress policy for the native network relay.
//!
//! This module deliberately owns no sockets. A relay supplies complete Ethernet frames, their
//! direction, and a monotonic timestamp. The engine returns a decision and retains only bounded
//! DNS, flow, and TLS `ClientHello` state.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr,
    time::Duration,
};

const ETHERNET_HEADER_LEN: usize = 14;
const ETHERTYPE_ARP: u16 = 0x0806;
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_IPV6: u16 = 0x86dd;
const IPPROTO_TCP: u8 = 6;
const IPPROTO_UDP: u8 = 17;
const DNS_PORT: u16 = 53;
const DHCP_SERVER_PORT: u16 = 67;
const DHCP_CLIENT_PORT: u16 = 68;
const HTTPS_PORT: u16 = 443;
const MAX_CIDRS: usize = 128;
const MAX_DOMAINS: usize = 128;
const MAX_PUBLISHED_PORTS: usize = 32;
const MAX_INFRASTRUCTURE_ADDRESSES: usize = 16;
const MAX_CONTROL_ADDRESSES: usize = 16;
const MAX_FLOWS_HARD: usize = 16_384;
const MAX_DNS_STATE_HARD: usize = 16_384;
const MAX_TLS_BUFFER_HARD: usize = 64 * 1024;
const MAX_TOTAL_TLS_BYTES_HARD: usize = 16 * 1024 * 1024;
const MAX_DNS_MESSAGE: usize = 4096;
const MAX_DNS_NAME_POINTERS: usize = 32;
const MAX_DNS_RECORDS: usize = 256;
const MAX_TLS_RECORD: usize = 18 * 1024;
const MAX_DOMAIN_LEN: usize = 253;
// gvproxy's host DNS proxy returns zero-TTL answers. Keep a correlated answer just long enough
// for the resolver's immediate TLS connection, without turning it into a reusable DNS cache.
const ZERO_TTL_DNS_BINDING_GRACE_SECS: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyMode {
    Disabled,
    Unrestricted,
    Allowlist,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDirection {
    GuestToNetwork,
    NetworkToGuest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowReason {
    ArpInfrastructure,
    DhcpInfrastructure,
    DnsInfrastructure,
    Unrestricted,
    Cidr,
    DomainHandshake,
    DomainSni,
    Preview,
    ReverseFlow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    Disabled,
    Malformed,
    UnsupportedEtherType,
    Fragment,
    UnsupportedProtocol,
    Infrastructure,
    ControlEndpoint,
    NotAllowlisted,
    DomainRequiresTcpTls,
    TlsInvalid,
    TlsNoSni,
    TlsEch,
    TlsSniMismatch,
    OutOfOrder,
    StateLimit,
    NoReverseFlow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason {
    TlsClientHello,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow(AllowReason),
    /// The relay must not write this frame to the network. It may discard it and rely on TCP
    /// retransmission after a later frame yields `Allow(AllowReason::DomainSni)`; any retained
    /// frames must instead be bounded per flow and released only for that verified flow.
    Hold(HoldReason),
    Deny(DenyReason),
}

impl PolicyDecision {
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allow(_))
    }

    pub fn is_held(self) -> bool {
        matches!(self, Self::Hold(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyLimits {
    pub max_flows: usize,
    pub max_pending_dns: usize,
    pub max_dns_bindings: usize,
    pub max_tls_buffer_per_flow: usize,
    pub max_total_tls_bytes: usize,
    pub flow_ttl: Duration,
    pub pending_tls_ttl: Duration,
    pub dns_query_ttl: Duration,
    pub max_dns_ttl: Duration,
}

impl Default for PolicyLimits {
    fn default() -> Self {
        Self {
            max_flows: 1024,
            max_pending_dns: 256,
            max_dns_bindings: 1024,
            max_tls_buffer_per_flow: 16 * 1024,
            max_total_tls_bytes: 1024 * 1024,
            flow_ttl: Duration::from_secs(5 * 60),
            pending_tls_ttl: Duration::from_secs(30),
            dns_query_ttl: Duration::from_secs(10),
            max_dns_ttl: Duration::from_secs(60 * 60),
        }
    }
}

impl PolicyLimits {
    fn validate(self) -> Result<(), PolicyConfigError> {
        if self.max_flows == 0 || self.max_flows > MAX_FLOWS_HARD {
            return Err(PolicyConfigError::new(
                "max_flows is outside the safe bound",
            ));
        }
        if self.max_pending_dns == 0
            || self.max_pending_dns > MAX_DNS_STATE_HARD
            || self.max_dns_bindings == 0
            || self.max_dns_bindings > MAX_DNS_STATE_HARD
        {
            return Err(PolicyConfigError::new(
                "DNS state limit is outside the safe bound",
            ));
        }
        if self.max_tls_buffer_per_flow < 512
            || self.max_tls_buffer_per_flow > MAX_TLS_BUFFER_HARD
            || self.max_total_tls_bytes < self.max_tls_buffer_per_flow
            || self.max_total_tls_bytes > MAX_TOTAL_TLS_BYTES_HARD
        {
            return Err(PolicyConfigError::new(
                "TLS state limit is outside the safe bound",
            ));
        }
        if self.flow_ttl.is_zero()
            || self.pending_tls_ttl.is_zero()
            || self.dns_query_ttl.is_zero()
            || self.max_dns_ttl.is_zero()
        {
            return Err(PolicyConfigError::new("policy state TTLs must be non-zero"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyConfigError(String);

impl PolicyConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for PolicyConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PolicyConfigError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cidr {
    address: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn contains(self, candidate: IpAddr) -> bool {
        match (self.address, candidate) {
            (IpAddr::V4(network), IpAddr::V4(candidate)) => {
                let prefix = u32::from(self.prefix);
                let mask = if prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - prefix)
                };
                u32::from(network) & mask == u32::from(candidate) & mask
            }
            (IpAddr::V6(network), IpAddr::V6(candidate)) => {
                let prefix = u32::from(self.prefix);
                let mask = if prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - prefix)
                };
                u128::from(network) & mask == u128::from(candidate) & mask
            }
            _ => false,
        }
    }
}

impl FromStr for Cidr {
    type Err = PolicyConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (address, prefix) = value
            .split_once('/')
            .ok_or_else(|| PolicyConfigError::new("CIDR is missing a prefix"))?;
        if address.is_empty() || prefix.is_empty() || prefix.contains('/') {
            return Err(PolicyConfigError::new("CIDR has invalid syntax"));
        }
        let address = address
            .parse::<IpAddr>()
            .map_err(|_| PolicyConfigError::new("CIDR has an invalid address"))?;
        let prefix = prefix
            .parse::<u8>()
            .map_err(|_| PolicyConfigError::new("CIDR has an invalid prefix"))?;
        if prefix > if address.is_ipv4() { 32 } else { 128 } {
            return Err(PolicyConfigError::new("CIDR prefix exceeds address width"));
        }
        Ok(Self { address, prefix })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DomainPattern {
    suffix: String,
    wildcard: bool,
}

impl DomainPattern {
    pub fn matches(&self, name: &str) -> bool {
        let Some(name) = normalize_domain(name.trim_end_matches('.')) else {
            return false;
        };
        if name != self.suffix
            && !(self.wildcard
                && name.len() > self.suffix.len()
                && name.as_bytes()[name.len() - self.suffix.len() - 1] == b'.'
                && name[name.len() - self.suffix.len()..] == self.suffix)
        {
            return false;
        }
        !self.wildcard || name.len() > self.suffix.len()
    }
}

impl FromStr for DomainPattern {
    type Err = PolicyConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let wildcard = value.starts_with("*.");
        if value.starts_with('*') && !wildcard {
            return Err(PolicyConfigError::new(
                "wildcard must be the complete first label",
            ));
        }
        let value = value.strip_prefix("*.").unwrap_or(value);
        let suffix = normalize_domain(value)
            .ok_or_else(|| PolicyConfigError::new("invalid domain pattern"))?;
        Ok(Self { suffix, wildcard })
    }
}

#[derive(Debug, Clone)]
pub struct PolicyConfig {
    mode: PolicyMode,
    cidrs: Vec<Cidr>,
    domains: Vec<DomainPattern>,
    dns_servers: BTreeSet<IpAddr>,
    arp_targets: BTreeSet<Ipv4Addr>,
    guest_ipv4_addresses: BTreeSet<Ipv4Addr>,
    control_addresses: BTreeSet<IpAddr>,
    published_tcp_ports: BTreeSet<u16>,
    limits: PolicyLimits,
}

impl PolicyConfig {
    pub fn new(
        mode: PolicyMode,
        cidrs: impl IntoIterator<Item = Cidr>,
        domains: impl IntoIterator<Item = DomainPattern>,
        dns_servers: impl IntoIterator<Item = IpAddr>,
        arp_targets: impl IntoIterator<Item = Ipv4Addr>,
        control_addresses: impl IntoIterator<Item = IpAddr>,
        limits: PolicyLimits,
    ) -> Result<Self, PolicyConfigError> {
        limits.validate()?;
        let cidrs = cidrs.into_iter().collect::<Vec<_>>();
        let domains = domains.into_iter().collect::<Vec<_>>();
        let dns_servers = dns_servers.into_iter().collect::<BTreeSet<_>>();
        let arp_targets = arp_targets.into_iter().collect::<BTreeSet<_>>();
        let control_addresses = control_addresses.into_iter().collect::<BTreeSet<_>>();
        if cidrs.len() > MAX_CIDRS || domains.len() > MAX_DOMAINS {
            return Err(PolicyConfigError::new("allowlist exceeds the rule bound"));
        }
        if dns_servers.len() > MAX_INFRASTRUCTURE_ADDRESSES
            || arp_targets.len() > MAX_INFRASTRUCTURE_ADDRESSES
            || control_addresses.len() > MAX_CONTROL_ADDRESSES
        {
            return Err(PolicyConfigError::new(
                "infrastructure address list exceeds its bound",
            ));
        }
        if mode != PolicyMode::Allowlist && (!cidrs.is_empty() || !domains.is_empty()) {
            return Err(PolicyConfigError::new(
                "CIDR and domain rules require allowlist mode",
            ));
        }
        Ok(Self {
            mode,
            cidrs,
            domains,
            dns_servers,
            arp_targets,
            guest_ipv4_addresses: BTreeSet::new(),
            control_addresses,
            published_tcp_ports: BTreeSet::new(),
            limits,
        })
    }

    pub fn with_published_tcp_ports(
        mut self,
        ports: impl IntoIterator<Item = u16>,
    ) -> Result<Self, PolicyConfigError> {
        self.published_tcp_ports = ports.into_iter().collect();
        if self.published_tcp_ports.len() > MAX_PUBLISHED_PORTS
            || self.published_tcp_ports.contains(&0)
        {
            return Err(PolicyConfigError::new(
                "published TCP ports exceed the safe bound or contain zero",
            ));
        }
        Ok(self)
    }

    pub fn with_guest_ipv4_addresses(
        mut self,
        addresses: impl IntoIterator<Item = Ipv4Addr>,
    ) -> Result<Self, PolicyConfigError> {
        self.guest_ipv4_addresses = addresses.into_iter().collect();
        if self.guest_ipv4_addresses.len() > MAX_INFRASTRUCTURE_ADDRESSES
            || self.guest_ipv4_addresses.contains(&Ipv4Addr::UNSPECIFIED)
            || self.guest_ipv4_addresses.contains(&Ipv4Addr::BROADCAST)
        {
            return Err(PolicyConfigError::new(
                "guest IPv4 address list exceeds its bound or contains an invalid address",
            ));
        }
        Ok(self)
    }
}

fn normalize_domain(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > MAX_DOMAIN_LEN || value.ends_with('.') {
        return None;
    }
    if !value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Transport {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FlowKey {
    guest_ip: IpAddr,
    guest_port: u16,
    remote_ip: IpAddr,
    remote_port: u16,
    transport: Transport,
}

#[derive(Debug)]
enum FlowState {
    Allowed { expires: Duration },
    PendingTls(TlsFlow),
}

impl FlowState {
    fn expires(&self) -> Duration {
        match self {
            Self::Allowed { expires } => *expires,
            Self::PendingTls(flow) => flow.expires,
        }
    }
}

#[derive(Debug)]
struct TlsFlow {
    initial_sequence: u32,
    bytes: Vec<u8>,
    expires: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DnsQueryKey {
    server: IpAddr,
    guest: IpAddr,
    guest_port: u16,
    id: u16,
}

#[derive(Debug)]
struct PendingDns {
    name: String,
    expires: Duration,
}

#[derive(Debug)]
struct DnsBinding {
    name: String,
    expires: Duration,
}

#[derive(Debug)]
pub struct PolicyEngine {
    config: PolicyConfig,
    flows: BTreeMap<FlowKey, FlowState>,
    pending_dns: BTreeMap<DnsQueryKey, PendingDns>,
    dns_bindings: BTreeMap<IpAddr, Vec<DnsBinding>>,
    total_tls_bytes: usize,
}

impl PolicyEngine {
    pub fn new(config: PolicyConfig) -> Self {
        Self {
            config,
            flows: BTreeMap::new(),
            pending_dns: BTreeMap::new(),
            dns_bindings: BTreeMap::new(),
            total_tls_bytes: 0,
        }
    }

    pub fn evaluate_ethernet(
        &mut self,
        direction: FrameDirection,
        frame: &[u8],
        now: Duration,
    ) -> PolicyDecision {
        self.expire(now);
        if self.config.mode == PolicyMode::Disabled {
            return PolicyDecision::Deny(DenyReason::Disabled);
        }
        let Some(ether_type) = frame
            .get(12..14)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u16::from_be_bytes)
        else {
            return PolicyDecision::Deny(DenyReason::Malformed);
        };
        let Some(payload) = frame.get(ETHERNET_HEADER_LEN..) else {
            return PolicyDecision::Deny(DenyReason::Malformed);
        };
        match ether_type {
            ETHERTYPE_ARP => self.evaluate_arp(direction, payload),
            ETHERTYPE_IPV4 | ETHERTYPE_IPV6 => {
                let packet = if ether_type == ETHERTYPE_IPV4 {
                    parse_ipv4(payload)
                } else {
                    parse_ipv6(payload)
                };
                match packet {
                    Ok(packet) => self.evaluate_ip(direction, packet, now),
                    Err(reason) => PolicyDecision::Deny(reason),
                }
            }
            _ => PolicyDecision::Deny(DenyReason::UnsupportedEtherType),
        }
    }

    pub fn flow_count(&self) -> usize {
        self.flows.len()
    }

    pub fn dns_binding_count(&self) -> usize {
        self.dns_bindings.values().map(Vec::len).sum()
    }

    fn expire(&mut self, now: Duration) {
        self.pending_dns.retain(|_, query| query.expires > now);
        self.dns_bindings.retain(|_, bindings| {
            bindings.retain(|binding| binding.expires > now);
            !bindings.is_empty()
        });
        self.flows.retain(|_, state| state.expires() > now);
        self.total_tls_bytes = self
            .flows
            .values()
            .filter_map(|state| match state {
                FlowState::PendingTls(flow) => Some(flow.bytes.len()),
                FlowState::Allowed { .. } => None,
            })
            .sum();
    }

    fn evaluate_arp(&self, direction: FrameDirection, payload: &[u8]) -> PolicyDecision {
        if payload.len() < 28
            || payload[0..2] != [0, 1]
            || payload[2..4] != [0x08, 0x00]
            || payload[4] != 6
            || payload[5] != 4
        {
            return PolicyDecision::Deny(DenyReason::Malformed);
        }
        let operation = u16::from_be_bytes([payload[6], payload[7]]);
        let sender = Ipv4Addr::new(payload[14], payload[15], payload[16], payload[17]);
        let target = Ipv4Addr::new(payload[24], payload[25], payload[26], payload[27]);
        let allowed = match direction {
            FrameDirection::GuestToNetwork => {
                (operation == 1 && self.config.arp_targets.contains(&target))
                    || (operation == 2
                        && self.config.guest_ipv4_addresses.contains(&sender)
                        && self.config.arp_targets.contains(&target))
            }
            FrameDirection::NetworkToGuest => {
                (operation == 2 && self.config.arp_targets.contains(&sender))
                    || (operation == 1
                        && self.config.arp_targets.contains(&sender)
                        && self.config.guest_ipv4_addresses.contains(&target))
            }
        };
        if allowed {
            PolicyDecision::Allow(AllowReason::ArpInfrastructure)
        } else {
            PolicyDecision::Deny(DenyReason::Infrastructure)
        }
    }
}

#[derive(Clone, Copy)]
struct IpPacket<'a> {
    source: IpAddr,
    destination: IpAddr,
    protocol: u8,
    payload: &'a [u8],
}

fn parse_ipv4(payload: &[u8]) -> Result<IpPacket<'_>, DenyReason> {
    if payload.len() < 20 || payload[0] >> 4 != 4 {
        return Err(DenyReason::Malformed);
    }
    let header_len = usize::from(payload[0] & 0x0f) * 4;
    let total_len = usize::from(u16::from_be_bytes([payload[2], payload[3]]));
    if header_len < 20 || total_len < header_len || total_len > payload.len() {
        return Err(DenyReason::Malformed);
    }
    let fragment = u16::from_be_bytes([payload[6], payload[7]]);
    if fragment & 0xbfff != 0 {
        return Err(DenyReason::Fragment);
    }
    if checksum(&payload[..header_len]) != 0 {
        return Err(DenyReason::Malformed);
    }
    Ok(IpPacket {
        source: IpAddr::V4(Ipv4Addr::new(
            payload[12],
            payload[13],
            payload[14],
            payload[15],
        )),
        destination: IpAddr::V4(Ipv4Addr::new(
            payload[16],
            payload[17],
            payload[18],
            payload[19],
        )),
        protocol: payload[9],
        payload: &payload[header_len..total_len],
    })
}

fn parse_ipv6(payload: &[u8]) -> Result<IpPacket<'_>, DenyReason> {
    if payload.len() < 40 || payload[0] >> 4 != 6 {
        return Err(DenyReason::Malformed);
    }
    let body_len = usize::from(u16::from_be_bytes([payload[4], payload[5]]));
    let total_len = 40_usize
        .checked_add(body_len)
        .ok_or(DenyReason::Malformed)?;
    if total_len > payload.len() {
        return Err(DenyReason::Malformed);
    }
    let next_header = payload[6];
    if next_header == 44 {
        return Err(DenyReason::Fragment);
    }
    if !matches!(next_header, IPPROTO_TCP | IPPROTO_UDP) {
        return Err(DenyReason::UnsupportedProtocol);
    }
    let source = <[u8; 16]>::try_from(&payload[8..24]).map_err(|_| DenyReason::Malformed)?;
    let destination = <[u8; 16]>::try_from(&payload[24..40]).map_err(|_| DenyReason::Malformed)?;
    Ok(IpPacket {
        source: IpAddr::V6(Ipv6Addr::from(source)),
        destination: IpAddr::V6(Ipv6Addr::from(destination)),
        protocol: next_header,
        payload: &payload[40..total_len],
    })
}

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0_u32;
    for chunk in bytes.chunks(2) {
        let word = if chunk.len() == 2 {
            u16::from_be_bytes([chunk[0], chunk[1]])
        } else {
            u16::from(chunk[0]) << 8
        };
        sum = sum.wrapping_add(u32::from(word));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !u16::try_from(sum).expect("a folded Internet checksum fits in 16 bits")
}

#[derive(Debug, Clone, Copy)]
struct UdpPacket<'a> {
    source_port: u16,
    destination_port: u16,
    payload: &'a [u8],
}

fn parse_udp(payload: &[u8]) -> Result<UdpPacket<'_>, DenyReason> {
    if payload.len() < 8 {
        return Err(DenyReason::Malformed);
    }
    let length = usize::from(u16::from_be_bytes([payload[4], payload[5]]));
    if length < 8 || length > payload.len() {
        return Err(DenyReason::Malformed);
    }
    Ok(UdpPacket {
        source_port: u16::from_be_bytes([payload[0], payload[1]]),
        destination_port: u16::from_be_bytes([payload[2], payload[3]]),
        payload: &payload[8..length],
    })
}

#[derive(Debug, Clone, Copy)]
struct TcpPacket<'a> {
    source_port: u16,
    destination_port: u16,
    sequence: u32,
    syn: bool,
    ack: bool,
    rst: bool,
    payload: &'a [u8],
}

fn parse_tcp(payload: &[u8]) -> Result<TcpPacket<'_>, DenyReason> {
    if payload.len() < 20 {
        return Err(DenyReason::Malformed);
    }
    let header_len = usize::from(payload[12] >> 4) * 4;
    if header_len < 20 || header_len > payload.len() || payload[12] & 0x0f != 0 {
        return Err(DenyReason::Malformed);
    }
    Ok(TcpPacket {
        source_port: u16::from_be_bytes([payload[0], payload[1]]),
        destination_port: u16::from_be_bytes([payload[2], payload[3]]),
        sequence: u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]),
        syn: payload[13] & 0x02 != 0,
        ack: payload[13] & 0x10 != 0,
        rst: payload[13] & 0x04 != 0,
        payload: &payload[header_len..],
    })
}

impl PolicyEngine {
    fn evaluate_ip(
        &mut self,
        direction: FrameDirection,
        packet: IpPacket<'_>,
        now: Duration,
    ) -> PolicyDecision {
        match packet.protocol {
            IPPROTO_UDP => match parse_udp(packet.payload) {
                Ok(udp) => {
                    self.evaluate_udp(direction, packet.source, packet.destination, udp, now)
                }
                Err(reason) => PolicyDecision::Deny(reason),
            },
            IPPROTO_TCP => match parse_tcp(packet.payload) {
                Ok(tcp) => {
                    self.evaluate_tcp(direction, packet.source, packet.destination, tcp, now)
                }
                Err(reason) => PolicyDecision::Deny(reason),
            },
            _ => PolicyDecision::Deny(DenyReason::UnsupportedProtocol),
        }
    }

    fn evaluate_udp(
        &mut self,
        direction: FrameDirection,
        source: IpAddr,
        destination: IpAddr,
        udp: UdpPacket<'_>,
        now: Duration,
    ) -> PolicyDecision {
        if self.is_dhcp(direction, source, destination, &udp) {
            return PolicyDecision::Allow(AllowReason::DhcpInfrastructure);
        }
        if direction == FrameDirection::GuestToNetwork
            && udp.destination_port == DNS_PORT
            && self.config.dns_servers.contains(&destination)
        {
            return self.start_dns_query(source, destination, &udp, now);
        }
        if direction == FrameDirection::NetworkToGuest
            && udp.source_port == DNS_PORT
            && self.config.dns_servers.contains(&source)
        {
            return self.finish_dns_response(source, destination, &udp, now);
        }
        if direction == FrameDirection::GuestToNetwork
            && self.config.control_addresses.contains(&destination)
        {
            return PolicyDecision::Deny(DenyReason::ControlEndpoint);
        }
        let key = flow_key(
            direction,
            source,
            destination,
            udp.source_port,
            udp.destination_port,
            Transport::Udp,
        );
        if direction == FrameDirection::NetworkToGuest {
            return self.allow_reverse(&key, now);
        }
        if let Some(FlowState::Allowed { expires }) = self.flows.get_mut(&key) {
            *expires = now.saturating_add(self.config.limits.flow_ttl);
            return PolicyDecision::Allow(AllowReason::ReverseFlow);
        }
        let permitted = match self.config.mode {
            PolicyMode::Unrestricted => Some(AllowReason::Unrestricted),
            PolicyMode::Allowlist if self.cidr_matches(destination) => Some(AllowReason::Cidr),
            PolicyMode::Allowlist => None,
            PolicyMode::Disabled => return PolicyDecision::Deny(DenyReason::Disabled),
        };
        let Some(reason) = permitted else {
            return if self.has_domain_binding(destination) {
                PolicyDecision::Deny(DenyReason::DomainRequiresTcpTls)
            } else {
                PolicyDecision::Deny(DenyReason::NotAllowlisted)
            };
        };
        if !self.insert_allowed_flow(key, now) {
            return PolicyDecision::Deny(DenyReason::StateLimit);
        }
        PolicyDecision::Allow(reason)
    }

    fn evaluate_tcp(
        &mut self,
        direction: FrameDirection,
        source: IpAddr,
        destination: IpAddr,
        tcp: TcpPacket<'_>,
        now: Duration,
    ) -> PolicyDecision {
        // TCP DNS is intentionally not treated as trusted observation state. It may be allowed by
        // an explicit CIDR/unrestricted rule, but cannot create domain bindings.
        let key = flow_key(
            direction,
            source,
            destination,
            tcp.source_port,
            tcp.destination_port,
            Transport::Tcp,
        );
        if direction == FrameDirection::NetworkToGuest {
            let decision = self.allow_reverse(&key, now);
            if !matches!(decision, PolicyDecision::Deny(DenyReason::NoReverseFlow)) {
                return decision;
            }
            if tcp.syn
                && !tcp.ack
                && self
                    .config
                    .published_tcp_ports
                    .contains(&tcp.destination_port)
            {
                if !self.insert_allowed_flow(key, now) {
                    return PolicyDecision::Deny(DenyReason::StateLimit);
                }
                return PolicyDecision::Allow(AllowReason::Preview);
            }
            return decision;
        }

        if let Some(state) = self.flows.remove(&key) {
            return self.evaluate_existing_tcp(key, state, &tcp, now);
        }
        if self.config.control_addresses.contains(&destination) {
            return PolicyDecision::Deny(DenyReason::ControlEndpoint);
        }
        if !tcp.syn {
            return PolicyDecision::Deny(DenyReason::NoReverseFlow);
        }
        match self.config.mode {
            PolicyMode::Unrestricted => {
                if !self.insert_allowed_flow(key, now) {
                    return PolicyDecision::Deny(DenyReason::StateLimit);
                }
                PolicyDecision::Allow(AllowReason::Unrestricted)
            }
            PolicyMode::Allowlist if self.cidr_matches(destination) => {
                if !self.insert_allowed_flow(key, now) {
                    return PolicyDecision::Deny(DenyReason::StateLimit);
                }
                PolicyDecision::Allow(AllowReason::Cidr)
            }
            PolicyMode::Allowlist
                if tcp.destination_port == HTTPS_PORT && self.has_domain_binding(destination) =>
            {
                if self.flows.len() >= self.config.limits.max_flows {
                    return PolicyDecision::Deny(DenyReason::StateLimit);
                }
                let initial_sequence = tcp.sequence.wrapping_add(u32::from(tcp.syn));
                let mut flow = TlsFlow {
                    initial_sequence,
                    bytes: Vec::new(),
                    expires: now.saturating_add(self.config.limits.pending_tls_ttl),
                };
                let progress = append_tls_segment(
                    &mut flow,
                    &tcp,
                    self.config.limits.max_tls_buffer_per_flow,
                    self.config.limits.max_total_tls_bytes - self.total_tls_bytes,
                );
                self.finish_tls_progress(key, flow, progress, tcp.rst, !tcp.payload.is_empty(), now)
            }
            PolicyMode::Allowlist => PolicyDecision::Deny(DenyReason::NotAllowlisted),
            PolicyMode::Disabled => PolicyDecision::Deny(DenyReason::Disabled),
        }
    }

    fn evaluate_existing_tcp(
        &mut self,
        key: FlowKey,
        state: FlowState,
        tcp: &TcpPacket<'_>,
        now: Duration,
    ) -> PolicyDecision {
        match state {
            FlowState::Allowed { .. } => {
                if !tcp.rst {
                    self.flows.insert(
                        key,
                        FlowState::Allowed {
                            expires: now.saturating_add(self.config.limits.flow_ttl),
                        },
                    );
                }
                PolicyDecision::Allow(AllowReason::ReverseFlow)
            }
            FlowState::PendingTls(mut flow) => {
                self.total_tls_bytes = self.total_tls_bytes.saturating_sub(flow.bytes.len());
                let progress = append_tls_segment(
                    &mut flow,
                    tcp,
                    self.config.limits.max_tls_buffer_per_flow,
                    self.config.limits.max_total_tls_bytes - self.total_tls_bytes,
                );
                self.finish_tls_progress(key, flow, progress, tcp.rst, !tcp.payload.is_empty(), now)
            }
        }
    }

    fn finish_tls_progress(
        &mut self,
        key: FlowKey,
        mut flow: TlsFlow,
        progress: Result<TlsParse, DenyReason>,
        rst: bool,
        has_outbound_payload: bool,
        now: Duration,
    ) -> PolicyDecision {
        if rst {
            return PolicyDecision::Allow(AllowReason::DomainHandshake);
        }
        match progress {
            Ok(TlsParse::NeedMore) => {
                flow.expires = now.saturating_add(self.config.limits.pending_tls_ttl);
                self.total_tls_bytes = self.total_tls_bytes.saturating_add(flow.bytes.len());
                self.flows.insert(key, FlowState::PendingTls(flow));
                if has_outbound_payload {
                    PolicyDecision::Hold(HoldReason::TlsClientHello)
                } else {
                    PolicyDecision::Allow(AllowReason::DomainHandshake)
                }
            }
            Ok(TlsParse::ClientHello { sni, ech }) => {
                if ech {
                    return PolicyDecision::Deny(DenyReason::TlsEch);
                }
                let Some(sni) = sni else {
                    return PolicyDecision::Deny(DenyReason::TlsNoSni);
                };
                if !self.domain_matches(&sni) || !self.binding_matches(key.remote_ip, &sni) {
                    return PolicyDecision::Deny(DenyReason::TlsSniMismatch);
                }
                self.flows.insert(
                    key,
                    FlowState::Allowed {
                        expires: now.saturating_add(self.config.limits.flow_ttl),
                    },
                );
                PolicyDecision::Allow(AllowReason::DomainSni)
            }
            Err(reason) => PolicyDecision::Deny(reason),
        }
    }

    fn insert_allowed_flow(&mut self, key: FlowKey, now: Duration) -> bool {
        if !self.flows.contains_key(&key) && self.flows.len() >= self.config.limits.max_flows {
            return false;
        }
        self.flows.insert(
            key,
            FlowState::Allowed {
                expires: now.saturating_add(self.config.limits.flow_ttl),
            },
        );
        true
    }

    fn allow_reverse(&mut self, key: &FlowKey, now: Duration) -> PolicyDecision {
        let Some(state) = self.flows.get_mut(key) else {
            return PolicyDecision::Deny(DenyReason::NoReverseFlow);
        };
        match state {
            FlowState::Allowed { expires } => {
                *expires = now.saturating_add(self.config.limits.flow_ttl);
                PolicyDecision::Allow(AllowReason::ReverseFlow)
            }
            FlowState::PendingTls(_) => PolicyDecision::Allow(AllowReason::DomainHandshake),
        }
    }

    fn cidr_matches(&self, address: IpAddr) -> bool {
        self.config.cidrs.iter().any(|cidr| cidr.contains(address))
    }

    fn domain_matches(&self, name: &str) -> bool {
        self.config
            .domains
            .iter()
            .any(|domain| domain.matches(name))
    }

    fn has_domain_binding(&self, address: IpAddr) -> bool {
        self.dns_bindings.get(&address).is_some_and(|bindings| {
            bindings
                .iter()
                .any(|binding| self.domain_matches(&binding.name))
        })
    }

    fn binding_matches(&self, address: IpAddr, name: &str) -> bool {
        self.dns_bindings.get(&address).is_some_and(|bindings| {
            bindings
                .iter()
                .any(|binding| binding.name.eq_ignore_ascii_case(name))
        })
    }

    fn is_dhcp(
        &self,
        direction: FrameDirection,
        source: IpAddr,
        destination: IpAddr,
        udp: &UdpPacket<'_>,
    ) -> bool {
        match (direction, source, destination) {
            (FrameDirection::GuestToNetwork, IpAddr::V4(source), IpAddr::V4(destination)) => {
                udp.source_port == DHCP_CLIENT_PORT
                    && udp.destination_port == DHCP_SERVER_PORT
                    && (source.is_unspecified() || source.is_private())
                    && (destination.is_broadcast()
                        || self.config.arp_targets.contains(&destination))
            }
            (FrameDirection::NetworkToGuest, IpAddr::V4(source), IpAddr::V4(destination)) => {
                udp.source_port == DHCP_SERVER_PORT
                    && udp.destination_port == DHCP_CLIENT_PORT
                    && self.config.arp_targets.contains(&source)
                    && (destination.is_broadcast() || destination.is_private())
            }
            _ => false,
        }
    }
}

fn flow_key(
    direction: FrameDirection,
    source: IpAddr,
    destination: IpAddr,
    source_port: u16,
    destination_port: u16,
    transport: Transport,
) -> FlowKey {
    match direction {
        FrameDirection::GuestToNetwork => FlowKey {
            guest_ip: source,
            guest_port: source_port,
            remote_ip: destination,
            remote_port: destination_port,
            transport,
        },
        FrameDirection::NetworkToGuest => FlowKey {
            guest_ip: destination,
            guest_port: destination_port,
            remote_ip: source,
            remote_port: source_port,
            transport,
        },
    }
}

#[derive(Debug)]
enum TlsParse {
    NeedMore,
    ClientHello { sni: Option<String>, ech: bool },
}

fn append_tls_segment(
    flow: &mut TlsFlow,
    tcp: &TcpPacket<'_>,
    per_flow_limit: usize,
    total_remaining: usize,
) -> Result<TlsParse, DenyReason> {
    if tcp.payload.is_empty() {
        return Ok(TlsParse::NeedMore);
    }
    let payload_sequence = tcp.sequence.wrapping_add(u32::from(tcp.syn));
    let offset = usize::try_from(payload_sequence.wrapping_sub(flow.initial_sequence))
        .map_err(|_| DenyReason::OutOfOrder)?;
    if offset > flow.bytes.len() {
        return Err(DenyReason::OutOfOrder);
    }
    let overlap = flow.bytes.len() - offset;
    let checked = overlap.min(tcp.payload.len());
    if flow.bytes[offset..offset + checked] != tcp.payload[..checked] {
        return Err(DenyReason::OutOfOrder);
    }
    let new_bytes = &tcp.payload[checked..];
    let new_len = flow
        .bytes
        .len()
        .checked_add(new_bytes.len())
        .ok_or(DenyReason::StateLimit)?;
    if new_len > per_flow_limit || new_bytes.len() > total_remaining {
        return Err(DenyReason::StateLimit);
    }
    flow.bytes.extend_from_slice(new_bytes);
    parse_tls_client_hello(&flow.bytes)
}

impl PolicyEngine {
    fn start_dns_query(
        &mut self,
        guest: IpAddr,
        server: IpAddr,
        udp: &UdpPacket<'_>,
        now: Duration,
    ) -> PolicyDecision {
        let Ok(query) = parse_dns_query(udp.payload) else {
            return PolicyDecision::Deny(DenyReason::Malformed);
        };
        if self.config.mode == PolicyMode::Allowlist && !self.domain_matches(&query.name) {
            return PolicyDecision::Deny(DenyReason::NotAllowlisted);
        }
        let key = DnsQueryKey {
            server,
            guest,
            guest_port: udp.source_port,
            id: query.id,
        };
        if !self.pending_dns.contains_key(&key)
            && self.pending_dns.len() >= self.config.limits.max_pending_dns
        {
            return PolicyDecision::Deny(DenyReason::StateLimit);
        }
        self.pending_dns.insert(
            key,
            PendingDns {
                name: query.name,
                expires: now.saturating_add(self.config.limits.dns_query_ttl),
            },
        );
        PolicyDecision::Allow(AllowReason::DnsInfrastructure)
    }

    fn finish_dns_response(
        &mut self,
        server: IpAddr,
        guest: IpAddr,
        udp: &UdpPacket<'_>,
        now: Duration,
    ) -> PolicyDecision {
        let Some(id) = udp
            .payload
            .get(0..2)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u16::from_be_bytes)
        else {
            return PolicyDecision::Deny(DenyReason::Malformed);
        };
        let key = DnsQueryKey {
            server,
            guest,
            guest_port: udp.destination_port,
            id,
        };
        let Some(pending) = self.pending_dns.remove(&key) else {
            return PolicyDecision::Deny(DenyReason::NoReverseFlow);
        };
        let Ok(response) = parse_dns_response(udp.payload) else {
            return PolicyDecision::Deny(DenyReason::Malformed);
        };
        if response.id != id || !response.question.eq_ignore_ascii_case(&pending.name) {
            return PolicyDecision::Deny(DenyReason::Infrastructure);
        }
        if self.config.mode == PolicyMode::Allowlist {
            self.observe_dns_answers(&pending.name, &response.answers, now);
        }
        PolicyDecision::Allow(AllowReason::DnsInfrastructure)
    }

    fn observe_dns_answers(&mut self, question: &str, answers: &[DnsRecord], now: Duration) {
        let max_ttl_secs = self
            .config
            .limits
            .max_dns_ttl
            .as_secs()
            .min(u64::from(u32::MAX));
        let mut reachable = BTreeMap::from([(question.to_owned(), max_ttl_secs)]);
        // CNAME records can appear in any answer order. Bound the fixed-point walk by the already
        // bounded record count to make cycles harmless.
        for _ in 0..answers.len() {
            let mut changed = false;
            for answer in answers {
                let DnsRecord::Cname { owner, target, ttl } = answer else {
                    continue;
                };
                let Some(parent_ttl) = reachable.get(owner).copied() else {
                    continue;
                };
                let ttl = parent_ttl.min(effective_dns_ttl(*ttl)).min(max_ttl_secs);
                if ttl == 0 {
                    continue;
                }
                match reachable.get(target) {
                    Some(existing) if *existing <= ttl => {}
                    _ => {
                        reachable.insert(target.clone(), ttl);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        for answer in answers {
            let DnsRecord::Address {
                owner,
                address,
                ttl,
            } = answer
            else {
                continue;
            };
            let Some(path_ttl) = reachable.get(owner).copied() else {
                continue;
            };
            let ttl = path_ttl.min(effective_dns_ttl(*ttl)).min(max_ttl_secs);
            if ttl == 0 {
                continue;
            }
            self.insert_dns_binding(
                *address,
                question,
                now.saturating_add(Duration::from_secs(ttl)),
            );
        }
    }

    fn insert_dns_binding(&mut self, address: IpAddr, name: &str, expires: Duration) {
        if let Some(existing) = self.dns_bindings.get_mut(&address).and_then(|bindings| {
            bindings
                .iter_mut()
                .find(|binding| binding.name.eq_ignore_ascii_case(name))
        }) {
            existing.expires = expires;
            return;
        }
        if self.dns_binding_count() >= self.config.limits.max_dns_bindings {
            return;
        }
        self.dns_bindings
            .entry(address)
            .or_default()
            .push(DnsBinding {
                name: name.to_owned(),
                expires,
            });
    }
}

fn effective_dns_ttl(ttl: u32) -> u64 {
    if ttl == 0 {
        ZERO_TTL_DNS_BINDING_GRACE_SECS
    } else {
        u64::from(ttl)
    }
}

struct DnsQuery {
    id: u16,
    name: String,
}

#[derive(Debug)]
struct DnsResponse {
    id: u16,
    question: String,
    answers: Vec<DnsRecord>,
}

#[derive(Debug)]
enum DnsRecord {
    Cname {
        owner: String,
        target: String,
        ttl: u32,
    },
    Address {
        owner: String,
        address: IpAddr,
        ttl: u32,
    },
}

fn parse_dns_query(message: &[u8]) -> Result<DnsQuery, ()> {
    let header = parse_dns_header(message)?;
    if header.flags & 0x8000 != 0
        || header.flags & 0x7800 != 0
        || header.question_count != 1
        || header.answer_count != 0
        || header.authority_count != 0
        || header.additional_count > 1
    {
        return Err(());
    }
    let mut cursor = 12;
    let name = read_dns_name(message, &mut cursor)?;
    if name.is_empty() {
        return Err(());
    }
    let (query_type, query_class) = read_dns_question_tail(message, &mut cursor)?;
    if !matches!(query_type, 1 | 28) || query_class != 1 {
        return Err(());
    }
    if header.additional_count == 1 {
        parse_dns_opt(message, &mut cursor)?;
    }
    if cursor != message.len() {
        return Err(());
    }
    Ok(DnsQuery {
        id: header.id,
        name,
    })
}

fn parse_dns_response(message: &[u8]) -> Result<DnsResponse, ()> {
    let header = parse_dns_header(message)?;
    if header.flags & 0x8000 == 0
        || header.flags & 0x7800 != 0
        || header.flags & 0x000f != 0
        || header.question_count != 1
    {
        return Err(());
    }
    let total_records = usize::from(header.answer_count)
        .checked_add(usize::from(header.authority_count))
        .and_then(|count| count.checked_add(usize::from(header.additional_count)))
        .ok_or(())?;
    if total_records > MAX_DNS_RECORDS {
        return Err(());
    }
    let mut cursor = 12;
    let question = read_dns_name(message, &mut cursor)?;
    if question.is_empty() {
        return Err(());
    }
    let (query_type, query_class) = read_dns_question_tail(message, &mut cursor)?;
    if !matches!(query_type, 1 | 28) || query_class != 1 {
        return Err(());
    }
    let mut answers = Vec::new();
    for index in 0..total_records {
        let owner = read_dns_name(message, &mut cursor)?;
        let fixed = message
            .get(cursor..cursor.checked_add(10).ok_or(())?)
            .ok_or(())?;
        let record_type = u16::from_be_bytes([fixed[0], fixed[1]]);
        let class = u16::from_be_bytes([fixed[2], fixed[3]]);
        let ttl = u32::from_be_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]);
        let data_len = usize::from(u16::from_be_bytes([fixed[8], fixed[9]]));
        cursor += 10;
        let data_start = cursor;
        let data_end = cursor.checked_add(data_len).ok_or(())?;
        let data = message.get(data_start..data_end).ok_or(())?;
        cursor = data_end;
        // Only answer-section IN records establish trust. Authority/additional records remain
        // syntactically checked but cannot inject bindings.
        if index >= usize::from(header.answer_count) || class != 1 {
            continue;
        }
        match record_type {
            1 if data.len() == 4 => answers.push(DnsRecord::Address {
                owner,
                address: IpAddr::V4(Ipv4Addr::new(data[0], data[1], data[2], data[3])),
                ttl,
            }),
            28 if data.len() == 16 => {
                let octets = <[u8; 16]>::try_from(data).map_err(|_| ())?;
                answers.push(DnsRecord::Address {
                    owner,
                    address: IpAddr::V6(Ipv6Addr::from(octets)),
                    ttl,
                });
            }
            5 => {
                let mut name_cursor = data_start;
                let target = read_dns_name(message, &mut name_cursor)?;
                if target.is_empty() || name_cursor != data_end {
                    return Err(());
                }
                answers.push(DnsRecord::Cname { owner, target, ttl });
            }
            _ => {}
        }
    }
    if cursor != message.len() {
        return Err(());
    }
    Ok(DnsResponse {
        id: header.id,
        question,
        answers,
    })
}

struct DnsHeader {
    id: u16,
    flags: u16,
    question_count: u16,
    answer_count: u16,
    authority_count: u16,
    additional_count: u16,
}

fn parse_dns_header(message: &[u8]) -> Result<DnsHeader, ()> {
    if message.len() < 12 || message.len() > MAX_DNS_MESSAGE {
        return Err(());
    }
    Ok(DnsHeader {
        id: u16::from_be_bytes([message[0], message[1]]),
        flags: u16::from_be_bytes([message[2], message[3]]),
        question_count: u16::from_be_bytes([message[4], message[5]]),
        answer_count: u16::from_be_bytes([message[6], message[7]]),
        authority_count: u16::from_be_bytes([message[8], message[9]]),
        additional_count: u16::from_be_bytes([message[10], message[11]]),
    })
}

fn read_dns_question_tail(message: &[u8], cursor: &mut usize) -> Result<(u16, u16), ()> {
    let end = cursor.checked_add(4).ok_or(())?;
    let tail = message.get(*cursor..end).ok_or(())?;
    *cursor = end;
    Ok((
        u16::from_be_bytes([tail[0], tail[1]]),
        u16::from_be_bytes([tail[2], tail[3]]),
    ))
}

fn parse_dns_opt(message: &[u8], cursor: &mut usize) -> Result<(), ()> {
    let owner = read_dns_name(message, cursor)?;
    if !owner.is_empty() {
        return Err(());
    }
    let fixed = message
        .get(*cursor..cursor.checked_add(10).ok_or(())?)
        .ok_or(())?;
    if u16::from_be_bytes([fixed[0], fixed[1]]) != 41 {
        return Err(());
    }
    let data_len = usize::from(u16::from_be_bytes([fixed[8], fixed[9]]));
    *cursor = cursor
        .checked_add(10)
        .and_then(|position| position.checked_add(data_len))
        .filter(|end| *end <= message.len())
        .ok_or(())?;
    Ok(())
}

fn read_dns_name(message: &[u8], cursor: &mut usize) -> Result<String, ()> {
    let mut position = *cursor;
    let mut consumed = None;
    let mut pointer_count = 0;
    let mut labels = Vec::new();
    let mut encoded_len = 0_usize;
    loop {
        let length = *message.get(position).ok_or(())?;
        if length & 0xc0 == 0xc0 {
            let next = *message.get(position.checked_add(1).ok_or(())?).ok_or(())?;
            consumed.get_or_insert(position.checked_add(2).ok_or(())?);
            position = usize::from(u16::from(length & 0x3f) << 8 | u16::from(next));
            pointer_count += 1;
            if pointer_count > MAX_DNS_NAME_POINTERS {
                return Err(());
            }
            continue;
        }
        if length & 0xc0 != 0 {
            return Err(());
        }
        position = position.checked_add(1).ok_or(())?;
        if length == 0 {
            *cursor = consumed.unwrap_or(position);
            break;
        }
        let length = usize::from(length);
        if length > 63 {
            return Err(());
        }
        let end = position.checked_add(length).ok_or(())?;
        let label = message.get(position..end).ok_or(())?;
        if label.is_empty()
            || label[0] == b'-'
            || label[label.len() - 1] == b'-'
            || !label
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            return Err(());
        }
        encoded_len = encoded_len
            .checked_add(length + usize::from(!labels.is_empty()))
            .ok_or(())?;
        if encoded_len > MAX_DOMAIN_LEN {
            return Err(());
        }
        labels.push(String::from_utf8(label.to_ascii_lowercase()).map_err(|_| ())?);
        position = end;
    }
    Ok(labels.join("."))
}

fn parse_tls_client_hello(stream: &[u8]) -> Result<TlsParse, DenyReason> {
    let mut offset = 0_usize;
    let mut handshake = Vec::new();
    loop {
        let Some(header_end) = offset.checked_add(5) else {
            return Err(DenyReason::TlsInvalid);
        };
        if stream.len() < header_end {
            return Ok(TlsParse::NeedMore);
        }
        if stream[offset] != 22 || stream[offset + 1] != 3 {
            return Err(DenyReason::TlsInvalid);
        }
        let record_len = usize::from(u16::from_be_bytes([stream[offset + 3], stream[offset + 4]]));
        if record_len == 0 || record_len > MAX_TLS_RECORD {
            return Err(DenyReason::TlsInvalid);
        }
        let record_end = header_end
            .checked_add(record_len)
            .ok_or(DenyReason::TlsInvalid)?;
        if stream.len() < record_end {
            return Ok(TlsParse::NeedMore);
        }
        handshake.extend_from_slice(&stream[header_end..record_end]);
        if handshake.len() >= 4 {
            if handshake[0] != 1 {
                return Err(DenyReason::TlsInvalid);
            }
            let hello_len = (usize::from(handshake[1]) << 16)
                | (usize::from(handshake[2]) << 8)
                | usize::from(handshake[3]);
            if hello_len > MAX_TLS_BUFFER_HARD.saturating_sub(4) {
                return Err(DenyReason::TlsInvalid);
            }
            let message_end = 4_usize
                .checked_add(hello_len)
                .ok_or(DenyReason::TlsInvalid)?;
            if handshake.len() >= message_end {
                return parse_client_hello_body(&handshake[4..message_end]);
            }
        }
        offset = record_end;
        if offset == stream.len() {
            return Ok(TlsParse::NeedMore);
        }
    }
}

fn parse_client_hello_body(body: &[u8]) -> Result<TlsParse, DenyReason> {
    if body.len() < 34 || body[0] != 3 {
        return Err(DenyReason::TlsInvalid);
    }
    let mut cursor = 34;
    let session_len = usize::from(*body.get(cursor).ok_or(DenyReason::TlsInvalid)?);
    cursor = cursor
        .checked_add(1 + session_len)
        .ok_or(DenyReason::TlsInvalid)?;
    let cipher_len = read_tls_u16(body, &mut cursor)?;
    if cipher_len < 2 || cipher_len % 2 != 0 {
        return Err(DenyReason::TlsInvalid);
    }
    cursor = cursor
        .checked_add(cipher_len)
        .filter(|end| *end <= body.len())
        .ok_or(DenyReason::TlsInvalid)?;
    let compression_len = usize::from(*body.get(cursor).ok_or(DenyReason::TlsInvalid)?);
    cursor = cursor
        .checked_add(1 + compression_len)
        .filter(|end| *end <= body.len())
        .ok_or(DenyReason::TlsInvalid)?;
    let all_extensions_len = read_tls_u16(body, &mut cursor)?;
    let extensions_end = cursor
        .checked_add(all_extensions_len)
        .filter(|end| *end == body.len())
        .ok_or(DenyReason::TlsInvalid)?;
    let mut sni = None;
    let mut ech = false;
    while cursor < extensions_end {
        let extension_type = read_tls_u16(body, &mut cursor)?;
        let item_len = read_tls_u16(body, &mut cursor)?;
        let end = cursor
            .checked_add(item_len)
            .filter(|end| *end <= extensions_end)
            .ok_or(DenyReason::TlsInvalid)?;
        match extension_type {
            0 => {
                if sni.is_some() {
                    return Err(DenyReason::TlsInvalid);
                }
                sni = Some(parse_sni_extension(&body[cursor..end])?);
            }
            0xfe0d | 0xffce => ech = true,
            _ => {}
        }
        cursor = end;
    }
    Ok(TlsParse::ClientHello {
        sni: sni.flatten(),
        ech,
    })
}

fn parse_sni_extension(extension: &[u8]) -> Result<Option<String>, DenyReason> {
    if extension.len() < 2 {
        return Err(DenyReason::TlsInvalid);
    }
    let list_len = usize::from(u16::from_be_bytes([extension[0], extension[1]]));
    if list_len != extension.len() - 2 {
        return Err(DenyReason::TlsInvalid);
    }
    let mut cursor = 2;
    let mut host = None;
    while cursor < extension.len() {
        let name_type = *extension.get(cursor).ok_or(DenyReason::TlsInvalid)?;
        cursor += 1;
        let name_len = read_tls_u16(extension, &mut cursor)?;
        let end = cursor
            .checked_add(name_len)
            .filter(|end| *end <= extension.len())
            .ok_or(DenyReason::TlsInvalid)?;
        if name_type == 0 {
            if host.is_some() {
                return Err(DenyReason::TlsInvalid);
            }
            let name = std::str::from_utf8(&extension[cursor..end])
                .ok()
                .and_then(normalize_domain)
                .ok_or(DenyReason::TlsInvalid)?;
            host = Some(name);
        }
        cursor = end;
    }
    Ok(host)
}

fn read_tls_u16(bytes: &[u8], cursor: &mut usize) -> Result<usize, DenyReason> {
    let end = cursor.checked_add(2).ok_or(DenyReason::TlsInvalid)?;
    let value = bytes.get(*cursor..end).ok_or(DenyReason::TlsInvalid)?;
    *cursor = end;
    Ok(usize::from(u16::from_be_bytes([value[0], value[1]])))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUEST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
    const DNS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
    const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
    const CONTROL: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 254);
    const REMOTE: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 9);

    fn config(cidrs: &[&str], domains: &[&str], limits: PolicyLimits) -> PolicyConfig {
        PolicyConfig::new(
            PolicyMode::Allowlist,
            cidrs.iter().map(|value| value.parse().unwrap()),
            domains.iter().map(|value| value.parse().unwrap()),
            [IpAddr::V4(DNS)],
            [GATEWAY, DNS],
            [IpAddr::V4(CONTROL)],
            limits,
        )
        .unwrap()
    }

    fn ethernet(ether_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0; ETHERNET_HEADER_LEN];
        frame[12..14].copy_from_slice(&ether_type.to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    fn ipv4(
        source: Ipv4Addr,
        destination: Ipv4Addr,
        protocol: u8,
        payload: &[u8],
        fragment: u16,
    ) -> Vec<u8> {
        let mut packet = vec![0_u8; 20];
        packet[0] = 0x45;
        packet[2..4].copy_from_slice(&u16::try_from(20 + payload.len()).unwrap().to_be_bytes());
        packet[6..8].copy_from_slice(&fragment.to_be_bytes());
        packet[8] = 64;
        packet[9] = protocol;
        packet[12..16].copy_from_slice(&source.octets());
        packet[16..20].copy_from_slice(&destination.octets());
        let header_checksum = checksum(&packet);
        packet[10..12].copy_from_slice(&header_checksum.to_be_bytes());
        packet.extend_from_slice(payload);
        ethernet(ETHERTYPE_IPV4, &packet)
    }

    fn ipv6(source: Ipv6Addr, destination: Ipv6Addr, protocol: u8, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0_u8; 40];
        packet[0] = 0x60;
        packet[4..6].copy_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
        packet[6] = protocol;
        packet[7] = 64;
        packet[8..24].copy_from_slice(&source.octets());
        packet[24..40].copy_from_slice(&destination.octets());
        packet.extend_from_slice(payload);
        ethernet(ETHERTYPE_IPV6, &packet)
    }

    fn udp(source_port: u16, destination_port: u16, payload: &[u8]) -> Vec<u8> {
        let mut packet = Vec::with_capacity(8 + payload.len());
        packet.extend_from_slice(&source_port.to_be_bytes());
        packet.extend_from_slice(&destination_port.to_be_bytes());
        packet.extend_from_slice(&u16::try_from(8 + payload.len()).unwrap().to_be_bytes());
        packet.extend_from_slice(&[0, 0]);
        packet.extend_from_slice(payload);
        packet
    }

    fn tcp(
        source_port: u16,
        destination_port: u16,
        sequence: u32,
        flags: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut packet = vec![0_u8; 20];
        packet[0..2].copy_from_slice(&source_port.to_be_bytes());
        packet[2..4].copy_from_slice(&destination_port.to_be_bytes());
        packet[4..8].copy_from_slice(&sequence.to_be_bytes());
        packet[12] = 5 << 4;
        packet[13] = flags;
        packet.extend_from_slice(payload);
        packet
    }

    fn outbound_udp(remote: Ipv4Addr, guest_port: u16, remote_port: u16) -> Vec<u8> {
        ipv4(
            GUEST,
            remote,
            IPPROTO_UDP,
            &udp(guest_port, remote_port, b"data"),
            0,
        )
    }

    fn dns_name(name: &str) -> Vec<u8> {
        let mut encoded = Vec::new();
        for label in name.split('.') {
            encoded.push(u8::try_from(label.len()).unwrap());
            encoded.extend_from_slice(label.as_bytes());
        }
        encoded.push(0);
        encoded
    }

    fn dns_query(id: u16, name: &str, query_type: u16) -> Vec<u8> {
        let mut message = Vec::new();
        message.extend_from_slice(&id.to_be_bytes());
        message.extend_from_slice(&0x0100_u16.to_be_bytes());
        message.extend_from_slice(&1_u16.to_be_bytes());
        message.extend_from_slice(&[0; 6]);
        message.extend_from_slice(&dns_name(name));
        message.extend_from_slice(&query_type.to_be_bytes());
        message.extend_from_slice(&1_u16.to_be_bytes());
        message
    }

    enum TestAnswer<'a> {
        A(&'a str, Ipv4Addr, u32),
        Aaaa(&'a str, Ipv6Addr, u32),
        Cname(&'a str, &'a str, u32),
    }

    fn dns_response(
        id: u16,
        question: &str,
        query_type: u16,
        answers: &[TestAnswer<'_>],
    ) -> Vec<u8> {
        let mut message = Vec::new();
        message.extend_from_slice(&id.to_be_bytes());
        message.extend_from_slice(&0x8180_u16.to_be_bytes());
        message.extend_from_slice(&1_u16.to_be_bytes());
        message.extend_from_slice(&u16::try_from(answers.len()).unwrap().to_be_bytes());
        message.extend_from_slice(&[0; 4]);
        message.extend_from_slice(&dns_name(question));
        message.extend_from_slice(&query_type.to_be_bytes());
        message.extend_from_slice(&1_u16.to_be_bytes());
        for answer in answers {
            let (owner, record_type, ttl, data) = match answer {
                TestAnswer::A(owner, address, ttl) => {
                    (*owner, 1_u16, *ttl, address.octets().to_vec())
                }
                TestAnswer::Aaaa(owner, address, ttl) => {
                    (*owner, 28_u16, *ttl, address.octets().to_vec())
                }
                TestAnswer::Cname(owner, target, ttl) => (*owner, 5_u16, *ttl, dns_name(target)),
            };
            message.extend_from_slice(&dns_name(owner));
            message.extend_from_slice(&record_type.to_be_bytes());
            message.extend_from_slice(&1_u16.to_be_bytes());
            message.extend_from_slice(&ttl.to_be_bytes());
            message.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
            message.extend_from_slice(&data);
        }
        message
    }

    fn exchange_dns(
        engine: &mut PolicyEngine,
        name: &str,
        query_type: u16,
        answers: &[TestAnswer<'_>],
        now: Duration,
    ) {
        let id = 0x1234;
        let query = ipv4(
            GUEST,
            DNS,
            IPPROTO_UDP,
            &udp(53_000, DNS_PORT, &dns_query(id, name, query_type)),
            0,
        );
        assert_eq!(
            engine.evaluate_ethernet(FrameDirection::GuestToNetwork, &query, now),
            PolicyDecision::Allow(AllowReason::DnsInfrastructure)
        );
        let response = ipv4(
            DNS,
            GUEST,
            IPPROTO_UDP,
            &udp(
                DNS_PORT,
                53_000,
                &dns_response(id, name, query_type, answers),
            ),
            0,
        );
        assert_eq!(
            engine.evaluate_ethernet(FrameDirection::NetworkToGuest, &response, now),
            PolicyDecision::Allow(AllowReason::DnsInfrastructure)
        );
    }

    fn client_hello(sni: Option<&str>, ech: bool) -> Vec<u8> {
        let mut extensions = Vec::new();
        if let Some(sni) = sni {
            let name = sni.as_bytes();
            let mut extension = Vec::new();
            extension.extend_from_slice(&u16::try_from(name.len() + 3).unwrap().to_be_bytes());
            extension.push(0);
            extension.extend_from_slice(&u16::try_from(name.len()).unwrap().to_be_bytes());
            extension.extend_from_slice(name);
            extensions.extend_from_slice(&0_u16.to_be_bytes());
            extensions.extend_from_slice(&u16::try_from(extension.len()).unwrap().to_be_bytes());
            extensions.extend_from_slice(&extension);
        }
        if ech {
            extensions.extend_from_slice(&0xfe0d_u16.to_be_bytes());
            extensions.extend_from_slice(&1_u16.to_be_bytes());
            extensions.push(0);
        }
        let mut body = Vec::new();
        body.extend_from_slice(&[3, 3]);
        body.extend_from_slice(&[0; 32]);
        body.push(0);
        body.extend_from_slice(&2_u16.to_be_bytes());
        body.extend_from_slice(&0x1301_u16.to_be_bytes());
        body.push(1);
        body.push(0);
        body.extend_from_slice(&u16::try_from(extensions.len()).unwrap().to_be_bytes());
        body.extend_from_slice(&extensions);
        let mut handshake = vec![1];
        let body_len = u32::try_from(body.len()).unwrap().to_be_bytes();
        handshake.extend_from_slice(&body_len[1..]);
        handshake.extend_from_slice(&body);
        let mut record = vec![22, 3, 3];
        record.extend_from_slice(&u16::try_from(handshake.len()).unwrap().to_be_bytes());
        record.extend_from_slice(&handshake);
        record
    }

    fn tls_syn(engine: &mut PolicyEngine, remote: Ipv4Addr, guest_port: u16) -> PolicyDecision {
        let syn = ipv4(
            GUEST,
            remote,
            IPPROTO_TCP,
            &tcp(guest_port, HTTPS_PORT, 100, 0x02, &[]),
            0,
        );
        engine.evaluate_ethernet(FrameDirection::GuestToNetwork, &syn, Duration::ZERO)
    }

    #[test]
    fn cidr_and_domain_patterns_are_strict() {
        let cidr: Cidr = "192.0.2.0/24".parse().unwrap();
        assert!(cidr.contains("192.0.2.255".parse().unwrap()));
        assert!(!cidr.contains("192.0.3.1".parse().unwrap()));
        assert!("2001:db8::/129".parse::<Cidr>().is_err());

        let wildcard: DomainPattern = "*.example.com".parse().unwrap();
        assert!(wildcard.matches("api.example.com"));
        assert!(wildcard.matches("deep.api.example.com"));
        assert!(!wildcard.matches("example.com"));
        assert!(!wildcard.matches("badexample.com"));
        assert!("*example.com".parse::<DomainPattern>().is_err());
    }

    #[test]
    fn cidr_allows_ipv4_ipv6_and_only_correlated_reverse_flows() {
        let mut engine = PolicyEngine::new(config(
            &["203.0.113.0/24", "2001:db8::/32"],
            &[],
            PolicyLimits::default(),
        ));
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &outbound_udp(REMOTE, 50_000, 123),
                Duration::ZERO,
            ),
            PolicyDecision::Allow(AllowReason::Cidr)
        );
        let reverse = ipv4(REMOTE, GUEST, IPPROTO_UDP, &udp(123, 50_000, b"reply"), 0);
        assert_eq!(
            engine.evaluate_ethernet(FrameDirection::NetworkToGuest, &reverse, Duration::ZERO),
            PolicyDecision::Allow(AllowReason::ReverseFlow)
        );
        let unsolicited = ipv4(REMOTE, GUEST, IPPROTO_UDP, &udp(123, 50_001, b"reply"), 0);
        assert_eq!(
            engine.evaluate_ethernet(FrameDirection::NetworkToGuest, &unsolicited, Duration::ZERO),
            PolicyDecision::Deny(DenyReason::NoReverseFlow)
        );

        let v6 = ipv6(
            "fd00::2".parse().unwrap(),
            "2001:db8::1".parse().unwrap(),
            IPPROTO_UDP,
            &udp(40_000, 123, b"v6"),
        );
        assert!(
            engine
                .evaluate_ethernet(FrameDirection::GuestToNetwork, &v6, Duration::ZERO)
                .is_allowed()
        );
    }

    #[test]
    fn malformed_fragments_and_control_endpoint_fail_closed() {
        let mut engine = PolicyEngine::new(config(&["0.0.0.0/0"], &[], PolicyLimits::default()));
        assert_eq!(
            engine.evaluate_ethernet(FrameDirection::GuestToNetwork, &[0; 13], Duration::ZERO),
            PolicyDecision::Deny(DenyReason::Malformed)
        );
        let fragment = ipv4(GUEST, REMOTE, IPPROTO_UDP, &udp(1000, 1000, b"x"), 0x2000);
        assert_eq!(
            engine.evaluate_ethernet(FrameDirection::GuestToNetwork, &fragment, Duration::ZERO),
            PolicyDecision::Deny(DenyReason::Fragment)
        );
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &outbound_udp(CONTROL, 1000, 80),
                Duration::ZERO,
            ),
            PolicyDecision::Deny(DenyReason::ControlEndpoint)
        );
        let v6_fragment = ipv6(
            "fd00::2".parse().unwrap(),
            "2001:db8::1".parse().unwrap(),
            44,
            &[0; 8],
        );
        assert_eq!(
            engine.evaluate_ethernet(FrameDirection::GuestToNetwork, &v6_fragment, Duration::ZERO),
            PolicyDecision::Deny(DenyReason::Fragment)
        );
    }

    #[test]
    fn infrastructure_is_narrowly_limited() {
        let mut engine = PolicyEngine::new(config(&[], &["example.com"], PolicyLimits::default()));
        let mut arp = vec![0_u8; 28];
        arp[0..2].copy_from_slice(&1_u16.to_be_bytes());
        arp[2..4].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        arp[4] = 6;
        arp[5] = 4;
        arp[6..8].copy_from_slice(&1_u16.to_be_bytes());
        arp[24..28].copy_from_slice(&GATEWAY.octets());
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &ethernet(ETHERTYPE_ARP, &arp),
                Duration::ZERO,
            ),
            PolicyDecision::Allow(AllowReason::ArpInfrastructure)
        );
        arp[24..28].copy_from_slice(&Ipv4Addr::new(10, 0, 2, 99).octets());
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &ethernet(ETHERTYPE_ARP, &arp),
                Duration::ZERO,
            ),
            PolicyDecision::Deny(DenyReason::Infrastructure)
        );
        let discover = ipv4(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            IPPROTO_UDP,
            &udp(DHCP_CLIENT_PORT, DHCP_SERVER_PORT, b"discover"),
            0,
        );
        assert_eq!(
            engine.evaluate_ethernet(FrameDirection::GuestToNetwork, &discover, Duration::ZERO),
            PolicyDecision::Allow(AllowReason::DhcpInfrastructure)
        );
        let untrusted_dns = ipv4(
            GUEST,
            GATEWAY,
            IPPROTO_UDP,
            &udp(53000, DNS_PORT, &dns_query(1, "example.com", 1)),
            0,
        );
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &untrusted_dns,
                Duration::ZERO
            ),
            PolicyDecision::Deny(DenyReason::NotAllowlisted)
        );
    }

    #[test]
    fn dns_and_dhcp_are_narrow_exceptions_on_the_control_address() {
        let policy = PolicyConfig::new(
            PolicyMode::Allowlist,
            [],
            ["example.com".parse().unwrap()],
            [IpAddr::V4(CONTROL)],
            [CONTROL],
            [IpAddr::V4(CONTROL)],
            PolicyLimits::default(),
        )
        .unwrap();
        let mut engine = PolicyEngine::new(policy);
        let query = ipv4(
            GUEST,
            CONTROL,
            IPPROTO_UDP,
            &udp(53_000, DNS_PORT, &dns_query(7, "example.com", 1)),
            0,
        );
        assert_eq!(
            engine.evaluate_ethernet(FrameDirection::GuestToNetwork, &query, Duration::ZERO),
            PolicyDecision::Allow(AllowReason::DnsInfrastructure)
        );
        let renew = ipv4(
            GUEST,
            CONTROL,
            IPPROTO_UDP,
            &udp(DHCP_CLIENT_PORT, DHCP_SERVER_PORT, b"renew"),
            0,
        );
        assert_eq!(
            engine.evaluate_ethernet(FrameDirection::GuestToNetwork, &renew, Duration::ZERO),
            PolicyDecision::Allow(AllowReason::DhcpInfrastructure)
        );
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &outbound_udp(CONTROL, 53_001, 80),
                Duration::ZERO,
            ),
            PolicyDecision::Deny(DenyReason::ControlEndpoint)
        );
    }

    #[test]
    fn trusted_dns_cname_and_aaaa_bindings_expire() {
        let mut engine =
            PolicyEngine::new(config(&[], &["*.example.com"], PolicyLimits::default()));
        let remote_v6: Ipv6Addr = "2001:db8::44".parse().unwrap();
        exchange_dns(
            &mut engine,
            "api.example.com",
            28,
            &[
                TestAnswer::Aaaa("edge.cdn.test", remote_v6, 20),
                TestAnswer::Cname("api.example.com", "edge.cdn.test", 2),
            ],
            Duration::ZERO,
        );
        assert_eq!(engine.dns_binding_count(), 1);
        let syn = ipv6(
            "fd00::2".parse().unwrap(),
            remote_v6,
            IPPROTO_TCP,
            &tcp(40_000, HTTPS_PORT, 10, 0x02, &[]),
        );
        assert_eq!(
            engine.evaluate_ethernet(FrameDirection::GuestToNetwork, &syn, Duration::from_secs(1)),
            PolicyDecision::Allow(AllowReason::DomainHandshake)
        );
        let expired_syn = ipv6(
            "fd00::2".parse().unwrap(),
            remote_v6,
            IPPROTO_TCP,
            &tcp(40_001, HTTPS_PORT, 10, 0x02, &[]),
        );
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &expired_syn,
                Duration::from_secs(3),
            ),
            PolicyDecision::Deny(DenyReason::NotAllowlisted)
        );
        assert_eq!(engine.dns_binding_count(), 0);
    }

    #[test]
    fn zero_ttl_dns_answer_has_only_a_short_connection_grace() {
        let mut engine = PolicyEngine::new(config(&[], &["example.com"], PolicyLimits::default()));
        exchange_dns(
            &mut engine,
            "example.com",
            1,
            &[TestAnswer::A("example.com", REMOTE, 0)],
            Duration::ZERO,
        );
        assert_eq!(engine.dns_binding_count(), 1);

        let immediate = ipv4(
            GUEST,
            REMOTE,
            IPPROTO_TCP,
            &tcp(40_000, HTTPS_PORT, 100, 0x02, &[]),
            0,
        );
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &immediate,
                Duration::from_secs(ZERO_TTL_DNS_BINDING_GRACE_SECS - 1),
            ),
            PolicyDecision::Allow(AllowReason::DomainHandshake)
        );

        let expired = ipv4(
            GUEST,
            REMOTE,
            IPPROTO_TCP,
            &tcp(40_001, HTTPS_PORT, 100, 0x02, &[]),
            0,
        );
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &expired,
                Duration::from_secs(ZERO_TTL_DNS_BINDING_GRACE_SECS + 1),
            ),
            PolicyDecision::Deny(DenyReason::NotAllowlisted)
        );
        assert_eq!(engine.dns_binding_count(), 0);
    }

    #[test]
    fn domain_tls_requires_dns_and_matching_sni_across_retransmissions() {
        let mut engine =
            PolicyEngine::new(config(&[], &["*.example.com"], PolicyLimits::default()));
        exchange_dns(
            &mut engine,
            "api.example.com",
            1,
            &[TestAnswer::A("api.example.com", REMOTE, 60)],
            Duration::ZERO,
        );
        assert_eq!(
            tls_syn(&mut engine, REMOTE, 40_000),
            PolicyDecision::Allow(AllowReason::DomainHandshake)
        );
        let hello = client_hello(Some("api.example.com"), false);
        let split = 23;
        let first = ipv4(
            GUEST,
            REMOTE,
            IPPROTO_TCP,
            &tcp(40_000, HTTPS_PORT, 101, 0x18, &hello[..split]),
            0,
        );
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &first,
                Duration::from_millis(1)
            ),
            PolicyDecision::Hold(HoldReason::TlsClientHello)
        );
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &first,
                Duration::from_millis(2)
            ),
            PolicyDecision::Hold(HoldReason::TlsClientHello)
        );
        let second = ipv4(
            GUEST,
            REMOTE,
            IPPROTO_TCP,
            &tcp(
                40_000,
                HTTPS_PORT,
                101 + u32::try_from(split).unwrap(),
                0x18,
                &hello[split..],
            ),
            0,
        );
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &second,
                Duration::from_millis(3)
            ),
            PolicyDecision::Allow(AllowReason::DomainSni)
        );
        let reverse = ipv4(
            REMOTE,
            GUEST,
            IPPROTO_TCP,
            &tcp(HTTPS_PORT, 40_000, 1, 0x10, b"server"),
            0,
        );
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::NetworkToGuest,
                &reverse,
                Duration::from_millis(4)
            ),
            PolicyDecision::Allow(AllowReason::ReverseFlow)
        );
    }

    fn engine_with_binding() -> PolicyEngine {
        let mut engine =
            PolicyEngine::new(config(&[], &["*.example.com"], PolicyLimits::default()));
        exchange_dns(
            &mut engine,
            "api.example.com",
            1,
            &[TestAnswer::A("api.example.com", REMOTE, 60)],
            Duration::ZERO,
        );
        engine
    }

    #[test]
    fn domain_only_denies_udp_quic_ech_no_sni_and_mismatched_sni() {
        let mut engine = engine_with_binding();
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &outbound_udp(REMOTE, 40_000, HTTPS_PORT),
                Duration::ZERO,
            ),
            PolicyDecision::Deny(DenyReason::DomainRequiresTcpTls)
        );

        for (port, hello, expected) in [
            (
                40_001,
                client_hello(Some("api.example.com"), true),
                DenyReason::TlsEch,
            ),
            (40_002, client_hello(None, false), DenyReason::TlsNoSni),
            (
                40_003,
                client_hello(Some("other.example.com"), false),
                DenyReason::TlsSniMismatch,
            ),
        ] {
            assert!(tls_syn(&mut engine, REMOTE, port).is_allowed());
            let frame = ipv4(
                GUEST,
                REMOTE,
                IPPROTO_TCP,
                &tcp(port, HTTPS_PORT, 101, 0x18, &hello),
                0,
            );
            assert_eq!(
                engine.evaluate_ethernet(
                    FrameDirection::GuestToNetwork,
                    &frame,
                    Duration::from_millis(1),
                ),
                PolicyDecision::Deny(expected)
            );
        }
    }

    #[test]
    fn state_limits_and_pending_tls_expiry_are_enforced() {
        let limits = PolicyLimits {
            max_flows: 1,
            max_pending_dns: 1,
            max_dns_bindings: 1,
            max_tls_buffer_per_flow: 512,
            max_total_tls_bytes: 512,
            pending_tls_ttl: Duration::from_secs(1),
            ..PolicyLimits::default()
        };
        let mut engine = PolicyEngine::new(config(&["203.0.113.0/24"], &[], limits));
        assert!(
            engine
                .evaluate_ethernet(
                    FrameDirection::GuestToNetwork,
                    &outbound_udp(REMOTE, 40_000, 1),
                    Duration::ZERO,
                )
                .is_allowed()
        );
        assert_eq!(engine.flow_count(), 1);
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &outbound_udp(REMOTE, 40_001, 1),
                Duration::ZERO,
            ),
            PolicyDecision::Deny(DenyReason::StateLimit)
        );

        let invalid_limits = PolicyLimits {
            max_total_tls_bytes: 1,
            ..PolicyLimits::default()
        };
        assert!(
            PolicyConfig::new(PolicyMode::Unrestricted, [], [], [], [], [], invalid_limits,)
                .is_err()
        );
    }

    #[test]
    fn published_tcp_port_allows_only_inbound_initiated_preview_flow() {
        let config = config(&["203.0.113.0/24"], &[], PolicyLimits::default())
            .with_guest_ipv4_addresses([GUEST])
            .unwrap()
            .with_published_tcp_ports([8080])
            .unwrap();
        let mut engine = PolicyEngine::new(config);
        let unpublished = ipv4(
            REMOTE,
            GUEST,
            IPPROTO_TCP,
            &tcp(50_000, 8081, 1, 0x02, &[]),
            0,
        );
        assert_eq!(
            engine.evaluate_ethernet(FrameDirection::NetworkToGuest, &unpublished, Duration::ZERO,),
            PolicyDecision::Deny(DenyReason::NoReverseFlow)
        );

        let inbound = ipv4(
            CONTROL,
            GUEST,
            IPPROTO_TCP,
            &tcp(50_000, 8080, 1, 0x02, &[]),
            0,
        );
        assert_eq!(
            engine.evaluate_ethernet(FrameDirection::NetworkToGuest, &inbound, Duration::ZERO,),
            PolicyDecision::Allow(AllowReason::Preview)
        );
        let mut request = vec![0_u8; 28];
        request[0..2].copy_from_slice(&1_u16.to_be_bytes());
        request[2..4].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        request[4] = 6;
        request[5] = 4;
        request[6..8].copy_from_slice(&1_u16.to_be_bytes());
        request[14..18].copy_from_slice(&GATEWAY.octets());
        request[24..28].copy_from_slice(&GUEST.octets());
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::NetworkToGuest,
                &ethernet(ETHERTYPE_ARP, &request),
                Duration::ZERO,
            ),
            PolicyDecision::Allow(AllowReason::ArpInfrastructure)
        );
        request[6..8].copy_from_slice(&2_u16.to_be_bytes());
        request[14..18].copy_from_slice(&GUEST.octets());
        request[24..28].copy_from_slice(&GATEWAY.octets());
        assert_eq!(
            engine.evaluate_ethernet(
                FrameDirection::GuestToNetwork,
                &ethernet(ETHERTYPE_ARP, &request),
                Duration::ZERO,
            ),
            PolicyDecision::Allow(AllowReason::ArpInfrastructure)
        );
        let reply = ipv4(
            GUEST,
            CONTROL,
            IPPROTO_TCP,
            &tcp(8080, 50_000, 10, 0x12, &[]),
            0,
        );
        assert_eq!(
            engine.evaluate_ethernet(FrameDirection::GuestToNetwork, &reply, Duration::ZERO,),
            PolicyDecision::Allow(AllowReason::ReverseFlow)
        );
    }
}
