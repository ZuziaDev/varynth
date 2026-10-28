use std::env;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub jwt_secret: String,
    pub web_origin: String,
    pub public_url: String,
    pub bind: String,
    pub workers_account_id: Option<String>,
    pub workers_token: Option<String>,
    pub openai_key: Option<String>,
    pub anthropic_key: Option<String>,
    pub openrouter_key: Option<String>,
    pub worker_gateways: Vec<String>,
    pub worker_gateway_key: Option<String>,
    pub parallel_keys: Vec<String>,
    pub exa_keys: Vec<String>,
    pub firecrawl_keys: Vec<String>,
    pub pro_emails: Vec<String>,
    /// Proxies whose `X-Forwarded-For` header is trusted. When empty, the
    /// header is ignored and the socket peer address identifies the caller.
    pub trusted_proxies: Vec<IpCidr>,
    pub stub_upstream: bool,
    pub enable_public_placeholders: bool,
    pub billing_stub: bool,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let jwt_secret = required("JWT_SECRET")?;
        if jwt_secret.len() < 32 {
            return Err("JWT_SECRET must be at least 32 bytes".into());
        }
        Ok(Self {
            database_url: required("DATABASE_URL")?,
            jwt_secret,
            web_origin: env_or("WEB_ORIGIN", "http://127.0.0.1:3000"),
            public_url: env_or("VARYNTH_API_PUBLIC_URL", "http://127.0.0.1:8080"),
            bind: env_or("BIND_ADDR", "127.0.0.1:8080"),
            workers_account_id: nonempty("WORKERS_AI_ACCOUNT_ID"),
            workers_token: nonempty("WORKERS_AI_API_TOKEN"),
            openai_key: nonempty("OPENAI_API_KEY"),
            anthropic_key: nonempty("ANTHROPIC_API_KEY"),
            openrouter_key: nonempty("OPENROUTER_API_KEY"),
            worker_gateways: gateways(),
            worker_gateway_key: nonempty("WORKER_GATEWAY_KEY"),
            parallel_keys: numbered("PARALLEL_API_KEY"),
            exa_keys: numbered("EXA_API_KEY"),
            firecrawl_keys: numbered("FIRECRAWL_API_KEY"),
            pro_emails: split_list("VARYNTH_PRO_EMAILS"),
            trusted_proxies: trusted_proxies(),
            stub_upstream: flag("VARYNTH_STUB_UPSTREAM"),
            enable_public_placeholders: flag("VARYNTH_ENABLE_PUBLIC_PLACEHOLDERS"),
            billing_stub: flag("VARYNTH_BILLING_STUB"),
        })
    }

    pub fn upstream_key(&self, kind: &str) -> Option<String> {
        match kind {
            "workers_ai" => self.workers_token.clone(),
            "worker_gateway" => self.worker_gateway_key.clone(),
            "openai" => self.openai_key.clone(),
            "anthropic" => self.anthropic_key.clone(),
            "openrouter" => self.openrouter_key.clone(),
            _ => None,
        }
    }

    /// Cookies carry `Secure` (and the `__Host-` prefix) only when the public
    /// URL is HTTPS, so plain-http local development keeps working.
    pub fn secure_cookies(&self) -> bool {
        self.public_url.trim().to_ascii_lowercase().starts_with("https://")
    }

    pub fn is_trusted_proxy(&self, ip: IpAddr) -> bool {
        let ip = canonical_ip(ip);
        self.trusted_proxies.iter().any(|net| net.contains(ip))
    }
}

/// Normalizes IPv4-mapped IPv6 addresses (`::ffff:1.2.3.4`) to IPv4 so a
/// caller cannot obtain a second quota bucket via a different socket family.
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        other => other,
    }
}

/// A single IP address or CIDR block, used for `VARYNTH_TRUSTED_PROXIES`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpCidr {
    addr: IpAddr,
    prefix: u8,
}

