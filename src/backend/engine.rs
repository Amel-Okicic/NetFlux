//! WinDivert network-layer capture + token-bucket traffic shaping.
//!
//! Two threads cooperate here:
//!   * [`run_engine`] receives every packet, attributes it to a process via the
//!     flow table, looks up that process's rule, and applies a verdict: pass,
//!     drop (block), or, if a speed limit applies and there aren't enough
//!     tokens, enqueue the packet in a per-`(exe, direction)` bucket for later
//!     injection.
//!   * [`run_drainer`] wakes every few milliseconds, refills the buckets, and
//!     re-injects queued packets as tokens become available.
//!
//! Both threads share one WinDivert handle (see [`EngineHandle`]).
//!
//! Byte counting reflects shaped throughput: bytes are only counted when a
//! packet is actually injected (immediately by the engine, or later by the
//! drainer). Dropped/blocked packets are not counted.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use windivert::prelude::*;
use windivert_sys::ChecksumFlags;

use super::Shared;
use super::synthetic;

/// Receive buffer size. WinDivert never hands us anything larger than a single
/// (possibly offloaded) IP packet; 64 KiB covers the maximum.
const RECV_BUFFER: usize = 65_535;
/// How often the drainer wakes to refill buckets and flush queues.
const DRAIN_INTERVAL: Duration = Duration::from_millis(5);
/// Per-bucket queue caps (whichever is hit first). Beyond these, packets are
/// dropped, which for TCP just applies additional back-pressure.
const MAX_QUEUE_PKTS: usize = 256;
const MAX_QUEUE_BYTES: usize = 1024 * 1024;
/// How many distinct unattributed 5-tuples to log per 5s report.
const UNATTRIBUTED_SAMPLES: usize = 5;

/// The single network-layer WinDivert handle, shared by the engine (recv+send)
/// and drainer (send) threads.
///
/// A second handle is not an option: re-injected packets are re-presented to
/// network-layer handles of lower priority, and a lower-priority handle
/// silently drops them, breaking connectivity. Injecting through the same
/// handle that captured the packet skips re-capture entirely.
///
/// Safety: the WinDivert C API documents handles as thread-safe; the Rust
/// wrapper is conservative in not implementing Send/Sync.
pub struct EngineHandle(pub WinDivert<NetworkLayer>);
unsafe impl Send for EngineHandle {}
unsafe impl Sync for EngineHandle {}

/// WinDivert filter for the network-layer handle.
///
/// Loopback traffic is excluded on purpose. It is never link traffic, so there
/// is nothing to meter or shape, and re-injecting it is harmful: security
/// software such as NordVPN Threat Protection redirects browser connections to
/// a local proxy through a WFP connect-redirect, and a re-injected loopback
/// packet loses that redirect state, so the connection never completes and the
/// browser reports "connection closed" while every non-browser app still works.
const NETWORK_FILTER: &str = "!loopback";

/// Open the shared network-layer capture handle. Fails if not elevated or if
/// the WinDivert driver files are missing.
pub fn open() -> Result<EngineHandle, WinDivertError> {
    Ok(EngineHandle(WinDivert::network(
        NETWORK_FILTER,
        0,
        WinDivertFlags::new(),
    )?))
}

/// Traffic direction, from the local host's point of view.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Dir {
    /// Inbound / download.
    Down,
    /// Outbound / upload.
    Up,
}

/// Bucket identity: one token bucket per executable per direction.
pub type BucketKey = (String, Dir);

/// A token bucket plus the queue of packets waiting on it.
pub struct ShapedBucket {
    /// Per-process limiter, absent when only the machine-wide cap applies.
    pub tb: Option<TokenBucket>,
    pub queue: VecDeque<WinDivertPacket<'static, NetworkLayer>>,
    pub queued_bytes: usize,
    /// Deficit carried between scheduler rounds for byte-fair service.
    pub deficit: u64,
}

impl ShapedBucket {
    pub fn new(rate: Option<u64>) -> Self {
        Self {
            tb: rate.map(TokenBucket::new),
            queue: VecDeque::new(),
            queued_bytes: 0,
            deficit: 0,
        }
    }
}

