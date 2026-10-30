//! The sans-IO uTP connection state machine.
//!
//! A [`Connection`] is fed datagrams ([`Connection::on_packet`]) and timer
//! expiries ([`Connection::on_timeout`]) and asked for datagrams to send
//! ([`Connection::poll_transmit`]); the application reads and writes byte
//! streams through it. It never touches a socket or the clock: every call
//! takes `now`.
//!
//! * **Reliability**: cumulative and selective ACKs (BEP 29 extension 1),
//!   fast retransmit after three duplicate ACKs or three later packets
//!   selectively acknowledged, and a retransmission timer with exponential
//!   back-off (RFC 6298 estimator).
//! * **Congestion control**: LEDBAT (RFC 6817). The window grows while the
//!   one-way queuing delay stays under [`Config::target_delay`] and shrinks
//!   when it is above, so uTP yields to TCP and interactive traffic. A slow
//!   start doubles the window until the first loss or until delay rises.
//! * **Flow control**: the advertised window is the free receive buffer.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};

use crate::packet::{Packet, PacketType};

/// Connection tunables. [`Config::default`] follows libutp.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Config {
    /// Largest payload per packet. 1382 keeps a packet (with header and a
    /// small SACK) inside a 1500-byte Ethernet frame over IPv6 too.
    pub max_payload: usize,
    /// Bytes buffered for sending (unsent plus unacknowledged).
    pub send_buffer: usize,
    /// Bytes buffered on the receive side (in order plus out of order).
    pub recv_buffer: usize,
    /// LEDBAT queuing delay target.
    pub target_delay: Duration,
    /// SYN retransmissions before a connect fails.
    pub syn_retries: u32,
    /// Consecutive retransmission timeouts before the connection fails.
    pub max_retransmits: u32,
    /// Silence after which the connection fails.
    pub idle_timeout: Duration,
    /// Idle time after which an empty ACK is sent to keep NAT state.
    pub keepalive: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_payload: 1382,
            send_buffer: 1 << 20,
            recv_buffer: 1 << 20,
            target_delay: Duration::from_millis(100),
            syn_retries: 2,
            max_retransmits: 6,
            idle_timeout: Duration::from_secs(60),
            keepalive: Duration::from_secs(29),
        }
    }
}

/// Why a connection ended abnormally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConnError {
    /// The remote sent a reset.
    #[error("connection reset by peer")]
    Reset,
    /// Retransmissions or silence exceeded their limit.
    #[error("connection timed out")]
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    SynSent,
    /// Accepted; data may only flow once the initiator answered our ACK
    /// (so its first data packet cannot be confused with a lost one).
    SynRecv,
    Connected,
    Closed,
}

/// A packet sent and not yet acknowledged.
#[derive(Debug)]
struct Sent {
    seq: u16,
    ty: PacketType,
    payload: Bytes,
    first_sent: Option<Instant>,
    transmissions: u32,
    /// Queued for retransmission.
    resend: bool,
}

/// Packets in flight at once (bounded so sequence numbers stay unambiguous
/// and the peer's reorder buffer is not overrun).
const MAX_IN_FLIGHT: usize = 512;
/// Out-of-order packets accepted ahead of the next expected one.
const MAX_REORDER: u16 = 1024;
/// LEDBAT: most the window grows per RTT in congestion avoidance.
const MAX_CWND_INCREASE: i64 = 3000;
const MIN_RTO: Duration = Duration::from_millis(500);
const INITIAL_RTO: Duration = Duration::from_secs(1);
const MAX_RTO: Duration = Duration::from_secs(30);
/// How long a cleanly closed connection keeps acknowledging retransmitted
/// packets after the last one (like TCP's `TIME_WAIT`): two of the longest
/// retransmission intervals.
const LINGER: Duration = Duration::from_secs(60);
/// Base delay is the minimum over this many one-minute buckets.
const DELAY_BUCKETS: usize = 3;

/// Minimum one-way delay samples per minute; their minimum is the base
/// delay (the propagation delay without queuing).
#[derive(Debug, Default)]
struct DelayHistory {
    buckets: VecDeque<(Instant, u32)>,
}

impl DelayHistory {
    fn add(&mut self, now: Instant, sample: u32) {
        match self.buckets.back_mut() {
            Some((start, min)) if now.duration_since(*start) < Duration::from_secs(60) => {
                *min = (*min).min(sample);
            }
            _ => {
                self.buckets.push_back((now, sample));
                if self.buckets.len() > DELAY_BUCKETS {
                    self.buckets.pop_front();
                }
            }
        }
    }

