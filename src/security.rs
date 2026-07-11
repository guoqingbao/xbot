use std::net::{IpAddr, ToSocketAddrs};

use ipnet::IpNet;
use regex::Regex;
use url::Url;

/// Check if an IP is in private ranges (RFC 1918 + loopback + link-local)
pub fn is_private_ip(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.octets()[0] == 10
                || (v4.octets()[0] == 172 && (v4.octets()[1] >= 16 && v4.octets()[1] <= 31))
                || (v4.octets()[0] == 192 && v4.octets()[1] == 168)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.octets()[0] >= 0xfc && v6.octets()[0] <= 0xfd)
                || (v6.octets()[0] == 0xfe && (v6.octets()[1] & 0xc0) == 0x80)
        }
    }
}

fn blocked_networks() -> Vec<IpNet> {
    [
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "::1/128",
        "fc00::/7",
        "fe80::/10",
    ]
    .into_iter()
    .filter_map(|cidr| cidr.parse::<IpNet>().ok())
    .collect()
}

/// Check if IP is in blocked private ranges
pub fn is_in_blocked_network(addr: IpAddr) -> bool {
    blocked_networks().iter().any(|net| net.contains(&addr))
}

fn resolve_host(host: &str) -> Result<Vec<IpAddr>, String> {
    let addrs = (host, 0)
        .to_socket_addrs()
        .map_err(|_| format!("Cannot resolve hostname: {host}"))?;
    Ok(addrs.map(|addr| addr.ip()).collect())
}

pub fn validate_url_target(url: &str, allowed_networks: &[IpNet]) -> (bool, String) {
    let parsed = match Url::parse(url) {
        Ok(url) => url,
        Err(err) => return (false, err.to_string()),
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return (
            false,
            format!("Only http/https allowed, got '{}'", parsed.scheme()),
        );
    }
    let Some(host) = parsed.host_str() else {
        return (false, "Missing hostname".to_string());
    };
    let resolved = match resolve_host(host) {
        Ok(addrs) => addrs,
        Err(err) => return (false, err),
    };
    for addr in resolved {
        // Check if IP is in blocked ranges
        if is_in_blocked_network(addr) {
            // Allow if it's in the whitelist
            if !allowed_networks.iter().any(|net| net.contains(&addr)) {
                return (
                    false,
                    format!("Blocked: {host} resolves to private address {addr}"),
                );
            }
        }
    }
    (true, String::new())
}

pub fn validate_resolved_url(url: &str, allowed_networks: &[IpNet]) -> (bool, String) {
    let Ok(parsed) = Url::parse(url) else {
        return (true, String::new());
    };
    let Some(host) = parsed.host_str() else {
        return (true, String::new());
    };
    if let Ok(addr) = host.parse::<IpAddr>() {
        if is_in_blocked_network(addr) && !allowed_networks.iter().any(|net| net.contains(&addr)) {
            return (
                false,
                format!("Redirect target is a private address: {addr}"),
            );
        }
        return (true, String::new());
    }
    match resolve_host(host) {
        Ok(addrs) => {
            for addr in addrs {
                if is_in_blocked_network(addr) && !allowed_networks.iter().any(|net| net.contains(&addr)) {
                    return (
                        false,
                        format!("Redirect target {host} resolves to private address {addr}"),
                    );
                }
            }
            (true, String::new())
        }
        Err(_) => (true, String::new()),
    }
}

pub fn contains_internal_url(command: &str, allowed_networks: &[IpNet]) -> bool {
    let re = Regex::new(r#"https?://[^\s"'`;|<>]+"#).expect("valid URL regex");
    for m in re.find_iter(command) {
        let (ok, _) = validate_url_target(m.as_str(), allowed_networks);
        if !ok {
            return true;
        }
    }
    false
}