/// Synthetic queue key for packets that could not be attributed to a process.
const UNATTRIBUTED_BUCKET_KEY: &str = "\0netflux-unattributed";
/// Deficit-round-robin quantum. WinDivert's receive buffer bounds a packet to 65,535 bytes.
const DRR_QUANTUM: u64 = 65_535;

/// A byte-denominated token bucket. Capacity equals `rate`, giving a one-second
/// burst allowance; tokens refill continuously at `rate` bytes/second.
#[derive(Debug)]
pub struct TokenBucket {
    rate: u64,
    capacity: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(rate: u64) -> Self {
        Self::new_at(rate, Instant::now())
    }

    /// Construct with an explicit start instant (used by tests for determinism).
    pub fn new_at(rate: u64, now: Instant) -> Self {
        let capacity = rate as f64;
        Self {
            rate,
            capacity,
            tokens: capacity,
            last: now,
        }
    }

    /// Update the fill rate (e.g. after a rule edit), clamping current tokens to
    /// the new capacity.
    pub fn set_rate(&mut self, rate: u64) {
        if rate != self.rate {
            self.rate = rate;
            self.capacity = rate as f64;
            if self.tokens > self.capacity {
                self.tokens = self.capacity;
            }
        }
    }

    /// Add tokens for elapsed time since the last refill, capped at capacity.
    pub fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        if dt > 0.0 {
            self.tokens = (self.tokens + dt * self.rate as f64).min(self.capacity);
            self.last = now;
        }
    }

    /// Check whether `n` bytes can be charged without changing the bucket.
    /// Oversized packets may use a full bucket and create token debt.
    pub fn can_take(&self, n: u64) -> bool {
        let n = n as f64;
        if n > self.capacity {
            self.tokens >= self.capacity
        } else {
            self.tokens >= n
        }
    }

    /// Spend `n` bytes' worth of tokens. Returns `true` if consumed.
    pub fn try_take(&mut self, n: u64) -> bool {
        if !self.can_take(n) {
            return false;
        }
        self.tokens -= n as f64;
        true
    }
}

/// Engine thread entry point: capture, classify, verdict, inject.
pub fn run_engine(shared: Arc<Shared>, handle: Arc<EngineHandle>) {
    let divert = &handle.0;
    tracing::info!("engine thread started");

    let mut buf = vec![0u8; RECV_BUFFER];
    let mut stats = EngineStats::default();
    let mut last_report = Instant::now();
    while shared.running.load(Ordering::Relaxed) {
        let packet = match divert.recv(Some(&mut buf)) {
            Ok(p) => p,
            Err(WinDivertError::Recv(WinDivertRecvError::NoData)) => break,
            Err(e) => {
                stats.recv_err += 1;
                tracing::warn!("engine: recv error: {e}");
                continue;
            }
        };
        stats.recv_ok += 1;
        // Own the packet and recalculate checksums before any re-injection.
        // Captured packets can carry incomplete checksums due to hardware
        // offload; re-injecting them unmodified makes WinDivertSend report
        // success while the packets are dropped downstream.
        let mut packet = packet.into_owned();
        if packet.recalculate_checksums(ChecksumFlags::new()).is_err() {
            stats.calc_err += 1;
        }
        handle_packet(divert, &shared, packet, &mut stats);

        if last_report.elapsed() >= Duration::from_secs(5) {
            tracing::info!(
                "engine 5s: recv_ok={} recv_err={} sent={} send_err={} calc_err={} unattributed={} queued={} dropped={}",
                stats.recv_ok,
                stats.recv_err,
                stats.sent,
                stats.send_err,
                stats.calc_err,
                stats.unattributed,
                stats.queued,
                stats.dropped
            );
            for sample in &stats.unattributed_samples {
                tracing::info!("engine 5s: unattributed sample {sample}");
            }
            stats = EngineStats::default();
            last_report = Instant::now();
        }
    }
    tracing::info!("engine thread exiting");
}

/// Rolling diagnostics for the engine loop, reported every 5 seconds.
#[derive(Default)]
struct EngineStats {
    recv_ok: u64,
    recv_err: u64,
    sent: u64,
    send_err: u64,
    calc_err: u64,
    unattributed: u64,
    queued: u64,
    dropped: u64,
    /// Up to `UNATTRIBUTED_SAMPLES` example 5-tuples we failed to attribute,
    /// logged with the 5s report so misattribution can be diagnosed.
    unattributed_samples: Vec<String>,
}