    fn base(&self) -> Option<u32> {
        self.buckets.iter().map(|(_, min)| *min).min()
    }
}

/// One uTP connection.
#[derive(Debug)]
pub struct Connection {
    config: Config,
    epoch: Instant,
    state: State,
    error: Option<ConnError>,
    recv_id: u16,
    send_id: u16,
    /// Next sequence number to send.
    seq_nr: u16,
    /// Last in-order sequence number received.
    ack_nr: u16,

    send_buf: BytesMut,
    in_flight: VecDeque<Sent>,
    in_flight_bytes: usize,
    cwnd: usize,
    slow_start: bool,
    /// Slow start ends at this window (half the window at the last loss).
    ssthresh: usize,
    peer_wnd: usize,
    srtt: Option<Duration>,
    rttvar: Duration,
    rto: Duration,
    rto_deadline: Option<Instant>,
    timeouts: u32,
    dup_acks: u32,
    last_ack: u16,
    last_cut: Option<Instant>,
    delays: DelayHistory,

    recv_buf: BytesMut,
    reorder: HashMap<u16, Bytes>,
    reorder_bytes: usize,
    /// Our clock minus the peer's timestamp of its last packet.
    reply_micro: u32,
    ack_pending: bool,
    close_requested: bool,
    fin_seq: Option<u16>,
    their_fin: Option<u16>,
    eof: bool,
    last_recv: Instant,
    last_send: Instant,
    /// After a clean close: until when retransmitted FINs are still
    /// acknowledged (the peer may not have seen our last ACK).
    linger_until: Option<Instant>,
}

/// Whether `a` comes after `b` in wrapping sequence space.
const fn seq_after(a: u16, b: u16) -> bool {
    let d = a.wrapping_sub(b);
    d != 0 && d < 0x8000
}

impl Connection {
    fn base(config: Config, now: Instant, recv_id: u16, send_id: u16, state: State) -> Self {
        let mss = config.max_payload.max(1);
        Self {
            epoch: now,
            state,
            error: None,
            recv_id,
            send_id,
            seq_nr: fastrand::u16(..),
            ack_nr: 0,
            send_buf: BytesMut::new(),
            in_flight: VecDeque::new(),
            in_flight_bytes: 0,
            cwnd: 2 * mss,
            slow_start: true,
            ssthresh: usize::MAX,
            peer_wnd: config.recv_buffer,
            srtt: None,
            rttvar: Duration::ZERO,
            rto: INITIAL_RTO,
            rto_deadline: None,
            timeouts: 0,
            dup_acks: 0,
            last_ack: 0,
            last_cut: None,
            delays: DelayHistory::default(),
            recv_buf: BytesMut::new(),
            reorder: HashMap::new(),
            reorder_bytes: 0,
            reply_micro: 0,
            ack_pending: false,
            close_requested: false,
            fin_seq: None,
            their_fin: None,
            eof: false,
            last_recv: now,
            last_send: now,
            linger_until: None,
            config,
        }
    }

    /// Opens a connection: the first [`poll_transmit`](Self::poll_transmit)
    /// returns the SYN. `recv_id` must be unused for the remote address.
    #[must_use]
    pub fn connect(config: Config, now: Instant, recv_id: u16) -> Self {
        let mut c = Self::base(
            config,
            now,
            recv_id,
            recv_id.wrapping_add(1),
            State::SynSent,
        );
        c.push_sent(PacketType::Syn, Bytes::new());
        c
    }

    /// Accepts the connection a SYN opened; the first transmit is the
    /// acknowledgement.
    #[must_use]
    pub fn accept(config: Config, now: Instant, syn: &Packet) -> Self {
        let mut c = Self::base(
            config,
            now,
            syn.conn_id.wrapping_add(1),
            syn.conn_id,
            State::SynRecv,
        );
        c.ack_nr = syn.seq_nr;
        c.peer_wnd = syn.wnd_size as usize;
        c.reply_micro = c.micros(now).wrapping_sub(syn.timestamp);
        c.ack_pending = true;
        c
    }

    /// The id the peer puts in packets for us.
    #[must_use]
    pub const fn recv_id(&self) -> u16 {
        self.recv_id
    }

