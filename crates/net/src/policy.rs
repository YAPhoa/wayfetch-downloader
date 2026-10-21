//! Which destinations requests may reach.
//!
//! Every URL and redirect target is checked before connecting, and
//! [`PolicyResolver`] hands the connector only permitted addresses, so no
//! second resolution can reach a loopback, private, or metadata address.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, LazyLock};

use ipnet::IpNet;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

/// Longest URL a request may name, redirect targets included.
pub const MAX_URL_BYTES: usize = 8 * 1024;

/// Longest host name DNS can carry.
const MAX_HOST_BYTES: usize = 253;

/// Networks that are not publicly routable, or that address the local host,
/// a local network, or a cloud metadata service. IPv4-mapped IPv6
/// addresses are checked as IPv4; NAT64 and 6to4 addresses by the IPv4
/// address they embed.
static NON_PUBLIC: LazyLock<Vec<IpNet>> = LazyLock::new(|| {
    [
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.0.0.0/24",
        "192.0.2.0/24",
        "192.88.99.0/24",
        "192.168.0.0/16",
        "198.18.0.0/15",
        "198.51.100.0/24",
        "203.0.113.0/24",
        "224.0.0.0/4",
        "240.0.0.0/4",
        "::/128",
        "::1/128",
        "64:ff9b:1::/48",
        "100::/64",
        "2001::/23",
        "2001:db8::/32",
        "fc00::/7",
        "fe80::/10",
        "fec0::/10",
        "ff00::/8",
    ]
    .iter()
    .map(|net| net.parse().expect("a valid network"))
    .collect()
});

/// Which destinations requests may reach.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressPolicy {
    /// Networks an administrator allows although they are not public, for
    /// intentional downloads from a local server. Empty by default: only
    /// public addresses are reachable.
    pub allowed_networks: Vec<IpNet>,
}

/// The policy refused a destination.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Refused(pub(crate) String);

impl EgressPolicy {
    /// Whether requests may connect to `ip`.
    pub fn permits(&self, ip: IpAddr) -> bool {
        // An IPv4-mapped IPv6 address leads to its IPv4 address.
        let ip = ip.to_canonical();
        is_public(ip) || self.allowed_networks.iter().any(|net| net.contains(&ip))
    }

    /// Checks a URL before any connection to it: scheme, embedded
    /// credentials, length, host, port, and a literal address.
    pub(crate) fn check(&self, url: &Url) -> Result<(), Refused> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Refused(format!("scheme {} is not allowed", url.scheme())));
        }
        if url.as_str().len() > MAX_URL_BYTES {
            return Err(Refused(format!("URL longer than {MAX_URL_BYTES} bytes")));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Refused("URL embeds credentials".into()));
        }
        if url.port() == Some(0) {
            return Err(Refused("port 0".into()));
        }
        match url.host() {
            None => Err(Refused("URL has no host".into())),
            Some(Host::Domain(name)) if name.len() > MAX_HOST_BYTES => {
                Err(Refused(format!("host longer than {MAX_HOST_BYTES} bytes")))
            }
            Some(Host::Domain(_)) => Ok(()),
            Some(Host::Ipv4(ip)) => self.check_ip(ip.into()),
            Some(Host::Ipv6(ip)) => self.check_ip(ip.into()),
        }
    }

    fn check_ip(&self, ip: IpAddr) -> Result<(), Refused> {
        if self.permits(ip) {
            Ok(())
        } else {
            Err(Refused(format!("destination {ip} is not allowed")))
        }
    }
}

fn is_public(ip: IpAddr) -> bool {
    if NON_PUBLIC.iter().any(|net| net.contains(&ip)) {
        return false;
    }
    match ip {
        IpAddr::V4(_) => true,
        IpAddr::V6(v6) => embedded_v4(v6).is_none_or(|v4| is_public(v4.into())),
    }
}

/// The IPv4 address a NAT64 (`64:ff9b::/96`) or 6to4 (`2002::/16`) address
/// embeds, which is where it actually leads.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let o = ip.octets();
    match ip.segments() {
        [0x64, 0xff9b, 0, 0, 0, 0, _, _] => Some(Ipv4Addr::new(o[12], o[13], o[14], o[15])),
        [0x2002, ..] => Some(Ipv4Addr::new(o[2], o[3], o[4], o[5])),
        _ => None,
    }
}

/// Resolves host names for the connector and keeps only the addresses the
/// policy permits, so the connection cannot reach any other.
pub(crate) struct PolicyResolver(pub(crate) Arc<EgressPolicy>);

impl Resolve for PolicyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = self.0.clone();
        Box::pin(async move {
            let host = name.as_str();
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host, 0)).await?.collect();
            let permitted: Vec<SocketAddr> = found
                .iter()
                .copied()
                .filter(|addr| policy.permits(addr.ip()))
                .collect();
            if permitted.is_empty() {
                let reason = if found.is_empty() {
                    format!("{host} has no address")
                } else {
                    format!("{host} resolves only to addresses that are not allowed")
                };
                return Err(Refused(reason).into());
            }
            Ok(Box::new(permitted.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn only_public_addresses_are_reachable_by_default() {
        let policy = EgressPolicy::default();
        for denied in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
            "2001:db8::1",
        ] {
            assert!(!policy.permits(ip(denied)), "{denied}");
        }
        for allowed in ["93.184.215.14", "2606:4700::1", "64:ff9b::5db8:d70e"] {
            assert!(policy.permits(ip(allowed)), "{allowed}");
        }
    }

    #[test]
    fn an_allowlist_opens_named_networks_only() {
        let policy = EgressPolicy {
            allowed_networks: vec!["127.0.0.1/32".parse().unwrap()],
        };
        assert!(policy.permits(ip("127.0.0.1")));
        assert!(policy.permits(ip("::ffff:127.0.0.1")));
        assert!(!policy.permits(ip("127.0.0.2")));
        assert!(!policy.permits(ip("10.0.0.1")));
    }

    #[test]
    fn urls_are_checked_before_connecting() {
        let policy = EgressPolicy::default();
        let check = |s: &str| policy.check(&Url::parse(s).unwrap());
        assert!(check("https://files.example/a.bin").is_ok());
        assert!(check("http://files.example:8080/a.bin").is_ok());
        for refused in [
            "ftp://files.example/a.bin",
            "file:///etc/passwd",
            "https://user:secret@files.example/a.bin",
            "https://user@files.example/a.bin",
            "https://127.0.0.1/a.bin",
            // Alternative spellings of loopback normalize to it.
            "https://2130706433/a.bin",
            "https://0x7f.1/a.bin",
            "https://[::ffff:7f00:1]/a.bin",
            "https://files.example:0/a.bin",
        ] {
            assert!(check(refused).is_err(), "{refused}");
        }
        let long = format!("https://files.example/{}", "a".repeat(MAX_URL_BYTES));
        assert!(check(&long).is_err());
        let host = format!("https://{}.example/", "a".repeat(250));
        assert!(check(&host).is_err());
    }
}
