//! The P2P task: one UDP socket, one [`str0m::Rtc`] per peer, and signaling through the remote.
//!
//! The task owns all connection state and never waits on the remote itself: reading and writing
//! signaling files run in spawned tasks that report back through a channel.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::ChannelId;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};

use super::wire::{self, Message};
use super::{Cmd, P2pConfig, P2pStatus, PeerState, PeerStatus, stun};
use crate::codec::keys::{KeyKind, LogicalKey, SignalKind};
use crate::codec::signal::{self, Description, Signal};
use crate::database::Inner;
use crate::error::Result;
use crate::model::{DbId, DeviceId};
use crate::remote::RemoteEntry;

/// How long a pending offer or answer may take to connect before the peer is marked failed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(45);
/// Wait before connecting again to a peer that sent a wrong `Hello` (another database).
const FAILED_BACKOFF: Duration = Duration::from_secs(10 * 60);
/// First wait after a connection attempt failed; it doubles with each failure in a row up to
/// [`FAILED_BACKOFF`], so a peer back from a network outage reconnects within a minute or two.
const RETRY_BACKOFF: Duration = Duration::from_secs(15);
/// Wait before re-offering to a peer whose channel closed.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// How long the first STUN round may take before the first `online` file is written.
const STUN_WAIT: Duration = Duration::from_millis(1500);
/// Housekeeping tick when nothing else is due.
const TICK: Duration = Duration::from_secs(1);
/// After we come online or see a new peer, offers and answers are likely: poll fast this long.
const DISCOVERY: Duration = Duration::from_secs(60);

/// A peer seen online.
#[derive(Clone, Copy)]
struct Online {
    session: u64,
    expires_ms: u64,
}

enum Role {
    /// We offered; waiting for the answer.
    Offerer(Option<SdpPendingOffer>),
    Answerer,
}

struct Peer {
    rtc: Option<Rtc>,
    role: Role,
    state: PeerState,
    /// The peer's session this connection is for.
    their_session: u64,
    since: Instant,
    channel: Option<ChannelId>,
    hello_ok: bool,
    retry_at: Option<Instant>,
    /// Our offer or answer file for this attempt, deleted once connected or done.
    our_file: Option<String>,
}

/// Results of spawned remote work, back to the task.
enum Incoming {
    /// A parsed signaling file.
    Signal {
        key: String,
        from: DeviceId,
        kind: SignalKind,
        expires_ms: u64,
        signal: Signal,
    },
    /// A file we wrote (online, offer or answer) is up.
    Written(String),
    /// Writing our `online` file failed (e.g. offline): try again soon, not at the next refresh.
    OnlineFailed,
}

struct Driver {
    inner: Arc<Inner>,
    config: P2pConfig,
    sock: Arc<UdpSocket>,
    /// The host candidate, also the destination of every received packet.
    local: SocketAddr,
    srflx: Option<SocketAddr>,
    stun_pending: Vec<[u8; 12]>,
    crypto: Arc<str0m::crypto::CryptoProvider>,
    session: u64,
    db_id: Option<DbId>,
    me: DeviceId,
    peers: HashMap<DeviceId, Peer>,
    /// Failed attempts in a row per peer, for the retry backoff.
    failures: HashMap<DeviceId, u32>,
    online: HashMap<DeviceId, Online>,
    /// Signaling files already handled (or being fetched).
    seen: HashSet<String>,
    our_online: Option<String>,
    next_online: Instant,
    discover_until: Instant,
    incoming_tx: mpsc::UnboundedSender<Incoming>,
    status: watch::Sender<P2pStatus>,
}

/// Wait after the `n`th failed attempt in a row (n ≥ 1).
fn retry_backoff(n: u32) -> Duration {
    RETRY_BACKOFF
        .saturating_mul(1 << n.saturating_sub(1).min(16))
        .min(FAILED_BACKOFF)
}

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("system random generator");
    u64::from_be_bytes(b)
}

/// The address of the interface that routes to the internet (no packet is sent).
fn primary_ipv4() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