/// Classify one captured packet and apply a shaping verdict.
fn handle_packet(
    divert: &WinDivert<NetworkLayer>,
    shared: &Shared,
    packet: WinDivertPacket<'static, NetworkLayer>,
    stats: &mut EngineStats,
) {
    let outbound = packet.address.outbound();
    let len = packet.data.len() as u64;

    // Attribute the packet to a process via the flow table.
    let parsed = parse_packet(&packet.data);
    let exe = match parsed {
        Some((proto, src, dst)) => {
            let (local, remote) = if outbound { (src, dst) } else { (dst, src) };
            shared.flows.lock().unwrap().exe_for(proto, local, remote)
        }
        None => None,
    };

    if exe.is_none() {
        stats.unattributed += 1;
        if stats.unattributed_samples.len() < UNATTRIBUTED_SAMPLES {
            let desc = match parsed {
                Some((proto, src, dst)) => {
                    format!("proto={proto} src={src} dst={dst} outbound={outbound} len={len}")
                }
                None => format!("unparsed len={len} outbound={outbound}"),
            };
            if !stats.unattributed_samples.contains(&desc) {
                stats.unattributed_samples.push(desc);
            }
        }
    }

    // Per-process rules exclude synthetic rows. The machine-wide cap still
    // includes every captured packet, whether or not attribution succeeded.
    let rule = exe
        .as_ref()
        .filter(|e| synthetic::is_shapable(e))
        .and_then(|e| shared.rules.lock().unwrap().get(e).cloned());
    let process_limit = rule
        .as_ref()
        .and_then(|r| if outbound { r.up_limit } else { r.down_limit });
    let global_limits = *shared.global_limits.lock().unwrap();
    let global_limit = if outbound {
        global_limits.up_limit
    } else {
        global_limits.down_limit
    };

    // A process block/zero limit or a zero machine-wide limit drops this direction.
    if rule.as_ref().is_some_and(|r| r.blocked)
        || process_limit == Some(0)
        || global_limit == Some(0)
    {
        stats.dropped += 1;
        return;
    }

    let dir = if outbound { Dir::Up } else { Dir::Down };

    // With a global cap, every packet enters the fair scheduler. This avoids
    // letting the first process to arrive consume the shared token bucket.
    if global_limit.is_some() {
        let bucket_key = exe
            .clone()
            .unwrap_or_else(|| UNATTRIBUTED_BUCKET_KEY.to_string());
        let plen = packet.data.len();
        let mut buckets = shared.buckets.lock().unwrap();
        let bucket = buckets
            .entry((bucket_key, dir))
            .or_insert_with(|| ShapedBucket::new(process_limit));
        if let Some(rate) = process_limit {
            let tb = bucket.tb.get_or_insert_with(|| TokenBucket::new(rate));
            tb.set_rate(rate);
        } else {
            bucket.tb = None;
        }

        if bucket.queue.len() < MAX_QUEUE_PKTS && bucket.queued_bytes + plen <= MAX_QUEUE_BYTES {
            bucket.queued_bytes += plen;
            bucket.queue.push_back(packet);
            stats.queued += 1;
        } else {
            stats.dropped += 1;
        }
        return;
    }

    match process_limit {
        None => send_and_count(
            divert,
            shared,
            &packet,
            exe.as_deref(),
            outbound,
            len,
            stats,
        ),
        Some(rate) => {
            let bucket_key = exe
                .clone()
                .expect("a process rule requires an executable path");
            let plen = packet.data.len();
            let mut buckets = shared.buckets.lock().unwrap();
            let bucket = buckets
                .entry((bucket_key, dir))
                .or_insert_with(|| ShapedBucket::new(Some(rate)));
            let tb = bucket.tb.get_or_insert_with(|| TokenBucket::new(rate));
            tb.set_rate(rate);
            tb.refill(Instant::now());

            if tb.try_take(len) {
                drop(buckets);
                send_and_count(
                    divert,
                    shared,
                    &packet,
                    exe.as_deref(),
                    outbound,
                    len,
                    stats,
                );
            } else if bucket.queue.len() < MAX_QUEUE_PKTS
                && bucket.queued_bytes + plen <= MAX_QUEUE_BYTES
            {
                bucket.queued_bytes += plen;
                bucket.queue.push_back(packet);
                stats.queued += 1;
            } else {
                stats.dropped += 1;
            }
        }
    }
}

