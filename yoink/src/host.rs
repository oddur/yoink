//! Where a host is and how ssh reaches it: the [`Host`] identity every
//! Docker and ssh call takes, and the `address[:port]` text form an
//! operator types. Pure: no I/O, no config.

/// Minimal host identity used by the trait. A `config::HostConfig`
/// converts into it cheaply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub user: String,
    pub address: String,
    /// SSH port from `hosts[].port`. `None` emits no port flag, so
    /// ssh picks (22, or `~/.ssh/config`). Never part of `address`,
    /// which stays the host's identity and display label.
    pub port: Option<u16>,
}

impl Host {
    /// Sentinel address that routes to the operator's local docker
    /// socket instead of going over ssh. Two callers:
    /// `transport::unregistry` (image-push source) and `doctor` (arch
    /// probe). Operators can also write `address: local` in
    /// `yoink.yaml` to deploy at their own laptop.
    pub const LOCAL_ADDRESS: &'static str = "local";

    /// `ssh://user@address[:port]`. An IPv6 address is bracketed when
    /// a port follows it, as the URL form requires.
    #[must_use]
    pub fn ssh_url(&self) -> String {
        format!(
            "ssh://{}@{}",
            self.user,
            join_address_port(&self.address, self.ssh_port())
        )
    }

    /// The port every ssh connection to this host must use. `None`
    /// when `port:` is unset, and always for the `local` sentinel.
    #[must_use]
    pub fn ssh_port(&self) -> Option<u16> {
        if self.is_local() { None } else { self.port }
    }

    /// The `ssh` argv tail that selects this host: `[-p <port>]
    /// user@address`. Every `ssh` shell-out ends its options with
    /// this instead of formatting `user@address` itself, so a host
    /// with `port:` is reached the same way everywhere. `ssh` takes
    /// `-p`; `scp` would need `-P` (yoink has no `scp` call today).
    #[must_use]
    pub fn ssh_args(&self) -> Vec<String> {
        let mut args = Vec::with_capacity(3);
        if let Some(port) = self.ssh_port() {
            args.push("-p".to_string());
            args.push(port.to_string());
        }
        args.push(format!("{}@{}", self.user, self.address));
        args
    }

    /// True for the local-docker sentinel host — bypasses ssh and
    /// uses bollard's platform-default unix socket / npipe transport.
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.address == Self::LOCAL_ADDRESS
    }

    /// Construct a `Host` that points at the operator's local docker
    /// socket. The empty `user` is fine — the local-socket transport
    /// ignores it.
    #[must_use]
    pub fn local() -> Self {
        Self {
            user: String::new(),
            address: Self::LOCAL_ADDRESS.to_string(),
            port: None,
        }
    }
}

/// Split an operator-typed `host[:port]` into the bare host and its
/// SSH port. A trailing `:digits` is a port only after a hostname or
/// an IPv4 address (exactly one colon), or after a bracketed IPv6
/// address (`[::1]:2200`). A bare IPv6 address has several colons and
/// no port. Brackets are dropped from the returned host because ssh
/// wants the bare address.
///
/// Errors when the port is not in 1-65535 or a bracket is unbalanced.
pub fn split_address_port(input: &str) -> Result<(String, Option<u16>), String> {
    fn parse_port(digits: &str) -> Result<u16, String> {
        match digits.parse::<u16>() {
            Ok(p) if p != 0 => Ok(p),
            _ => Err(format!("port {digits:?} must be a number in 1-65535")),
        }
    }
    let input = input.trim();
    if let Some(rest) = input.strip_prefix('[') {
        let Some((v6, tail)) = rest.split_once(']') else {
            return Err("`[` has no closing `]`".to_string());
        };
        if tail.is_empty() {
            return Ok((v6.to_string(), None));
        }
        let Some(digits) = tail.strip_prefix(':') else {
            return Err("expected `:port` after `]`".to_string());
        };
        return Ok((v6.to_string(), Some(parse_port(digits)?)));
    }
    match input.rsplit_once(':') {
        Some((host, digits))
            if !host.is_empty()
                && !host.contains(':')
                && !digits.is_empty()
                && digits.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Ok((host.to_string(), Some(parse_port(digits)?)))
        }
        _ => Ok((input.to_string(), None)),
    }
}