pub(super) async fn run(
    inner: Arc<Inner>,
    config: P2pConfig,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    status: watch::Sender<P2pStatus>,
) {
    let Some(host) = config.host.or_else(primary_ipv4) else {
        tracing_like("p2p: no usable network interface");
        return;
    };
    let bind: SocketAddr = if host.is_loopback() {
        (host, 0).into()
    } else {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    };
    let Ok(sock) = UdpSocket::bind(bind).await else {
        tracing_like("p2p: cannot bind a UDP socket");
        return;
    };
    let port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
    let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel();
    let mut d = Driver {
        me: inner.device(),
        inner,
        config,
        sock: Arc::new(sock),
        local: SocketAddr::new(host, port),
        srflx: None,
        stun_pending: Vec::new(),
        crypto: Arc::new(str0m_aws_lc_rs::default_provider()),
        session: random_u64(),
        db_id: None,
        peers: HashMap::new(),
        failures: HashMap::new(),
        online: HashMap::new(),
        seen: HashSet::new(),
        our_online: None,
        next_online: Instant::now(),
        discover_until: Instant::now() + DISCOVERY,
        incoming_tx,
        status,
    };
    d.db_id = d.inner.store(|s| s.db_id()).await.ok().flatten();
    d.send_stun().await;
    let stun_deadline = tokio::time::Instant::now() + STUN_WAIT;
    if !d.stun_pending.is_empty() {
        let mut buf = vec![0u8; 2048];
        while d.srflx.is_none() && tokio::time::Instant::now() < stun_deadline {
            match tokio::time::timeout_at(stun_deadline, d.sock.recv_from(&mut buf)).await {
                Ok(Ok((n, _))) => {
                    d.on_stun(&buf[..n]);
                }
                _ => break,
            }
        }
    }
    // Online files written before we started are not in the change feed: list them once.
    d.bootstrap();

    let mut buf = vec![0u8; 2048];
    loop {
        d.housekeeping();
        let wake = d.drive_outputs().await;
        d.publish_status();
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Announce { seq, bytes }) => d.announce(seq, bytes),
                Some(Cmd::Signals(entries)) => d.on_entries(entries),
                Some(Cmd::Stop(done)) => {
                    d.shutdown().await;
                    let _ = done.send(());
                    return;
                }
                None => return,
            },
            Some(inc) = incoming_rx.recv() => d.on_incoming(inc),
            r = d.sock.recv_from(&mut buf) => {
                if let Ok((n, source)) = r {
                    d.on_packet(&buf[..n], source);
                }
            }
            _ = tokio::time::sleep_until(wake.into()) => {
                let now = Instant::now();
                for p in d.peers.values_mut() {
                    if let Some(rtc) = p.rtc.as_mut() {
                        let _ = rtc.handle_input(Input::Timeout(now));
                    }
                }
            }
        }
    }
}

/// Diagnostics without a logging dependency; quiet unless `BOTHY_P2P_DEBUG` is set.
fn tracing_like(msg: &str) {
    if std::env::var_os("BOTHY_P2P_DEBUG").is_some() {
        eprintln!("{msg}");
    }
}

impl Driver {
    async fn send_stun(&mut self) {
        for server in &self.config.stun_servers {
            let Ok(mut addrs) = tokio::net::lookup_host(server.as_str()).await else {
                continue;
            };
            let Some(addr) = addrs.find(SocketAddr::is_ipv4) else {
                continue;
            };
            let mut tid = [0u8; 12];
            let _ = getrandom::fill(&mut tid);
            if self.sock.send_to(&stun::request(tid), addr).await.is_ok() {
                self.stun_pending.push(tid);
            }
        }
    }

    fn on_stun(&mut self, data: &[u8]) -> bool {
        let Some(i) = self
            .stun_pending
            .iter()
            .position(|t| stun::is_response_to(data, t))
        else {
            return false;
        };
        let tid = self.stun_pending.swap_remove(i);
        if let Some(addr) = stun::mapped_address(data, &tid)
            && addr.is_ipv4()
            && addr != self.local
        {
            self.srflx = Some(addr);
        }
        true
    }