    /// Whether the handshake completed.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.state == State::Connected
    }

    /// Whether the connection is over (closed cleanly or failed).
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state == State::Closed
    }

    /// Why the connection failed, if it did.
    #[must_use]
    pub const fn error(&self) -> Option<ConnError> {
        self.error
    }

    /// Bytes ready to [`read`](Self::read).
    #[must_use]
    pub fn readable(&self) -> usize {
        self.recv_buf.len()
    }

    /// Whether our FIN and everything before it were acknowledged.
    #[must_use]
    pub fn is_fin_acked(&self) -> bool {
        // The FIN is the last packet: a selective ACK of it alone is not
        // enough while earlier data is missing.
        self.fin_seq.is_some() && self.in_flight.is_empty()
    }

    /// Whether the peer finished sending and everything was read.
    #[must_use]
    pub fn is_eof(&self) -> bool {
        self.eof && self.recv_buf.is_empty()
    }

    /// Bytes [`write`](Self::write) accepts now.
    #[must_use]
    pub fn writable(&self) -> usize {
        if self.close_requested || self.state == State::Closed {
            return 0;
        }
        self.config
            .send_buffer
            .saturating_sub(self.send_buf.len() + self.in_flight_bytes)
    }

    /// Current congestion window in bytes.
    #[must_use]
    pub const fn congestion_window(&self) -> usize {
        self.cwnd
    }

    /// Smoothed round-trip time, once measured.
    #[must_use]
    pub const fn rtt(&self) -> Option<Duration> {
        self.srtt
    }

    /// Queues as much of `data` as fits; returns how much. 0 once closing.
    pub fn write(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.writable());
        self.send_buf
            .extend_from_slice(data.get(..n).unwrap_or_default());
        n
    }

    /// Reads received bytes into `buf`.
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let before = self.recv_window();
        let n = buf.len().min(self.recv_buf.len());
        let chunk = self.recv_buf.split_to(n);
        if let Some(dst) = buf.get_mut(..n) {
            dst.copy_from_slice(&chunk);
        }
        // Tell a sender stalled on our window that it opened.
        if before < self.config.max_payload && self.recv_window() >= self.config.max_payload {
            self.ack_pending = true;
        }
        n
    }

    /// Finishes sending: a FIN follows the buffered data.
    pub fn close(&mut self) {
        self.close_requested = true;
    }

    /// Whether a cleanly closed connection still answers retransmissions.
    /// Drop it once this is false.
    #[must_use]
    pub const fn is_lingering(&self) -> bool {
        self.linger_until.is_some()
    }

    /// The remote retransmitted its SYN: acknowledge it again.
    pub fn on_syn(&mut self) {
        self.ack_pending = true;
    }

    fn micros(&self, now: Instant) -> u32 {
        // Wrapping microsecond clock, as on the wire.
        let micros = now.duration_since(self.epoch).as_micros() & u128::from(u32::MAX);
        u32::try_from(micros).unwrap_or(0)
    }

    fn recv_window(&self) -> usize {
        self.config
            .recv_buffer
            .saturating_sub(self.recv_buf.len() + self.reorder_bytes)
    }

    fn push_sent(&mut self, ty: PacketType, payload: Bytes) {
        self.in_flight_bytes += payload.len();
        self.in_flight.push_back(Sent {
            seq: self.seq_nr,
            ty,
            payload,
            first_sent: None,
            transmissions: 0,
            resend: true,
        });
        self.seq_nr = self.seq_nr.wrapping_add(1);
    }

    fn fail(&mut self, error: ConnError) {
        self.error.get_or_insert(error);
        self.state = State::Closed;
        self.in_flight.clear();
        self.in_flight_bytes = 0;
        self.send_buf.clear();
        self.rto_deadline = None;
        self.ack_pending = false;
    }

    /// Closed cleanly once our FIN is acknowledged and theirs received.
    fn maybe_closed(&mut self, now: Instant) {
        if self.is_fin_acked() && self.eof && self.state == State::Connected {
            self.state = State::Closed;
            self.rto_deadline = None;
            self.linger_until = Some(now + LINGER);
        }
    }

    /// The selective ACK of out-of-order packets (up to 256 after the gap).
    fn sack(&self) -> Vec<u8> {
        if self.reorder.is_empty() {
            return Vec::new();
        }
        let mut bits = vec![0u8; 32];
        let mut last = 0;
        for &seq in self.reorder.keys() {
            let i = usize::from(seq.wrapping_sub(self.ack_nr).wrapping_sub(2));
            if let Some(byte) = bits.get_mut(i / 8) {
                *byte |= 1 << (i % 8);
                last = last.max(i / 8);
            }
        }
        bits.truncate(last + 1);
        bits
    }

    fn header(&self, now: Instant, ty: PacketType, seq_nr: u16) -> Packet {
        let mut p = Packet::new(ty, self.send_id);
        if ty == PacketType::Syn {
            p.conn_id = self.recv_id;
        }
        p.timestamp = self.micros(now);
        p.timestamp_diff = self.reply_micro;
        p.wnd_size = u32::try_from(self.recv_window()).unwrap_or(u32::MAX);
        p.seq_nr = seq_nr;
        if ty != PacketType::Syn {
            p.ack_nr = self.ack_nr;
            p.sack = self.sack();
        }
        p
    }

    /// The next datagram to send, if any. Call until it returns `None`.
    pub fn poll_transmit(&mut self, now: Instant) -> Option<Packet> {
        if self.state == State::Closed {
            // The ACK of the peer's FIN (also while lingering).
            if self.error.is_none() && std::mem::take(&mut self.ack_pending) {
                return Some(self.header(now, PacketType::State, self.seq_nr));
            }
            return None;
        }
        let packet = self
            .retransmission(now)
            .or_else(|| self.new_data(now))
            .or_else(|| {
                self.ack_pending
                    .then(|| self.header(now, PacketType::State, self.seq_nr))
            })?;
        self.ack_pending = false;
        self.last_send = now;
        if !self.in_flight.is_empty() && self.rto_deadline.is_none() {
            self.rto_deadline = Some(now + self.rto);
        }
        Some(packet)
    }

    /// Bytes believed to be in the network: sent and neither acknowledged
    /// nor presumed lost.
    fn pipe(&self) -> usize {
        self.in_flight
            .iter()
            .filter(|s| !s.resend && s.transmissions > 0)
            .map(|s| s.payload.len())
            .sum()
    }

    /// Whether `size` more bytes fit the window. With nothing in the
    /// network one packet always fits, so a zero window is probed.
    fn fits(&self, size: usize) -> bool {
        let pipe = self.pipe();
        pipe == 0 || pipe + size <= self.window()
    }

    fn retransmission(&mut self, now: Instant) -> Option<Packet> {
        let index = self.in_flight.iter().position(|s| s.resend)?;
        if !self.fits(self.in_flight.get(index)?.payload.len()) {
            return None;
        }
        let (seq, ty, payload) = {
            let sent = self.in_flight.get_mut(index)?;
            sent.resend = false;
            sent.transmissions += 1;
            if sent.first_sent.is_none() {
                sent.first_sent = Some(now);
            }
            (sent.seq, sent.ty, sent.payload.clone())
        };
        let mut p = self.header(now, ty, seq);
        p.payload = payload;
        Some(p)
    }

    fn window(&self) -> usize {
        self.cwnd.min(self.peer_wnd)
    }

    fn new_data(&mut self, now: Instant) -> Option<Packet> {
        if self.state != State::Connected || self.in_flight.len() >= MAX_IN_FLIGHT {
            return None;
        }
        if !self.send_buf.is_empty() {
            let size = self.send_buf.len().min(self.config.max_payload.max(1));
            if !self.fits(size) {
                return None;
            }
            let payload = self.send_buf.split_to(size).freeze();
            return Some(self.send_new(now, PacketType::Data, payload));
        }
        if self.close_requested && self.fin_seq.is_none() {
            self.fin_seq = Some(self.seq_nr);
            return Some(self.send_new(now, PacketType::Fin, Bytes::new()));
        }
        None
    }

    fn send_new(&mut self, now: Instant, ty: PacketType, payload: Bytes) -> Packet {
        let seq = self.seq_nr;
        self.push_sent(ty, payload.clone());
        if let Some(sent) = self.in_flight.back_mut() {
            sent.resend = false;
            sent.transmissions = 1;
            sent.first_sent = Some(now);
        }
        let mut p = self.header(now, ty, seq);
        p.payload = payload;
        p
    }

    /// When [`on_timeout`](Self::on_timeout) must next be called.
    #[must_use]
    pub fn poll_timeout(&self) -> Option<Instant> {
        if self.state == State::Closed {
            return self.linger_until;
        }
        let idle = self.last_recv + self.config.idle_timeout;
        let keepalive = matches!(self.state, State::Connected | State::SynRecv)
            .then(|| self.last_send + self.config.keepalive);
        [self.rto_deadline, Some(idle), keepalive]
            .into_iter()
            .flatten()
            .min()
    }

    /// The peer stopped answering. If everything was delivered and the peer
    /// closed too, and only the acknowledgement of our FIN never came, that
    /// is a clean close for the application (as with TCP); otherwise the
    /// connection timed out.
    fn give_up(&mut self, now: Instant) {
        let only_fin = self.in_flight.len() == 1
            && self
                .in_flight
                .front()
                .is_some_and(|s| s.ty == PacketType::Fin);
        if only_fin && self.eof {
            self.in_flight.clear();
            self.in_flight_bytes = 0;
            self.maybe_closed(now);
            if self.state == State::Closed {
                return;
            }
        }
        self.fail(ConnError::TimedOut);
    }

    /// Handles expired timers.
    pub fn on_timeout(&mut self, now: Instant) {
        if self.state == State::Closed {
            if self.linger_until.is_some_and(|t| now >= t) {
                self.linger_until = None;
            }
            return;
        }
        if now >= self.last_recv + self.config.idle_timeout {
            return self.give_up(now);
        }
        if self.rto_deadline.is_some_and(|d| now >= d) {
            self.timeouts += 1;
            let limit = if self.state == State::SynSent {
                self.config.syn_retries
            } else {
                self.config.max_retransmits
            };
            if self.timeouts > limit {
                return self.give_up(now);
            }
            self.rto = (self.rto * 2).min(MAX_RTO);
            // Everything unacknowledged is presumed lost and resent from
            // the oldest as the window reopens in slow start (as TCP).
            let mss = self.config.max_payload.max(1);
            self.ssthresh = (self.cwnd / 2).max(2 * mss);
            self.cwnd = mss;
            self.slow_start = true;
            for sent in &mut self.in_flight {
                sent.resend = true;
            }
            self.rto_deadline = Some(now + self.rto);
        }
        if matches!(self.state, State::Connected | State::SynRecv)
            && now >= self.last_send + self.config.keepalive
        {
            self.ack_pending = true;
        }
    }

    /// Handles a datagram addressed to this connection.
    pub fn on_packet(&mut self, now: Instant, p: &Packet) {
        if self.state == State::Closed {
            if self.is_lingering() && matches!(p.ty, PacketType::Data | PacketType::Fin) {
                // Our last ACK was lost: repeat it, and keep listening.
                self.ack_pending = true;
                self.linger_until = Some(now + LINGER);
            }
            return;
        }
        if p.ty == PacketType::Reset {
            return self.fail(ConnError::Reset);
        }
        if p.ty == PacketType::Syn {
            return self.on_syn();
        }
        if self.state == State::SynSent {
            let syn = self.in_flight.front().map(|s| s.seq);
            if p.ty != PacketType::State || Some(p.ack_nr) != syn {
                return;
            }
            self.state = State::Connected;
            // The acceptor's first data packet carries this sequence number.
            self.ack_nr = p.seq_nr.wrapping_sub(1);
            // Confirm, so an acceptor waiting for us may start sending.
            self.ack_pending = true;
        } else if self.state == State::SynRecv {
            self.state = State::Connected;
        }
        self.last_recv = now;
        self.peer_wnd = p.wnd_size as usize;
        self.reply_micro = self.micros(now).wrapping_sub(p.timestamp);
        self.on_ack(now, p);
        if matches!(p.ty, PacketType::Data | PacketType::Fin) {
            self.on_data(p);
        }
        self.maybe_closed(now);
    }

    fn on_ack(&mut self, now: Instant, p: &Packet) {
        let Some(oldest) = self.in_flight.front().map(|s| s.seq) else {
            return;
        };
        // Only acknowledgements of packets we actually sent count.
        let cumulative = !seq_after(oldest, p.ack_nr.wrapping_add(1))
            && !seq_after(p.ack_nr, self.seq_nr.wrapping_sub(1));
        let mut acked_bytes = 0;
        let mut rtt_sample = None;
        let mut take = |s: &Sent| {
            acked_bytes += s.payload.len();
            if s.transmissions == 1
                && let Some(at) = s.first_sent
            {
                rtt_sample = Some(now.duration_since(at));
            }
        };
        if cumulative {
            while self
                .in_flight
                .front()
                .is_some_and(|s| !seq_after(s.seq, p.ack_nr))
            {
                if let Some(s) = self.in_flight.pop_front() {
                    take(&s);
                }
            }
        }
        if !p.sack.is_empty() {
            self.in_flight.retain(|s| {
                let sacked = p.sack_has(s.seq);
                if sacked {
                    take(s);
                }
                !sacked
            });
        }
        self.in_flight_bytes = self.in_flight.iter().map(|s| s.payload.len()).sum();
        let progressed = acked_bytes > 0 || rtt_sample.is_some();
        if let Some(rtt) = rtt_sample {
            self.update_rtt(rtt);
        }
        if progressed {
            // The path delivers again: drop the back-off.
            if let (Some(srtt), true) = (self.srtt, self.timeouts > 0) {
                self.rto = (srtt + self.rttvar * 4).clamp(MIN_RTO, MAX_RTO);
            }
            self.timeouts = 0;
            self.dup_acks = 0;
            self.rto_deadline = (!self.in_flight.is_empty()).then(|| now + self.rto);
            self.update_cwnd(now, acked_bytes, p.timestamp_diff);
        } else if cumulative
            && p.ty == PacketType::State
            && p.ack_nr == self.last_ack
            && !self.in_flight.is_empty()
        {
            self.dup_acks += 1;
            if self.dup_acks == 3 {
                self.lost_oldest(now);
            }
        }
        if cumulative {
            self.last_ack = p.ack_nr;
        }
        // Three later packets arrived while the oldest did not: lost.
        if !p.sack.is_empty() {
            let later = (0..256u16)
                .filter(|i| p.sack_has(p.ack_nr.wrapping_add(2).wrapping_add(*i)))
                .count();
            let stale = self.in_flight.front().is_some_and(|s| {
                s.transmissions == 1 || s.first_sent.is_some_and(|t| now >= t + self.rto)
            });
            if later >= 3 && stale {
                self.lost_oldest(now);
            }
        }
    }

    /// Fast retransmit: resend the oldest packet and cut the window, at most
    /// once per round trip.
    fn lost_oldest(&mut self, now: Instant) {
        let Some(oldest) = self.in_flight.front_mut() else {
            return;
        };
        if oldest.resend {
            return;
        }
        oldest.resend = true;
        let rtt = self.srtt.unwrap_or(INITIAL_RTO);
        if self.last_cut.is_none_or(|t| now >= t + rtt) {
            self.cwnd = (self.cwnd / 2).max(2 * self.config.max_payload.max(1));
            self.ssthresh = self.cwnd;
            self.slow_start = false;
            self.last_cut = Some(now);
        }
    }

    /// RFC 6298 estimator.
    fn update_rtt(&mut self, sample: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(sample);
                self.rttvar = sample / 2;
            }
            Some(srtt) => {
                let diff = srtt.abs_diff(sample);
                self.rttvar = (self.rttvar * 3 + diff) / 4;
                self.srtt = Some((srtt * 7 + sample) / 8);
            }
        }
        let srtt = self.srtt.unwrap_or(sample);
        self.rto = (srtt + self.rttvar * 4).clamp(MIN_RTO, MAX_RTO);
    }

    /// LEDBAT window update for `acked` bytes with a one-way delay sample.
    fn update_cwnd(&mut self, now: Instant, acked: usize, delay: u32) {
        let mss = self.config.max_payload.max(1);
        let target = i64::try_from(self.config.target_delay.as_micros())
            .unwrap_or(i64::MAX)
            .max(1);
        let queuing = if delay == 0 {
            0
        } else {
            self.delays.add(now, delay);
            let base = self.delays.base().unwrap_or(delay);
            // Clocks are unrelated; only the difference to the base means
            // something. Clamp nonsense (wrapped or > 10 s) to "on target".
            match delay.wrapping_sub(base) {
                d if d > 10_000_000 => target,
                d => i64::from(d),
            }
        };
        let acked = i64::try_from(acked).unwrap_or(i64::MAX);
        let cwnd = i64::try_from(self.cwnd).unwrap_or(i64::MAX).max(1);
        if self.slow_start && self.cwnd < self.ssthresh && queuing < target * 3 / 4 {
            self.cwnd = self
                .cwnd
                .saturating_add(usize::try_from(acked).unwrap_or(0));
        } else {
            self.slow_start = false;
            let off_target = target - queuing;
            let gain = MAX_CWND_INCREASE * off_target / target * acked / cwnd;
            let next = (cwnd + gain).max(i64::try_from(2 * mss).unwrap_or(i64::MAX));
            self.cwnd = usize::try_from(next).unwrap_or(2 * mss);
        }
        let ceiling = self.config.send_buffer.max(2 * mss);
        self.cwnd = self.cwnd.clamp(2 * mss, ceiling);
    }

    fn on_data(&mut self, p: &Packet) {
        self.ack_pending = true;
        if p.ty == PacketType::Fin && self.their_fin.is_none() {
            self.their_fin = Some(p.seq_nr);
        }
        let offset = p.seq_nr.wrapping_sub(self.ack_nr);
        if offset == 0 || offset > MAX_REORDER {
            // Already delivered (or absurdly far ahead): the ACK repeats.
            return;
        }
        if offset == 1 {
            self.deliver(p.seq_nr, &p.payload);
            while let Some(next) = self.reorder.remove(&self.ack_nr.wrapping_add(1)) {
                self.reorder_bytes -= next.len();
                self.deliver(self.ack_nr.wrapping_add(1), &next);
            }
        } else if self.recv_window() >= p.payload.len() && !self.reorder.contains_key(&p.seq_nr) {
            self.reorder_bytes += p.payload.len();
            self.reorder.insert(p.seq_nr, p.payload.clone());
        }
    }

    fn deliver(&mut self, seq: u16, payload: &[u8]) {
        if self.their_fin.is_some_and(|fin| seq_after(seq, fin)) {
            return;
        }
        self.recv_buf.extend_from_slice(payload);
        self.ack_nr = seq;
        if self.their_fin == Some(seq) {
            self.eof = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_order_wraps() {
        assert!(seq_after(1, 0));
        assert!(seq_after(0, 0xffff));
        assert!(!seq_after(0, 1));
        assert!(!seq_after(5, 5));
    }

    #[test]
    fn delay_history_keeps_recent_minimum() {
        let t = Instant::now();
        let mut h = DelayHistory::default();
        h.add(t, 500);
        h.add(t + Duration::from_secs(1), 300);
        assert_eq!(h.base(), Some(300));
        for minute in 1..=3u64 {
            h.add(t + Duration::from_secs(60 * minute + 1), 900);
        }
        assert_eq!(h.base(), Some(900), "old minimum aged out");
    }

    #[test]
    fn ledbat_grows_below_target_and_shrinks_above() {
        let t = Instant::now();
        let mut c = Connection::connect(Config::default(), t, 1);
        c.slow_start = false;
        c.cwnd = 100_000;
        // Base delay 10 ms: no queuing, the window grows.
        c.update_cwnd(t, 10_000, 10_000);
        let grown = c.cwnd;
        assert!(grown > 100_000, "{grown}");
        // 250 ms one-way delay (240 ms queuing, over the 100 ms target).
        c.update_cwnd(t, 10_000, 250_000);
        assert!(c.cwnd < grown, "{} >= {grown}", c.cwnd);
        // Never below two packets.
        for _ in 0..10_000 {
            c.update_cwnd(t, 50_000, 1_000_000);
        }
        assert_eq!(c.cwnd, 2 * c.config.max_payload);
    }

    #[test]
    fn slow_start_doubles_until_delay_rises() {
        let t = Instant::now();
        let mut c = Connection::connect(Config::default(), t, 1);
        let start = c.cwnd;
        c.update_cwnd(t, start, 5_000);
        assert_eq!(c.cwnd, 2 * start);
        c.update_cwnd(t, 1000, 5_000 + 90_000);
        assert!(!c.slow_start, "queuing above 3/4 of the target ends it");
    }

    #[test]
    fn rto_follows_rfc_6298() {
        let mut c = Connection::connect(Config::default(), Instant::now(), 1);
        c.update_rtt(Duration::from_millis(200));
        assert_eq!(c.rto, Duration::from_millis(600));
        c.update_rtt(Duration::from_millis(200));
        assert_eq!(c.rto, Duration::from_millis(500), "clamped at the minimum");
    }
}
