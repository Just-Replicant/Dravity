//! Two sans-IO connections over a simulated network with delay, loss and
//! reordering, on a virtual clock.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::cast_possible_truncation
)]

use std::time::{Duration, Instant};

use dravity_utp::{Config, ConnError, Connection, Packet, PacketType};
use proptest::prelude::*;

#[derive(Debug, Clone, Copy)]
struct Link {
    delay: Duration,
    jitter: Duration,
    /// Per-mille of packets dropped.
    loss: u32,
}

struct Sim {
    now: Instant,
    rng: fastrand::Rng,
    link: Link,
    /// (deliver at, to side, packet)
    wire: Vec<(Instant, usize, Packet)>,
    sides: [Option<Connection>; 2],
    sent: u64,
}

/// Patient limits: these tests check delivery, not when to give up.
fn config() -> Config {
    let mut c = Config::default();
    c.max_retransmits = 12;
    c.syn_retries = 12;
    c.idle_timeout = Duration::from_secs(600);
    c
}

impl Sim {
    fn new(link: Link, seed: u64) -> Self {
        let now = Instant::now();
        let client = Connection::connect(config(), now, 100);
        Self {
            now,
            rng: fastrand::Rng::with_seed(seed),
            link,
            wire: Vec::new(),
            sides: [Some(client), None],
            sent: 0,
        }
    }

    fn conn(&mut self, side: usize) -> &mut Connection {
        self.sides[side].as_mut().unwrap()
    }

    /// Moves packets onto the wire, delivers due ones and runs timers.
    fn step(&mut self) {
        for side in 0..2 {
            let Some(conn) = self.sides[side].as_mut() else {
                continue;
            };
            while let Some(p) = conn.poll_transmit(self.now) {
                self.sent += 1;
                // The SYN is never dropped so every run connects.
                if p.ty != PacketType::Syn && self.rng.u32(0..1000) < self.link.loss {
                    continue;
                }
                let jitter = self.link.jitter.mul_f64(self.rng.f64());
                // Round-trip encode/decode: the simulator carries bytes.
                let p = Packet::decode(&p.encode()).unwrap();
                self.wire
                    .push((self.now + self.link.delay + jitter, 1 - side, p));
            }
        }
        let next_wire = self.wire.iter().map(|(t, _, _)| *t).min();
        let next_timer = self
            .sides
            .iter()
            .flatten()
            .filter_map(Connection::poll_timeout)
            .min();
        let Some(next) = [next_wire, next_timer].into_iter().flatten().min() else {
            return;
        };
        self.now = self.now.max(next);
        let (due, rest): (Vec<_>, Vec<_>) =
            self.wire.drain(..).partition(|(t, _, _)| *t <= self.now);
        self.wire = rest;
        for (_, to, p) in due {
            match &mut self.sides[to] {
                Some(conn) => conn.on_packet(self.now, &p),
                None if p.ty == PacketType::Syn => {
                    self.sides[to] = Some(Connection::accept(config(), self.now, &p));
                }
                None => {}
            }
        }
        for conn in self.sides.iter_mut().flatten() {
            conn.on_timeout(self.now);
        }
    }
}

/// Sends `a_to_b` and `b_to_a` concurrently, closes both sides and checks
/// that everything arrived intact. Returns the simulated duration.
fn transfer(link: Link, seed: u64, a_to_b: &[u8], b_to_a: &[u8]) -> Duration {
    let mut sim = Sim::new(link, seed);
    let start = sim.now;
    let payloads = [a_to_b, b_to_a];
    let mut written = [0usize; 2];
    let mut received: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
    let mut buf = vec![0u8; 64 * 1024];
    for _ in 0..2_000_000 {
        sim.step();
        for side in 0..2 {
            if sim.sides[side].is_none() {
                continue;
            }
            let data = payloads[side];
            let conn = sim.conn(side);
            if conn.is_connected() && written[side] < data.len() {
                written[side] += conn.write(&data[written[side]..]);
                if written[side] == data.len() {
                    conn.close();
                }
            } else if conn.is_connected() && data.is_empty() {
                conn.close();
            }
            loop {
                let n = conn.read(&mut buf);
                if n == 0 {
                    break;
                }
                received[side].extend_from_slice(&buf[..n]);
            }
            assert_eq!(conn.error(), None, "side {side} failed");
        }
        let done = sim
            .sides
            .iter()
            .all(|c| c.as_ref().is_some_and(Connection::is_closed));
        if done {
            assert_eq!(received[1], a_to_b, "a -> b");
            assert_eq!(received[0], b_to_a, "b -> a");
            return sim.now.duration_since(start);
        }
    }
    panic!(
        "transfer did not finish: got {} / {} and {} / {}",
        received[1].len(),
        a_to_b.len(),
        received[0].len(),
        b_to_a.len()
    );
}