    fn new_rtc(&self) -> Rtc {
        let mut rtc = Rtc::builder()
            .set_crypto_provider(self.crypto.clone())
            .build(Instant::now());
        if let Ok(c) = Candidate::host(self.local, "udp") {
            let _ = rtc.add_local_candidate(c);
        }
        if let Some(addr) = self.srflx
            && let Ok(c) = Candidate::server_reflexive(addr, self.local, "udp")
        {
            let _ = rtc.add_local_candidate(c);
        }
        rtc
    }

    fn wall_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Writes a signaling file in a spawned task (sealed in an encrypted database).
    fn write(&self, kind: SignalKind, signal: Signal) {
        let Some(db_id) = self.db_id else { return };
        let inner = self.inner.clone();
        let me = self.me;
        let tx = self.incoming_tx.clone();
        tokio::spawn(async move {
            // An encrypted database that is locked writes nothing: SDPs carry IP addresses.
            // `online` is retried soon, so peers find us shortly after an unlock.
            let Ok(data_key) = crate::engine::sync_key(&inner).await else {
                if kind == SignalKind::Online {
                    let _ = tx.send(Incoming::OnlineFailed);
                }
                return;
            };
            let Ok(hlc) = inner.store(|s| s.tick()).await else {
                return;
            };
            let key = LogicalKey::signal(db_id, me, kind, hlc);
            let name = key.to_string();
            let json = signal.to_json(&key);
            #[cfg(feature = "crypto")]
            let body = match crate::engine::crypto::seal_json(data_key.as_ref(), &name, json) {
                Ok(b) => b,
                Err(_) => return,
            };
            #[cfg(not(feature = "crypto"))]
            let body = {
                let _ = data_key;
                json
            };
            if inner.remote.put_new(&name, &body, None).await.is_ok() {
                let _ = tx.send(Incoming::Written(name));
            } else if kind == SignalKind::Online {
                let _ = tx.send(Incoming::OnlineFailed);
            }
        });
    }