/// Inject a packet and, if attributed, count its bytes as shaped throughput.
fn send_and_count(
    divert: &WinDivert<NetworkLayer>,
    shared: &Shared,
    packet: &WinDivertPacket<NetworkLayer>,
    exe: Option<&str>,
    outbound: bool,
    len: u64,
    stats: &mut EngineStats,
) {
    if let Err(e) = divert.send(packet) {
        stats.send_err += 1;
        if stats.send_err <= 5 || stats.send_err.is_multiple_of(500) {
            tracing::warn!(
                "engine: send failed (#{}) len={len} outbound={outbound}: {e}",
                stats.send_err
            );
        }
        return;
    }
    stats.sent += 1;
    let (down, up) = if outbound { (0, len) } else { (len, 0) };
    let mut flows = shared.flows.lock().unwrap();
    match exe {
        Some(exe) => flows.add_bytes(exe, down, up),
        // Unattributed traffic still counts, under the "Unknown" row, so the
        // totals and the graph reflect actual link usage.
        None => flows.add_unattributed(down, up),
    }
}

/// Drainer thread entry point: flush queued packets as tokens refill.
///
/// Re-injects through the same handle the engine captures on; see
/// [`EngineHandle`] for why a second handle is not an option.
pub fn run_drainer(shared: Arc<Shared>, handle: Arc<EngineHandle>) {
    let divert = &handle.0;
    tracing::info!("drainer thread started");

    while shared.running.load(Ordering::Relaxed) {
        let now = Instant::now();
        // Snapshot policy before taking queue locks.
        let rules = shared.rules.lock().unwrap().clone();
        let global_limits = *shared.global_limits.lock().unwrap();
        let rate_for = |dir: Dir| match dir {
            Dir::Down => global_limits.down_limit,
            Dir::Up => global_limits.up_limit,
        };

        let mut to_send: Vec<(
            WinDivertPacket<'static, NetworkLayer>,
            Option<String>,
            u64,
            u64,
        )> = Vec::new();

        {
            let mut global_buckets = shared.global_buckets.lock().unwrap();
            for dir in [Dir::Down, Dir::Up] {
                match rate_for(dir) {
                    Some(rate) if rate > 0 => {
                        let bucket = global_buckets
                            .entry(dir)
                            .or_insert_with(|| TokenBucket::new(rate));
                        bucket.set_rate(rate);
                        bucket.refill(now);
                    }
                    _ => {
                        global_buckets.remove(&dir);
                    }
                }
            }

            let mut buckets = shared.buckets.lock().unwrap();
            let mut keys: Vec<BucketKey> = buckets.keys().cloned().collect();
            keys.sort_by(|a, b| {
                a.0.cmp(&b.0).then_with(|| {
                    let order = |dir| match dir {
                        Dir::Down => 0,
                        Dir::Up => 1,
                    };
                    order(a.1).cmp(&order(b.1))
                })
            });

            // Rotate the starting process every drain cycle so any remainder
            // in the global bucket is shared fairly over time.
            if !keys.is_empty() {
                let mut cursor = shared.scheduler_cursor.lock().unwrap();
                let start = *cursor % keys.len();
                keys.rotate_left(start);
                *cursor = (start + 1) % keys.len();
            }

            // Refresh per-process token buckets. Drop queues whose policy now
            // blocks them; with no limits, release them immediately.
            for key in &keys {
                let Some(bucket) = buckets.get_mut(key) else {
                    continue;
                };
                let exe = (key.0 != UNATTRIBUTED_BUCKET_KEY).then_some(key.0.as_str());
                let rule = exe
                    .filter(|path| synthetic::is_shapable(path))
                    .and_then(|path| rules.get(path));
                let process_limit = rule.and_then(|r| match key.1 {
                    Dir::Down => r.down_limit,
                    Dir::Up => r.up_limit,
                });
                let global_limit = rate_for(key.1);

                if rule.is_some_and(|r| r.blocked)
                    || process_limit == Some(0)
                    || global_limit == Some(0)
                {
                    bucket.queue.clear();
                    bucket.queued_bytes = 0;
                    bucket.deficit = 0;
                    continue;
                }

                if let Some(rate) = process_limit.filter(|rate| *rate > 0) {
                    let tb = bucket.tb.get_or_insert_with(|| TokenBucket::new(rate));
                    tb.set_rate(rate);
                    tb.refill(now);
                } else {
                    bucket.tb = None;
                }

                if process_limit.is_none() && global_limit.is_none() {
                    while let Some(packet) = bucket.queue.pop_front() {
                        let len = packet.data.len() as u64;
                        bucket.queued_bytes = bucket.queued_bytes.saturating_sub(len as usize);
                        let (down, up) = match key.1 {
                            Dir::Down => (len, 0),
                            Dir::Up => (0, len),
                        };
                        let owner = exe.map(str::to_string);
                        to_send.push((packet, owner, down, up));
                    }
                    bucket.deficit = 0;
                }
            }

            // Deficit round-robin schedules by bytes, not merely packet count.
            // Each active process receives one 65,535-byte quantum per round.
            // That covers the largest possible IP packet and makes small-packet
            // and large-packet processes compete for the same byte share.
            loop {
                let mut made_progress = false;
                for key in &keys {
                    let Some(bucket) = buckets.get_mut(key) else {
                        continue;
                    };
                    if bucket.queue.is_empty() {
                        continue;
                    }

                    bucket.deficit = bucket
                        .deficit
                        .saturating_add(DRR_QUANTUM)
                        .min(MAX_QUEUE_BYTES as u64);

                    while let Some(front) = bucket.queue.front() {
                        let len = front.data.len() as u64;
                        if len > bucket.deficit {
                            break;
                        }

                        let process_available =
                            bucket.tb.as_ref().is_none_or(|tb| tb.can_take(len));
                        let global_active = rate_for(key.1).is_some_and(|rate| rate > 0);
                        let global_available = if global_active {
                            global_buckets
                                .get(&key.1)
                                .is_some_and(|tb| tb.can_take(len))
                        } else {
                            true
                        };

                        if !process_available || !global_available {
                            break;
                        }

                        if let Some(tb) = bucket.tb.as_mut() {
                            let _ = tb.try_take(len);
                        }
                        if global_active && let Some(tb) = global_buckets.get_mut(&key.1) {
                            let _ = tb.try_take(len);
                        }

                        let packet = bucket.queue.pop_front().unwrap();
                        bucket.queued_bytes = bucket.queued_bytes.saturating_sub(len as usize);
                        bucket.deficit -= len;
                        let (down, up) = match key.1 {
                            Dir::Down => (len, 0),
                            Dir::Up => (0, len),
                        };
                        let owner = (key.0 != UNATTRIBUTED_BUCKET_KEY).then(|| key.0.clone());
                        to_send.push((packet, owner, down, up));
                        made_progress = true;
                    }
                }

                if !made_progress {
                    break;
                }
            }

            // Keep token state only while a positive per-process limit exists
            // or queued packets still need a global-only ceiling.
            buckets.retain(|(exe, dir), bucket| {
                if !bucket.queue.is_empty() {
                    return true;
                }
                let rule = (exe != UNATTRIBUTED_BUCKET_KEY)
                    .then_some(exe.as_str())
                    .filter(|path| synthetic::is_shapable(path))
                    .and_then(|path| rules.get(path));
                if rule.is_some_and(|r| r.blocked) {
                    return false;
                }
                rule.is_some_and(|r| match dir {
                    Dir::Down => r.down_limit.is_some_and(|rate| rate > 0),
                    Dir::Up => r.up_limit.is_some_and(|rate| rate > 0),
                })
            });
        }

        // Only successfully reinjected packets contribute to process totals.
        let mut sent_bytes: Vec<(Option<String>, u64, u64)> = Vec::new();
        for (packet, exe, down, up) in to_send {
            match divert.send(&packet) {
                Ok(_) => sent_bytes.push((exe, down, up)),
                Err(e) => tracing::warn!("drainer: send failed: {e}"),
            }
        }
        if !sent_bytes.is_empty() {
            let mut flows = shared.flows.lock().unwrap();
            for (exe, down, up) in sent_bytes {
                match exe {
                    Some(exe) => flows.add_bytes(&exe, down, up),
                    None => flows.add_unattributed(down, up),
                }
            }
        }

        std::thread::sleep(DRAIN_INTERVAL);
    }
    tracing::info!("drainer thread exiting");
}

