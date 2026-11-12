//! DSCP marking of peer TCP connections (`IP_TOS` / `IPV6_TCLASS`).

use std::net::SocketAddr;

use tokio::net::TcpStream;

/// Largest DSCP value (six bits).
pub const MAX_DSCP: u8 = 63;

/// The TOS / traffic class byte for a DSCP value (ECN bits left clear).
#[must_use]
pub(crate) const fn tos_byte(dscp: u8) -> u32 {
    ((dscp & MAX_DSCP) as u32) << 2
}

/// Marks `stream` with `dscp`. Failure is logged and otherwise ignored:
/// marking is a hint to routers, never a reason to drop a peer.
pub(crate) fn mark(stream: &TcpStream, dscp: Option<u8>, addr: SocketAddr) {
    let Some(dscp) = dscp else {
        return;
    };
    let sock = socket2::SockRef::from(stream);
    let result = if addr.is_ipv4() {
        sock.set_tos_v4(tos_byte(dscp))
    } else {
        set_v6(&sock, tos_byte(dscp))
    };
    if let Err(e) = result {
        tracing::debug!(%addr, dscp, error = %e, "DSCP marking failed");
    }
}

#[cfg(any(
    target_os = "android",
    target_os = "freebsd",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
))]
fn set_v6(sock: &socket2::SockRef<'_>, tclass: u32) -> std::io::Result<()> {
    sock.set_tclass_v6(tclass)
}

#[cfg(not(any(
    target_os = "android",
    target_os = "freebsd",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
)))]
fn set_v6(_sock: &socket2::SockRef<'_>, _tclass: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn dscp_goes_in_the_top_six_bits() {
        assert_eq!(tos_byte(0), 0);
        assert_eq!(tos_byte(8), 0x20); // CS1, "lower effort"
        assert_eq!(tos_byte(46), 0xb8); // EF
        assert_eq!(tos_byte(0xff), 0xfc);
    }

    #[tokio::test]
    async fn marks_a_real_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).await.unwrap();
        mark(&stream, Some(8), addr);
        #[cfg(target_os = "linux")]
        assert_eq!(socket2::SockRef::from(&stream).tos_v4().unwrap(), 0x20);
    }
}
