//! uTP, the micro transport protocol (BEP 29), for Dravity.
//!
//! uTP carries BitTorrent connections over UDP with LEDBAT congestion
//! control: it backs off as soon as it adds queuing delay, so a seeding
//! client does not swamp the user's other traffic, and it reaches peers
//! behind NATs that only pass UDP.
//!
//! * [`packet`] — the wire format, including selective ACKs.
//! * [`Connection`] — the sans-IO state machine (handshake, reliability,
//!   LEDBAT, flow control). It never performs I/O or reads the clock.
//! * [`UtpSocket`] / [`UtpStream`] — a Tokio driver that multiplexes
//!   connections on one UDP socket; streams implement `AsyncRead` and
//!   `AsyncWrite`.
//!
//! ```no_run
//! # async fn demo() -> std::io::Result<()> {
//! use dravity_utp::{Config, UtpSocket};
//! use tokio::io::AsyncWriteExt;
//!
//! let socket = UtpSocket::bind("0.0.0.0:0".parse().unwrap(), Config::default()).await?;
//! let mut stream = socket.connect("192.0.2.1:6881".parse().unwrap()).await?;
//! stream.write_all(b"hello").await?;
//! # Ok(()) }
//! ```

mod conn;
pub mod packet;
mod socket;

pub use conn::{Config, ConnError, Connection};
pub use packet::{Packet, PacketError, PacketType};
pub use socket::{UtpSocket, UtpStream};