/// Zero-copy parse of an IPv4/IPv6 packet into `(protocol, src, dst)` socket
/// addresses. Ports are extracted for TCP/UDP; other protocols yield port 0
/// (which won't match a flow-table entry). Returns `None` for truncated or
/// non-IP data.
fn parse_packet(data: &[u8]) -> Option<(u8, SocketAddr, SocketAddr)> {
    let version = data.first()? >> 4;
    match version {
        4 => {
            if data.len() < 20 {
                return None;
            }
            let ihl = (data[0] & 0x0f) as usize * 4;
            if ihl < 20 || data.len() < ihl {
                return None;
            }
            let proto = data[9];
            let src_ip = Ipv4Addr::new(data[12], data[13], data[14], data[15]);
            let dst_ip = Ipv4Addr::new(data[16], data[17], data[18], data[19]);
            let fragment = u16::from_be_bytes([data[6], data[7]]);
            let (sp, dp) = if fragment & 0x1fff == 0 {
                l4_ports(proto, &data[ihl..])
            } else {
                (0, 0)
            };
            Some((
                proto,
                SocketAddr::new(IpAddr::V4(src_ip), sp),
                SocketAddr::new(IpAddr::V4(dst_ip), dp),
            ))
        }
        6 => {
            if data.len() < 40 {
                return None;
            }
            let src_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&data[8..24]).ok()?);
            let dst_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&data[24..40]).ok()?);
            let mut proto = data[6];
            let mut offset = 40usize;
            let mut non_initial_fragment = false;

            // Walk common IPv6 extension headers, with a bounded chain length.
            for _ in 0..8 {
                match proto {
                    // Hop-by-hop, routing, destination options, mobility,
                    // HIP and Shim6 headers use 8-octet length units.
                    0 | 43 | 60 | 135 | 139 | 140 => {
                        let header = data.get(offset..offset + 2)?;
                        let next = header[0];
                        let length = (header[1] as usize + 1) * 8;
                        if data.len() < offset + length {
                            return None;
                        }
                        proto = next;
                        offset += length;
                    }
                    // Fragment header: non-first fragments have no L4 ports.
                    44 => {
                        let header = data.get(offset..offset + 8)?;
                        proto = header[0];
                        let fragment = u16::from_be_bytes([header[2], header[3]]);
                        non_initial_fragment = fragment & 0xfff8 != 0;
                        offset += 8;
                        if non_initial_fragment {
                            break;
                        }
                    }
                    // Authentication Header length is measured in 32-bit words.
                    51 => {
                        let header = data.get(offset..offset + 2)?;
                        let next = header[0];
                        let length = (header[1] as usize + 2) * 4;
                        if data.len() < offset + length {
                            return None;
                        }
                        proto = next;
                        offset += length;
                    }
                    _ => break,
                }
            }

            let (sp, dp) = if non_initial_fragment {
                (0, 0)
            } else {
                l4_ports(proto, data.get(offset..)?)
            };
            Some((
                proto,
                SocketAddr::new(IpAddr::V6(src_ip), sp),
                SocketAddr::new(IpAddr::V6(dst_ip), dp),
            ))
        }
        _ => None,
    }
}

