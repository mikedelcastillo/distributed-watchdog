use std::{
    net::UdpSocket,
    process::{Command, Stdio},
};

use anyhow::{Context, bail};

pub fn wake_on_lan(mac: &str, broadcast: &str) -> anyhow::Result<()> {
    let mac = parse_mac(mac)?;
    let mut packet = Vec::with_capacity(102);
    packet.extend_from_slice(&[0xff; 6]);
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }

    let socket = UdpSocket::bind("0.0.0.0:0").context("failed to bind UDP socket")?;
    socket
        .set_broadcast(true)
        .context("failed to enable UDP broadcast")?;
    socket
        .send_to(&packet, broadcast)
        .with_context(|| format!("failed to send Wake-on-LAN packet to {broadcast}"))?;
    Ok(())
}

pub fn shutdown_local(delay_seconds: u64, reason: &str) -> anyhow::Result<u64> {
    #[cfg(windows)]
    {
        let status = Command::new("shutdown")
            .args(["/s", "/t", &delay_seconds.to_string(), "/c", reason])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("failed to invoke Windows shutdown")?;
        if !status.success() {
            bail!("Windows shutdown command rejected the request");
        }
        Ok(delay_seconds)
    }

    #[cfg(unix)]
    {
        let (time_arg, actual_delay_seconds) = if delay_seconds == 0 {
            ("now".to_string(), 0)
        } else {
            let minutes = delay_seconds.div_ceil(60).max(1);
            (format!("+{minutes}"), minutes * 60)
        };
        let status = Command::new("shutdown")
            .args(["-h", &time_arg, reason])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("failed to invoke Unix shutdown")?;
        if !status.success() {
            bail!("Unix shutdown command rejected the request");
        }
        Ok(actual_delay_seconds)
    }

    #[cfg(not(any(unix, windows)))]
    {
        bail!("shutdown is not implemented on this platform");
    }
}

fn parse_mac(mac: &str) -> anyhow::Result<[u8; 6]> {
    let cleaned: String = mac.chars().filter(|ch| ch.is_ascii_hexdigit()).collect();
    if cleaned.len() != 12 {
        bail!("invalid MAC address {mac}");
    }

    let mut bytes = [0u8; 6];
    for index in 0..6 {
        bytes[index] = u8::from_str_radix(&cleaned[index * 2..index * 2 + 2], 16)
            .with_context(|| format!("invalid MAC address {mac}"))?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mac_accepts_common_separators() {
        assert_eq!(
            parse_mac("aa:bb-cc:dd-ee:ff").unwrap(),
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
    }

    #[test]
    fn parse_mac_rejects_wrong_length() {
        assert!(parse_mac("aa:bb:cc").is_err());
    }
}