fn data(len: usize, salt: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i % 256) as u8)
        .map(|b| b.wrapping_mul(31).wrapping_add(salt))
        .collect()
}

const LAN: Link = Link {
    delay: Duration::from_millis(5),
    jitter: Duration::ZERO,
    loss: 0,
};

#[test]
fn transfers_both_ways_and_closes() {
    transfer(LAN, 1, &data(1 << 20, 1), &data(300_000, 2));
}

#[test]
fn empty_streams_close() {
    transfer(LAN, 2, &[], &[]);
}

#[test]
fn survives_loss_and_reordering() {
    let lossy = Link {
        delay: Duration::from_millis(20),
        jitter: Duration::from_millis(15),
        loss: 50,
    };
    transfer(lossy, 3, &data(400_000, 3), &data(100_000, 4));
}

#[test]
fn window_grows_on_a_clean_link() {
    let mut sim = Sim::new(LAN, 4);
    let payload = data(4 << 20, 5);
    let mut written = 0;
    let mut buf = vec![0u8; 1 << 16];
    for _ in 0..200_000 {
        sim.step();
        if sim.sides[1].is_none() {
            continue;
        }
        let conn = sim.conn(0);
        if conn.is_connected() && written < payload.len() {
            written += conn.write(&payload[written..]);
        }
        while sim.conn(1).read(&mut buf) > 0 {}
        if written == payload.len() {
            break;
        }
    }
    let cwnd = sim.conn(0).congestion_window();
    assert!(cwnd > 64 * 1024, "window stayed at {cwnd}");
    let rtt = sim.conn(0).rtt().unwrap();
    assert!(rtt >= Duration::from_millis(10), "{rtt:?}");
}

#[test]
fn reset_fails_the_connection() {
    let mut sim = Sim::new(LAN, 5);
    while sim.sides[1].is_none() || !sim.conn(0).is_connected() {
        sim.step();
    }
    let mut reset = Packet::new(PacketType::Reset, 0);
    reset.conn_id = sim.conn(0).recv_id();
    let now = sim.now;
    sim.conn(0).on_packet(now, &reset);
    assert!(sim.conn(0).is_closed());
    assert_eq!(sim.conn(0).error(), Some(ConnError::Reset));
    assert_eq!(sim.conn(0).write(b"x"), 0);
}

#[test]
fn unanswered_syn_times_out() {
    let start = Instant::now();
    let mut conn = Connection::connect(Config::default(), start, 7);
    let mut now = start;
    let mut syns = 0;
    while !conn.is_closed() {
        while let Some(p) = conn.poll_transmit(now) {
            assert_eq!(p.ty, PacketType::Syn);
            syns += 1;
        }
        now = conn.poll_timeout().unwrap();
        conn.on_timeout(now);
    }
    assert_eq!(conn.error(), Some(ConnError::TimedOut));
    assert_eq!(syns, 3, "first SYN plus two retries");
    assert!(now.duration_since(start) >= Duration::from_secs(7));
}

#[test]
fn silent_peer_times_out_and_keepalives_are_sent() {
    let mut sim = Sim::new(LAN, 6);
    while sim.sides[1].is_none() || !sim.conn(0).is_connected() {
        sim.step();
    }
    // Side 1 vanishes.
    sim.sides[1] = None;
    let mut keepalives = 0;
    let mut now = sim.now;
    let conn = sim.conn(0);
    while !conn.is_closed() {
        while let Some(p) = conn.poll_transmit(now) {
            if p.ty == PacketType::State {
                keepalives += 1;
            }
        }
        now = conn.poll_timeout().unwrap();
        conn.on_timeout(now);
    }
    assert_eq!(conn.error(), Some(ConnError::TimedOut));
    assert!(keepalives >= 2, "{keepalives}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn any_lossy_link_delivers_everything(
        seed in any::<u64>(),
        loss in 0u32..150,
        delay_ms in 1u64..80,
        jitter_ms in 0u64..60,
        len_a in 0usize..120_000,
        len_b in 0usize..40_000,
    ) {
        let link = Link {
            delay: Duration::from_millis(delay_ms),
            jitter: Duration::from_millis(jitter_ms),
            loss,
        };
        transfer(link, seed, &data(len_a, 7), &data(len_b, 8));
    }
}