/// Inverse of [`split_address_port`]: `address[:port]`, with brackets
/// around an IPv6 address when a port follows it.
#[must_use]
pub fn join_address_port(address: &str, port: Option<u16>) -> String {
    match port {
        Some(port) if address.contains(':') => format!("[{address}]:{port}"),
        Some(port) => format!("{address}:{port}"),
        None => address.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host {
            user: "deploy".into(),
            address: "host-a".into(),
            port: None,
        }
    }

    #[test]
    fn host_ssh_url_format() {
        let h = host();
        assert_eq!(h.ssh_url(), "ssh://deploy@host-a");
    }

    fn host_on(address: &str, port: Option<u16>) -> Host {
        Host {
            user: "deploy".into(),
            address: address.into(),
            port,
        }
    }

    #[test]
    fn ssh_args_without_port_is_just_the_destination() {
        assert_eq!(host().ssh_args(), vec!["deploy@host-a"]);
        assert_eq!(host().ssh_port(), None);
    }

    #[test]
    fn ssh_args_with_port_puts_dash_p_before_the_destination() {
        let h = host_on("100.122.184.6", Some(2200));
        assert_eq!(h.ssh_args(), vec!["-p", "2200", "deploy@100.122.184.6"]);
        assert_eq!(h.ssh_port(), Some(2200));
    }

    #[test]
    fn ssh_url_with_port() {
        assert_eq!(
            host_on("100.122.184.6", Some(2200)).ssh_url(),
            "ssh://deploy@100.122.184.6:2200"
        );
        // IPv6 needs brackets once a port follows.
        assert_eq!(
            host_on("2001:db8::1", Some(2200)).ssh_url(),
            "ssh://deploy@[2001:db8::1]:2200"
        );
    }

    #[test]
    fn local_host_ignores_port() {
        let h = host_on(Host::LOCAL_ADDRESS, Some(2200));
        assert_eq!(h.ssh_port(), None);
        assert_eq!(h.ssh_args(), vec!["deploy@local"]);
        assert_eq!(h.ssh_url(), "ssh://deploy@local");
    }

    #[test]
    fn split_address_port_hostname_and_ipv4() {
        assert_eq!(
            split_address_port("prod-eu-1:2200"),
            Ok(("prod-eu-1".to_string(), Some(2200)))
        );
        assert_eq!(
            split_address_port("100.122.184.6:2200"),
            Ok(("100.122.184.6".to_string(), Some(2200)))
        );
    }

    #[test]
    fn split_address_port_without_port_is_unchanged() {
        for addr in ["prod-eu-1", "100.122.184.6", "local", "host.example.com"] {
            assert_eq!(split_address_port(addr), Ok((addr.to_string(), None)));
        }
    }

    #[test]
    fn split_address_port_bracketed_ipv6() {
        assert_eq!(
            split_address_port("[2001:db8::1]:2200"),
            Ok(("2001:db8::1".to_string(), Some(2200)))
        );
        // Brackets without a port: just the address.
        assert_eq!(
            split_address_port("[2001:db8::1]"),
            Ok(("2001:db8::1".to_string(), None))
        );
    }

    #[test]
    fn split_address_port_bare_ipv6_has_no_port() {
        // The trailing group is all digits, but with several colons
        // it is part of the address, not a port.
        for addr in [
            "2001:db8::1",
            "::1",
            "fe80::1:2200",
            "2001:db8:0:0:0:0:0:22",
        ] {
            assert_eq!(split_address_port(addr), Ok((addr.to_string(), None)));
        }
    }

    #[test]
    fn split_address_port_rejects_bad_ports_and_brackets() {
        assert!(split_address_port("host:0").is_err());
        assert!(split_address_port("host:65536").is_err());
        assert!(split_address_port("[::1]:").is_err());
        assert!(split_address_port("[::1]:ssh").is_err());
        assert!(split_address_port("[::1").is_err());
        assert!(split_address_port("[::1]2200").is_err());
        // Not all digits after the colon: not a port, left alone.
        assert_eq!(
            split_address_port("host:22a"),
            Ok(("host:22a".to_string(), None))
        );
    }
}