impl IpCidr {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (ip_text, prefix_text) = match text.split_once('/') {
            Some((ip, prefix)) => (ip, Some(prefix)),
            None => (text, None),
        };
        let addr = canonical_ip(ip_text.parse::<IpAddr>().ok()?);
        let max = match addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        let prefix = match prefix_text {
            Some(prefix) => {
                let value: u8 = prefix.trim().parse().ok()?;
                if value > max {
                    return None;
                }
                value
            }
            None => max,
        };
        Some(Self { addr, prefix })
    }

    pub fn addr(&self) -> IpAddr {
        self.addr
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical_ip(ip);
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                if self.prefix == 0 {
                    return true;
                }
                let shift = 32 - u32::from(self.prefix);
                v4_bits(net) >> shift == v4_bits(ip) >> shift
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                if self.prefix == 0 {
                    return true;
                }
                let shift = 128 - u32::from(self.prefix);
                v6_bits(net) >> shift == v6_bits(ip) >> shift
            }
            _ => false,
        }
    }
}

fn v4_bits(addr: Ipv4Addr) -> u32 {
    u32::from(addr)
}

fn v6_bits(addr: Ipv6Addr) -> u128 {
    u128::from(addr)
}

fn required(name: &str) -> Result<String, String> {
    match nonempty(name) {
        Some(value) => Ok(value),
        None => Err(format!("{name} is required")),
    }
}

fn nonempty(name: &str) -> Option<String> {
    env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn env_or(name: &str, default: &str) -> String {
    nonempty(name).unwrap_or_else(|| default.to_string())
}
fn flag(name: &str) -> bool {
    matches!(
        env::var(name).ok().as_deref().map(str::trim),
        Some("1") | Some("true") | Some("TRUE") | Some("yes")
    )
}

fn numbered(prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(value) = nonempty(prefix) {
        out.push(value);
    }
    for index in 2..=8 {
        if let Some(value) = nonempty(&format!("{prefix}_{index}")) {
            out.push(value);
        }
    }
    out
}

fn split_list(name: &str) -> Vec<String> {
    nonempty(name)
        .map(|value| {
            value
                .split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn trusted_proxies() -> Vec<IpCidr> {
    split_list("VARYNTH_TRUSTED_PROXIES")
        .iter()
        .filter_map(|item| IpCidr::parse(item))
        .collect()
}

fn gateways() -> Vec<String> {
    let configured = split_list("WORKER_GATEWAYS");
    if !configured.is_empty() {
        return configured;
    }
    [
        "https://api.zuziarodzen02.workers.dev/v1",
        "https://api.ridvankivanc1.workers.dev/v1",
        "https://api.rickcodeshare.workers.dev/v1",
        "https://api.ridvandev02.workers.dev/v1",
        "https://api.ridvankivanc4.workers.dev/v1",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_parses_addresses_and_blocks() {
        let single = IpCidr::parse("10.1.2.3").expect("single");
        assert_eq!(single.prefix(), 32);
        assert!(single.contains("10.1.2.3".parse().unwrap()));
        assert!(!single.contains("10.1.2.4".parse().unwrap()));

        let block = IpCidr::parse("10.0.0.0/8").expect("block");
        assert!(block.contains("10.255.1.1".parse().unwrap()));
        assert!(!block.contains("11.0.0.1".parse().unwrap()));

        let mapped = IpCidr::parse("10.0.0.0/8").expect("block");
        assert!(mapped.contains("::ffff:10.1.1.1".parse().unwrap()));

        assert!(IpCidr::parse("10.0.0.0/33").is_none());
        assert!(IpCidr::parse("not-an-ip").is_none());
        assert!(IpCidr::parse("fe80::/129").is_none());
    }

    #[test]
    fn mapped_v6_normalizes_to_v4() {
        assert_eq!(
            canonical_ip("::ffff:192.0.2.5".parse().unwrap()),
            "192.0.2.5".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn https_public_url_marks_cookies_secure() {
        let mut config = Config {
            database_url: String::new(),
            jwt_secret: String::new(),
            web_origin: String::new(),
            public_url: "HTTPS://api.varynth.dev".into(),
            bind: String::new(),
            workers_account_id: None,
            workers_token: None,
            openai_key: None,
            anthropic_key: None,
            openrouter_key: None,
            worker_gateways: Vec::new(),
            worker_gateway_key: None,
            parallel_keys: Vec::new(),
            exa_keys: Vec::new(),
            firecrawl_keys: Vec::new(),
            pro_emails: Vec::new(),
            trusted_proxies: Vec::new(),
            stub_upstream: false,
            enable_public_placeholders: false,
            billing_stub: false,
        };
        assert!(config.secure_cookies());
        config.public_url = "http://127.0.0.1:8080".into();
        assert!(!config.secure_cookies());
    }
}
