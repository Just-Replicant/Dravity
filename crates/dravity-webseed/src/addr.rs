//! Which addresses a web seed may connect to.
//!
//! A torrent's `url-list` (or a magnet's `ws`) is attacker-controlled, so by
//! default a web seed may only reach publicly routable addresses. The
//! classification itself lives in [`dravity_core::net`] so trackers and
//! peers share it.

pub use dravity_core::net::is_public;