    fn delete(&self, key: String) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let _ = inner.remote.delete(&key).await;
        });
    }

    fn bootstrap(&mut self) {
        let Some(db_id) = self.db_id else { return };
        let inner = self.inner.clone();
        let tx = self.incoming_tx.clone();
        let me = self.me;
        tokio::spawn(async move {
            let Ok(entries) = inner.remote.list(&format!("{db_id}/signaling/")).await else {
                return;
            };
            for e in entries {
                fetch_signal(&inner, db_id, me, e.key, &tx).await;
            }
        });
    }

    /// Signaling files from a sync: fetch the ones we have not seen.
    fn on_entries(&mut self, entries: Vec<RemoteEntry>) {
        // Entries come from a sync, so the database is attached by now (e.g. after a join).
        if self.db_id.is_none() {
            self.db_id = entries
                .iter()
                .find_map(|e| LogicalKey::parse(&e.key).map(|k| k.db_id));
            if self.db_id.is_some() {
                self.bootstrap();
            }
        }
        let Some(db_id) = self.db_id else { return };
        let now = Self::wall_ms();
        for e in entries {
            let Some(k) = LogicalKey::parse(&e.key) else {
                continue;
            };
            let KeyKind::Signal(dev, kind, hlc) = k.kind else {
                continue;
            };
            if k.db_id != db_id || !self.seen.insert(e.key.clone()) {
                continue;
            }
            if dev != self.me && signal::is_stale(hlc, now) {
                self.delete(e.key);
                continue;
            }
            if dev == self.me || signal::expires_ms(kind, hlc) < now {
                continue;
            }
            let targets_me = match kind {
                SignalKind::Online => true,
                SignalKind::Offer(to) | SignalKind::Answer(to) => to == self.me,
            };
            if !targets_me {
                continue;
            }
            let inner = self.inner.clone();
            let tx = self.incoming_tx.clone();
            let me = self.me;
            tokio::spawn(async move {
                fetch_signal(&inner, db_id, me, e.key, &tx).await;
            });
        }
    }

    fn on_incoming(&mut self, inc: Incoming) {
        match inc {
            Incoming::OnlineFailed => {
                // Peers cannot see us until it is up; the old one may have expired meanwhile.
                self.next_online = self.next_online.min(Instant::now() + RETRY_BACKOFF);
            }
            Incoming::Written(name) => {
                self.seen.insert(name.clone());
                if name.contains("/online-")
                    && let Some(old) = self.our_online.replace(name)
                {
                    self.delete(old);
                }
            }
            Incoming::Signal {
                key,
                from,
                kind,
                expires_ms,
                signal,
            } => {
                self.seen.insert(key);
                match (kind, signal) {
                    (SignalKind::Online, Signal::Online { session }) => {
                        let new = self.online.get(&from).is_none_or(|o| o.session != session);
                        if new {
                            self.discover_until = Instant::now() + DISCOVERY;
                        }
                        self.online.insert(
                            from,
                            Online {
                                session,
                                expires_ms,
                            },
                        );
                        self.maybe_offer(from);
                    }
                    (SignalKind::Offer(to), Signal::Description(d)) if to == self.me => {
                        self.on_offer(from, d)
                    }
                    (SignalKind::Answer(to), Signal::Description(d)) if to == self.me => {
                        self.on_answer(from, d)
                    }
                    _ => {}
                }
            }
        }
    }

    /// Offers to `peer` if we are the offerer (smaller id) and have no live attempt.
    fn maybe_offer(&mut self, peer: DeviceId) {
        if self.me >= peer {
            return;
        }
        let Some(online) = self.online.get(&peer).copied() else {
            return;
        };
        if online.expires_ms < Self::wall_ms() {
            return;
        }
        if let Some(p) = self.peers.get(&peer) {
            let same_session = p.their_session == online.session;
            match p.state {
                PeerState::Connected | PeerState::Signaling if same_session => return,
                PeerState::Failed | PeerState::Disconnected
                    if p.retry_at.is_some_and(|t| Instant::now() < t) && same_session =>
                {
                    return;
                }
                _ => {}
            }
        }
        let mut rtc = self.new_rtc();
        let mut api = rtc.sdp_api();
        api.add_channel(wire::LABEL.into());
        let Some((offer, pending)) = api.apply() else {
            return;
        };
        let sdp = offer.to_sdp_string();
        self.close(peer);
        self.peers.insert(
            peer,
            Peer {
                rtc: Some(rtc),
                role: Role::Offerer(Some(pending)),
                state: PeerState::Signaling,
                their_session: online.session,
                since: Instant::now(),
                channel: None,
                hello_ok: false,
                retry_at: None,
                our_file: None,
            },
        );
        self.write(
            SignalKind::Offer(peer),
            Signal::Description(Description {
                session: self.session,
                to_session: online.session,
                sdp,
            }),
        );
    }

    fn on_offer(&mut self, from: DeviceId, d: Description) {
        if d.to_session != self.session || from >= self.me {
            return;
        }
        if let Some(p) = self.peers.get(&from)
            && p.their_session == d.session
            && matches!(p.state, PeerState::Connected)
        {
            return;
        }
        let Ok(offer) = SdpOffer::from_sdp_string(&d.sdp) else {
            return;
        };
        let mut rtc = self.new_rtc();
        let Ok(answer) = rtc.sdp_api().accept_offer(offer) else {
            return;
        };
        let sdp = answer.to_sdp_string();
        self.close(from);
        self.peers.insert(
            from,
            Peer {
                rtc: Some(rtc),
                role: Role::Answerer,
                state: PeerState::Signaling,
                their_session: d.session,
                since: Instant::now(),
                channel: None,
                hello_ok: false,
                retry_at: None,
                our_file: None,
            },
        );
        self.write(
            SignalKind::Answer(from),
            Signal::Description(Description {
                session: self.session,
                to_session: d.session,
                sdp,
            }),
        );
    }

    fn on_answer(&mut self, from: DeviceId, d: Description) {
        if d.to_session != self.session {
            return;
        }
        let Some(p) = self.peers.get_mut(&from) else {
            return;
        };
        if p.their_session != d.session {
            return;
        }
        let Role::Offerer(pending) = &mut p.role else {
            return;
        };
        let (Some(pending), Some(rtc)) = (pending.take(), p.rtc.as_mut()) else {
            return;
        };
        let Ok(answer) = SdpAnswer::from_sdp_string(&d.sdp) else {
            return;
        };
        if rtc.sdp_api().accept_answer(pending, answer).is_err() {
            p.state = PeerState::Failed;
            let n = self.failures.entry(from).or_default();
            *n += 1;
            p.retry_at = Some(Instant::now() + retry_backoff(*n));
        }
    }

    /// Drops a peer's connection (keeps nothing but the record for status and retries).
    fn close(&mut self, peer: DeviceId) {
        if let Some(mut p) = self.peers.remove(&peer) {
            if let Some(rtc) = p.rtc.as_mut() {
                rtc.disconnect();
            }
            if let Some(f) = p.our_file.take() {
                self.delete(f);
            }
        }
    }

    fn on_packet(&mut self, data: &[u8], source: SocketAddr) {
        if self.on_stun(data) {
            return;
        }
        let Ok(contents) = data.try_into() else {
            return;
        };
        let input = Input::Receive(
            Instant::now(),
            Receive {
                proto: Protocol::Udp,
                source,
                destination: self.local,
                contents,
            },
        );
        for p in self.peers.values_mut() {
            if let Some(rtc) = p.rtc.as_mut()
                && rtc.accepts(&input)
            {
                let _ = rtc.handle_input(input);
                return;
            }
        }
    }

    fn announce(&mut self, seq: u64, bytes: Vec<u8>) {
        let inline = self.config.inline_segments && bytes.len() <= wire::INLINE_LIMIT;
        let msg = Message::SegmentUp {
            seq,
            bytes: inline.then_some(bytes),
        }
        .encode();
        for p in self.peers.values_mut() {
            if p.state != PeerState::Connected {
                continue;
            }
            if let (Some(rtc), Some(cid)) = (p.rtc.as_mut(), p.channel)
                && let Some(mut ch) = rtc.channel(cid)
            {
                let _ = ch.write(true, &msg);
            }
        }
    }

    /// Runs every Rtc until it wants a timeout; returns the earliest wake-up.
    async fn drive_outputs(&mut self) -> Instant {
        let mut wake = Instant::now() + TICK;
        let mut sends: Vec<(Vec<u8>, SocketAddr)> = Vec::new();
        let mut events: Vec<(DeviceId, Event)> = Vec::new();
        for (dev, p) in &mut self.peers {
            let Some(rtc) = p.rtc.as_mut() else { continue };
            loop {
                match rtc.poll_output() {
                    Ok(Output::Timeout(t)) => {
                        wake = wake.min(t);
                        break;
                    }
                    Ok(Output::Transmit(t)) => sends.push((t.contents.to_vec(), t.destination)),
                    Ok(Output::Event(e)) => events.push((*dev, e)),
                    Err(_) => {
                        rtc.disconnect();
                        break;
                    }
                }
            }
        }
        for (data, to) in sends {
            let _ = self.sock.send_to(&data, to).await;
        }
        for (dev, e) in events {
            self.on_event(dev, e);
        }
        wake.max(Instant::now() + Duration::from_millis(1))
    }

    fn on_event(&mut self, dev: DeviceId, e: Event) {
        let (db_id, me, session) = (self.db_id, self.me, self.session);
        let Some(p) = self.peers.get_mut(&dev) else {
            return;
        };
        match e {
            Event::ChannelOpen(cid, label) if label == wire::LABEL => {
                p.channel = Some(cid);
                if let (Some(db_id), Some(rtc)) = (db_id, p.rtc.as_mut())
                    && let Some(mut ch) = rtc.channel(cid)
                {
                    let hello = Message::Hello {
                        db_id,
                        device_id: me,
                        session,
                    };
                    let _ = ch.write(true, &hello.encode());
                }
            }
            Event::ChannelData(data) => match Message::decode(&data.data) {
                Ok(Message::Hello {
                    db_id: their_db,
                    device_id,
                    session: their_session,
                }) => {
                    if Some(their_db) == db_id
                        && device_id == dev
                        && their_session == p.their_session
                    {
                        p.hello_ok = true;
                        p.state = PeerState::Connected;
                        self.failures.remove(&dev);
                        if let Some(f) = p.our_file.take() {
                            self.delete(f);
                        }
                    } else if let Some(rtc) = p.rtc.as_mut() {
                        // Wrong database or device: not our peer.
                        rtc.disconnect();
                        p.state = PeerState::Failed;
                        p.retry_at = Some(Instant::now() + FAILED_BACKOFF);
                    }
                }
                Ok(Message::SegmentUp { seq, bytes }) if p.hello_ok => {
                    let inner = self.inner.clone();
                    match bytes {
                        Some(bytes) => {
                            tokio::spawn(async move {
                                if crate::engine::apply_pushed(&inner, dev, seq, bytes)
                                    .await
                                    .is_err()
                                {
                                    inner.remote_hint.notify_one();
                                }
                            });
                        }
                        None => self.inner.remote_hint.notify_one(),
                    }
                }
                // Unknown, newer or premature messages are hints we can drop.
                _ => {}
            },
            Event::ChannelClose(_)
            | Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                if let Some(rtc) = p.rtc.as_mut() {
                    rtc.disconnect();
                }
                p.rtc = None;
                p.channel = None;
                p.hello_ok = false;
                p.state = PeerState::Disconnected;
                p.retry_at = Some(Instant::now() + RECONNECT_DELAY);
            }
            _ => {}
        }
    }

    fn housekeeping(&mut self) {
        let now = Instant::now();
        if self.db_id.is_none() {
            return;
        }
        // Nothing goes on a remote until a sync has found this database there.
        if !self.inner.on_remote.load(Ordering::Acquire) {
            return;
        }
        // A fork gives this device a new id.
        self.me = self.inner.device();
        if now >= self.next_online {
            self.next_online = now + Duration::from_millis(signal::ONLINE_REFRESH_MS);
            self.write(
                SignalKind::Online,
                Signal::Online {
                    session: self.session,
                },
            );
            self.refresh_stun();
        }
        let mut retry = Vec::new();
        for (dev, p) in &mut self.peers {
            if p.state == PeerState::Signaling && now.duration_since(p.since) > CONNECT_TIMEOUT {
                if let Some(rtc) = p.rtc.as_mut() {
                    rtc.disconnect();
                }
                p.rtc = None;
                p.state = PeerState::Failed;
                let n = self.failures.entry(*dev).or_default();
                *n += 1;
                p.retry_at = Some(now + retry_backoff(*n));
            }
            if p.rtc.as_ref().is_some_and(|r| !r.is_alive()) && p.state == PeerState::Connected {
                p.rtc = None;
                p.state = PeerState::Disconnected;
                p.retry_at = Some(now + RECONNECT_DELAY);
            }
            if matches!(p.state, PeerState::Failed | PeerState::Disconnected)
                && p.retry_at.is_some_and(|t| now >= t)
            {
                retry.push(*dev);
            }
        }
        for dev in retry {
            if let Some(p) = self.peers.get_mut(&dev) {
                p.retry_at = None;
                // Force a new attempt even for the same session.
                p.their_session = p.their_session.wrapping_add(1);
            }
            self.maybe_offer(dev);
        }
        // Online peers we have not tried yet (e.g. seen before the database id was known).
        let untried: Vec<DeviceId> = self
            .online
            .keys()
            .filter(|d| **d > self.me && !self.peers.contains_key(d))
            .copied()
            .collect();
        for dev in untried {
            self.maybe_offer(dev);
        }
        let wall = Self::wall_ms();
        self.online.retain(|_, o| o.expires_ms >= wall);
    }

    /// Asks the STUN servers again (NAT mappings change); answers arrive on the main socket.
    fn refresh_stun(&mut self) {
        if self.config.stun_servers.is_empty() || !self.stun_pending.is_empty() {
            return;
        }
        let servers = self.config.stun_servers.clone();
        let sock = self.sock.clone();
        let mut tids = Vec::new();
        for _ in &servers {
            let mut tid = [0u8; 12];
            let _ = getrandom::fill(&mut tid);
            tids.push(tid);
        }
        self.stun_pending.extend(tids.iter().copied());
        tokio::spawn(async move {
            for (server, tid) in servers.iter().zip(tids) {
                if let Ok(mut addrs) = tokio::net::lookup_host(server.as_str()).await
                    && let Some(addr) = addrs.find(SocketAddr::is_ipv4)
                {
                    let _ = sock.send_to(&stun::request(tid), addr).await;
                }
            }
        });
    }

    fn publish_status(&self) {
        let mut peers: Vec<PeerStatus> = self
            .peers
            .iter()
            .map(|(d, p)| PeerStatus {
                device_id: *d,
                state: p.state,
            })
            .collect();
        peers.sort_by_key(|p| p.device_id);
        let discovering = Instant::now() < self.discover_until
            || self.peers.values().any(|p| p.state == PeerState::Signaling);
        self.status.send_if_modified(|s| {
            if s.peers == peers && s.discovering == discovering {
                false
            } else {
                s.peers = peers;
                s.discovering = discovering;
                true
            }
        });
    }

    async fn shutdown(&mut self) {
        let mut keys: Vec<String> = self.our_online.take().into_iter().collect();
        for p in self.peers.values_mut() {
            if let Some(rtc) = p.rtc.as_mut() {
                rtc.disconnect();
            }
            keys.extend(p.our_file.take());
        }
        for k in keys {
            let _ =
                tokio::time::timeout(Duration::from_secs(2), self.inner.remote.delete(&k)).await;
        }
        self.status.send_modify(|s| {
            s.running = false;
            s.discovering = false;
            s.peers.clear();
        });
    }
}

