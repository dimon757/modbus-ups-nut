use crate::config::Endpoint;
use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::net::UdpSocket;

/// Sends Wake-on-LAN magic packets to every endpoint. Called once the state
/// machine confirms recovery: grid restored, healthy past the recovery
/// debounce, AND we know we actually shut things down (see
/// `state::shutdown_fired`) -- this must never fire on an ordinary grid
/// blip that never reached a shutdown.
///
/// This only wakes machines that went down via the graceful SSH shutdown
/// path, where NIC standby power survived. If the inverter's own hardware
/// protection cut output first (no standby power, so no WOL listener), this
/// does nothing -- those machines instead depend on each one's own BIOS
/// "Power On after AC Loss" setting once the inverter's output returns.
pub async fn wake_all(endpoints: &[Endpoint], broadcast_addr: &str) {
    let socket = match bind_broadcast_socket().await {
        Ok(s) => s,
        Err(e) => {
            for ep in endpoints {
                log::error!("WOL failed for {} ({}): {:#}", ep.name, ep.mac_address, e);
            }
            return;
        }
    };
    for ep in endpoints {
        if let Err(e) = wake_one(&socket, &ep.mac_address, broadcast_addr).await {
            log::error!("WOL failed for {} ({}): {:#}", ep.name, ep.mac_address, e);
        } else {
            log::info!("WOL sent to {} ({})", ep.name, ep.mac_address);
        }
    }
}

/// Sends one Wake-on-LAN round now, then `resend_count` more,
/// `resend_interval` apart.
///
/// One round is not enough: if the grid came back while the shutdown
/// sequence was still running, a Proxmox host can still be inside
/// VM shutdown (walking its VMs down) when the first round arrives. A
/// machine that is still on ignores the packet, then finishes powering off
/// and would stay off on grid power. Later rounds catch it once it's down;
/// machines already running ignore them.
#[allow(dead_code)]
pub async fn wake_all_repeated(
    endpoints: &[Endpoint],
    broadcast_addr: &str,
    resend_count: u32,
    resend_interval: Duration,
) {
    for round in 0..=resend_count {
        if round > 0 {
            tokio::time::sleep(resend_interval).await;
        }
        log::info!("Wake-on-LAN round {}/{}", round + 1, resend_count + 1);
        wake_all(endpoints, broadcast_addr).await;
    }
}

async fn bind_broadcast_socket() -> Result<UdpSocket> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("binding UDP socket for WOL")?;
    socket
        .set_broadcast(true)
        .context("enabling SO_BROADCAST")?;
    Ok(socket)
}

/// Async so the current_thread runtime is not blocked while the UDP send
/// waits on the kernel. Uses tokio's UdpSocket rather than std::net.
async fn wake_one(socket: &UdpSocket, mac: &str, broadcast_addr: &str) -> Result<()> {
    let mac_bytes = parse_mac(mac)?;

    let mut packet = [0xFFu8; 6 + 16 * 6];
    for chunk in packet[6..].chunks_exact_mut(6) {
        chunk.copy_from_slice(&mac_bytes);
    }

    socket
        .send_to(&packet, broadcast_addr)
        .await
        .with_context(|| format!("sending magic packet to {}", broadcast_addr))?;

    Ok(())
}

pub(crate) fn parse_mac(mac: &str) -> Result<[u8; 6]> {
    let trimmed = mac.trim();
    // One separator style throughout: "AA:BB:.." or "AA-BB-..", never mixed.
    let sep = if trimmed.contains(':') { ':' } else { '-' };
    if trimmed.contains(':') && trimmed.contains('-') {
        bail!("invalid MAC address {:?}: mixes ':' and '-' separators", mac);
    }
    let parts: Vec<&str> = trimmed.split(sep).collect();
    if parts.len() != 6 {
        bail!("invalid MAC address {:?}: expected 6 colon/dash-separated octets", mac);
    }
    let mut out = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        // Exactly two hex digits: `from_str_radix` alone would also accept
        // "+F" or "0AA", silently turning a typo into some other MAC.
        if p.len() != 2 || !p.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("invalid octet {:?} in MAC address {:?}: expected two hex digits", p, mac);
        }
        out[i] = u8::from_str_radix(p, 16)
            .with_context(|| format!("invalid octet {:?} in MAC address {:?}", p, mac))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EndpointKind;

    fn endpoint(mac: &str) -> Endpoint {
        Endpoint {
            name: mac.into(),
            kind: EndpointKind::Windows,
            host: "10.0.0.1".into(),
            ssh_user: "u".into(),
            ssh_key_path: "/dev/null".into(),
            shutdown_delay_secs: 0,
            mac_address: mac.into(),
            ssh_connect_retry_secs: None,
        }
    }

    #[test]
    fn parse_mac_handles_whitespace_and_separators() {
        assert_eq!(
            parse_mac(" AA:BB:CC:DD:EE:FF \n").unwrap(),
            [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]
        );
        assert_eq!(
            parse_mac("11-22-33-44-55-66").unwrap(),
            [0x11, 0x22, 0x33, 0x44, 0x55, 0x66]
        );
        assert!(parse_mac("AA:BB:CC:DD:EE").is_err());
        assert!(parse_mac("AA:BB:CC:DD:EE:GG").is_err());
    }

    #[test]
    fn parse_mac_rejects_malformed_octets_and_mixed_separators() {
        assert!(parse_mac("AA:BB:CC:DD:EE:+F").is_err());
        assert!(parse_mac("AA:BB:CC:DD:EE:0AA").is_err());
        assert!(parse_mac("AA:BB:CC:DD:EE:F").is_err());
        assert!(parse_mac("AA:BB:CC:DD:EE: F").is_err());
        assert!(parse_mac("AA:BB-CC:DD:EE:FF").is_err());
        assert_eq!(parse_mac("aa:bb:cc:dd:ee:ff").unwrap(), [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    }

    #[tokio::test]
    async fn sends_first_round_plus_resends() {
        // Receiver stays on std::net so the test does not need a second
        // runtime task just to read packets.
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let addr = rx.local_addr().unwrap().to_string();
        let eps = [endpoint("AA:BB:CC:DD:EE:01"), endpoint("AA-BB-CC-DD-EE-02")];

        wake_all_repeated(&eps, &addr, 2, Duration::ZERO).await;

        // 3 rounds x 2 endpoints, each a standard 102-byte magic packet.
        let mut buf = [0u8; 256];
        for _ in 0..6 {
            let n = rx.recv(&mut buf).expect("expected another magic packet");
            assert_eq!(n, 102);
            assert_eq!(&buf[..6], &[0xFF; 6]);
        }
    }
}