/// Extract `(src_port, dst_port)` for TCP (6) / UDP (17); `(0, 0)` otherwise.
fn l4_ports(proto: u8, l4: &[u8]) -> (u16, u16) {
    if (proto == 6 || proto == 17) && l4.len() >= 4 {
        (
            u16::from_be_bytes([l4[0], l4[1]]),
            u16::from_be_bytes([l4[2], l4[3]]),
        )
    } else {
        (0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_take_checks_availability_without_spending_tokens() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new_at(1000, t0);
        assert!(bucket.can_take(700));
        assert!(bucket.try_take(1000));
        assert!(!bucket.can_take(1));
    }

    #[test]
    fn full_bucket_allows_burst_up_to_capacity() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new_at(1000, t0);
        // Starts full: can spend the whole capacity at once.
        assert!(b.try_take(1000));
        // Now empty: even one byte fails.
        assert!(!b.try_take(1));
    }

    #[test]
    fn refill_is_proportional_to_elapsed_time() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new_at(1000, t0);
        assert!(b.try_take(1000)); // drain
        // After 0.5s, ~500 tokens are back.
        b.refill(t0 + Duration::from_millis(500));
        assert!(b.try_take(500));
        assert!(!b.try_take(1));
    }

    #[test]
    fn refill_never_exceeds_capacity() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new_at(1000, t0);
        assert!(b.try_take(1000));
        // Idle for 10s, but capacity caps the burst at 1000.
        b.refill(t0 + Duration::from_secs(10));
        assert!(b.try_take(1000));
        assert!(!b.try_take(1));
    }

    #[test]
    fn oversized_packet_is_charged_at_its_full_size() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new_at(500, t0);
        assert!(b.try_take(1500)); // spends 1500, leaving -1000 tokens
        assert!(!b.try_take(1));
        b.refill(t0 + Duration::from_millis(2999));
        assert!(!b.try_take(1500));
        b.refill(t0 + Duration::from_secs(3));
        assert!(b.try_take(1500));
    }

    #[test]
    fn set_rate_clamps_tokens_to_new_capacity() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new_at(1000, t0); // full: 1000 tokens
        b.set_rate(400); // capacity shrinks; tokens clamp to 400
        assert!(b.try_take(400));
        assert!(!b.try_take(1));
    }

    #[test]
    fn parses_ipv4_tcp_5tuple() {
        // Minimal IPv4 header (IHL=5 => 20 bytes), protocol=TCP(6), then ports.
        let mut pkt = vec![0u8; 24];
        pkt[0] = 0x45; // version 4, IHL 5
        pkt[9] = 6; // TCP
        pkt[12..16].copy_from_slice(&[10, 0, 0, 1]); // src
        pkt[16..20].copy_from_slice(&[93, 184, 216, 34]); // dst
        pkt[20..22].copy_from_slice(&50000u16.to_be_bytes()); // src port
        pkt[22..24].copy_from_slice(&443u16.to_be_bytes()); // dst port

        let (proto, src, dst) = parse_packet(&pkt).unwrap();
        assert_eq!(proto, 6);
        assert_eq!(src, "10.0.0.1:50000".parse().unwrap());
        assert_eq!(dst, "93.184.216.34:443".parse().unwrap());
    }

    #[test]
    fn parses_ipv6_destination_options_before_tcp() {
        let mut pkt = vec![0u8; 52];
        pkt[0] = 0x60;
        pkt[6] = 60; // Destination Options
        pkt[8..24].copy_from_slice(&"2001:db8::1".parse::<Ipv6Addr>().unwrap().octets());
        pkt[24..40].copy_from_slice(&"2001:db8::2".parse::<Ipv6Addr>().unwrap().octets());
        pkt[40] = 6; // Next header is TCP
        pkt[41] = 0; // Eight-byte extension header
        pkt[48..50].copy_from_slice(&50001u16.to_be_bytes());
        pkt[50..52].copy_from_slice(&443u16.to_be_bytes());

        let (proto, src, dst) = parse_packet(&pkt).unwrap();
        assert_eq!(proto, 6);
        assert_eq!(src, "[2001:db8::1]:50001".parse().unwrap());
        assert_eq!(dst, "[2001:db8::2]:443".parse().unwrap());
    }

    #[test]
    fn rejects_truncated_and_non_ip() {
        assert!(parse_packet(&[]).is_none());
        assert!(parse_packet(&[0x45, 0x00]).is_none()); // too short for v4
        assert!(parse_packet(&[0x00; 20]).is_none()); // version 0
    }
}