/// Reads one signaling file addressed to us (or an `online` file) and reports it.
async fn fetch_signal(
    inner: &Arc<Inner>,
    db_id: DbId,
    me: DeviceId,
    key: String,
    tx: &mpsc::UnboundedSender<Incoming>,
) {
    let Some(k) = LogicalKey::parse(&key) else {
        return;
    };
    let KeyKind::Signal(from, kind, hlc) = k.kind else {
        return;
    };
    if k.db_id != db_id || from == me {
        return;
    }
    let expires_ms = signal::expires_ms(kind, hlc);
    let Ok(data_key) = crate::engine::sync_key(inner).await else {
        return;
    };
    let Ok(Some(bytes)) = inner.remote.get(&key).await else {
        return;
    };
    let Ok(json) = open(data_key.as_ref(), &key, bytes) else {
        return;
    };
    if let Ok(signal) = Signal::from_json(&k, &json) {
        let _ = tx.send(Incoming::Signal {
            key,
            from,
            kind,
            expires_ms,
            signal,
        });
    }
}

fn open(data_key: Option<&crate::codec::DataKey>, key: &str, bytes: Vec<u8>) -> Result<Vec<u8>> {
    #[cfg(feature = "crypto")]
    if let Some(k) = data_key {
        return crate::crypto::open_json(k, key, &bytes);
    }
    #[cfg(not(feature = "crypto"))]
    let _ = (data_key, key);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_from_seconds_to_ten_minutes() {
        assert_eq!(retry_backoff(1), Duration::from_secs(15));
        assert_eq!(retry_backoff(2), Duration::from_secs(30));
        assert_eq!(retry_backoff(4), Duration::from_secs(120));
        assert_eq!(retry_backoff(7), FAILED_BACKOFF);
        assert_eq!(retry_backoff(u32::MAX), FAILED_BACKOFF);
    }
}
