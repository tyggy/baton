//! Pairing payload + terminal QR — mirrors the Mac app's `Pairing.swift`
//! (same JSON, same key order), so a phone pairs the same way from either.
//!
//! `{"v":1,"url":"https://<magicdns>","fallback":"http://<ts ipv4>:<port>","lan":"http://<lan ip>:<port>","token":"…"}`

use super::{find_tool, run};
use serde_json::Value;
use std::time::Duration;

/// Read-only Tailscale status (never changes the user's config).
#[derive(Clone, Debug, Default)]
pub struct Tailscale {
    pub cli: Option<String>,
    pub backend_state: Option<String>,
    pub dns_name: Option<String>,
    pub ipv4: Option<String>,
    pub cert_domains: Vec<String>,
    /// Local proxy targets from `tailscale serve status --json`.
    pub serve_targets: Vec<String>,
    pub error: Option<String>,
}

pub const TAILSCALE_CANDIDATES: [&str; 3] = [
    "/usr/local/bin/tailscale",
    "/opt/homebrew/bin/tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
];

impl Tailscale {
    pub fn running(&self) -> bool {
        self.backend_state.as_deref() == Some("Running")
    }

    pub fn serves_port(&self, port: u16) -> bool {
        let p = format!(":{port}");
        self.serve_targets
            .iter()
            .any(|t| t.trim_end_matches('/').ends_with(&p))
    }

    /// `tailscale status --json` and `tailscale serve status --json`.
    pub fn load() -> Tailscale {
        let mut t = Tailscale::default();
        let Some(cli) = find_tool("tailscale", &TAILSCALE_CANDIDATES) else {
            return t;
        };
        let cli = cli.to_string_lossy().into_owned();
        t.cli = Some(cli.clone());
        let st = run(&cli, &["status", "--json"], Duration::from_secs(8));
        let parsed: Option<Value> = serde_json::from_str(&st.stdout).ok();
        let Some(obj) = parsed.filter(|v| v.is_object()) else {
            t.error = Some(if st.stderr.trim().is_empty() {
                "tailscale status failed".into()
            } else {
                st.stderr.trim().to_string()
            });
            return t;
        };
        t.apply_status(&obj);
        let sv = run(&cli, &["serve", "status", "--json"], Duration::from_secs(8));
        if let Ok(v) = serde_json::from_str::<Value>(&sv.stdout) {
            proxy_targets(&v, &mut t.serve_targets);
        }
        t
    }

    fn apply_status(&mut self, obj: &Value) {
        self.backend_state = obj["BackendState"].as_str().map(String::from);
        self.cert_domains = obj["CertDomains"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let me = &obj["Self"];
        self.dns_name = me["DNSName"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_end_matches('.').to_string());
        self.ipv4 = me["TailscaleIPs"].as_array().and_then(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .find(|ip| !ip.contains(':'))
                .map(String::from)
        });
    }
}

/// Every `Proxy` value in the serve config, wherever it sits.
fn proxy_targets(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                match (k.as_str(), x) {
                    ("Proxy", Value::String(s)) => out.push(s.clone()),
                    _ => proxy_targets(x, out),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| proxy_targets(x, out)),
        _ => {}
    }
}

/// The addresses a phone should use (same rules as the Mac app):
/// `url` is the MagicDNS HTTPS name when the Mac is on a tailnet; the plain
/// HTTP addresses need the daemon to listen beyond loopback.
#[derive(Clone, Debug, PartialEq)]
pub struct Addresses {
    pub url: String,
    pub fallback: Option<String>,
    pub lan: Option<String>,
}

pub fn addresses(
    ts: &Tailscale,
    lan_ip: Option<&str>,
    port: u16,
    public: bool,
) -> Option<Addresses> {
    let lan = lan_ip
        .filter(|_| public)
        .map(|ip| format!("http://{ip}:{port}"));
    let ts_http = ts
        .ipv4
        .as_deref()
        .filter(|_| public)
        .map(|ip| format!("http://{ip}:{port}"));
    if let Some(dns) = &ts.dns_name {
        return Some(Addresses {
            url: format!("https://{dns}"),
            fallback: ts_http.or(lan.clone()),
            lan,
        });
    }
    let first = ts_http.clone().or(lan.clone())?;
    Some(Addresses {
        url: first,
        fallback: if ts_http.is_none() { None } else { lan.clone() },
        lan,
    })
}

fn enc(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

/// `{"v":1,"url":…,"fallback":…,"lan":…,"token":…}` — keys in this order.
pub fn payload(a: &Addresses, token: &str) -> String {
    let mut parts = vec!["\"v\":1".to_string(), format!("\"url\":{}", enc(&a.url))];
    if let Some(f) = &a.fallback {
        parts.push(format!("\"fallback\":{}", enc(f)));
    }
    if let Some(l) = &a.lan {
        parts.push(format!("\"lan\":{}", enc(l)));
    }
    parts.push(format!("\"token\":{}", enc(token)));
    format!("{{{}}}", parts.join(","))
}

/// `pairing.json` for `GET /api/pairing` (no token inside).
pub fn published(a: &Addresses, remote_access: bool) -> String {
    let mut parts = vec!["\"v\":1".to_string(), format!("\"url\":{}", enc(&a.url))];
    if let Some(f) = &a.fallback {
        parts.push(format!("\"fallback\":{}", enc(f)));
    }
    if let Some(l) = &a.lan {
        parts.push(format!("\"lan\":{}", enc(l)));
    }
    parts.push(format!("\"remote_access\":{remote_access}"));
    parts.push(format!(
        "\"updated\":{}",
        enc(&chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%S%:z")
            .to_string())
    ));
    format!("{{{}}}\n", parts.join(","))
}

pub fn pairing_file() -> std::path::PathBuf {
    match std::env::var_os("BATON_PAIRING_FILE") {
        Some(p) if !p.is_empty() => std::path::PathBuf::from(p),
        _ => super::super::config::home().join("Library/Application Support/Baton/pairing.json"),
    }
}

/// The Mac's LAN IPv4 (an `en*` interface, `en0` preferred).
pub fn lan_ipv4() -> Option<String> {
    let mut found: Option<String> = None;
    // SAFETY: getifaddrs/freeifaddrs pair; every pointer is checked before use.
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return None;
        }
        let mut p = ifap;
        while !p.is_null() {
            let ifa = &*p;
            p = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || i32::from((*ifa.ifa_addr).sa_family) != libc::AF_INET {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy();
            if !name.starts_with("en") {
                continue;
            }
            let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            if found.is_none() || name == "en0" {
                found = Some(ip.to_string());
            }
        }
        libc::freeifaddrs(ifap);
    }
    found
}

/// A QR code as terminal text: two modules per character cell, drawn in
/// explicit white-on-black so it scans on light and dark terminals alike.
pub fn qr_text(data: &str, color: bool) -> Option<String> {
    let code = qrcode::QrCode::with_error_correction_level(data, qrcode::EcLevel::M).ok()?;
    let w = code.width();
    let colors = code.to_colors();
    let quiet = 2i64;
    let dark = |x: i64, y: i64| -> bool {
        if x < 0 || y < 0 || x >= w as i64 || y >= w as i64 {
            return false;
        }
        colors[y as usize * w + x as usize] == qrcode::Color::Dark
    };
    let mut out = String::new();
    let mut y = -quiet;
    while y < w as i64 + quiet {
        if color {
            out.push_str("\x1b[97;40m");
        }
        for x in -quiet..w as i64 + quiet {
            // Light modules are drawn (white); dark ones are the background.
            out.push(match (dark(x, y), dark(x, y + 1)) {
                (false, false) => '█',
                (false, true) => '▀',
                (true, false) => '▄',
                (true, true) => ' ',
            });
        }
        if color {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
        y += 2;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(dns: Option<&str>, ip: Option<&str>) -> Tailscale {
        Tailscale {
            dns_name: dns.map(String::from),
            ipv4: ip.map(String::from),
            ..Tailscale::default()
        }
    }

    #[test]
    fn addresses_match_the_mac_app() {
        let a = addresses(
            &ts(Some("mac.tail.ts.net"), Some("100.1.2.3")),
            Some("192.168.1.5"),
            19877,
            true,
        )
        .unwrap();
        assert_eq!(a.url, "https://mac.tail.ts.net");
        assert_eq!(a.fallback.as_deref(), Some("http://100.1.2.3:19877"));
        assert_eq!(a.lan.as_deref(), Some("http://192.168.1.5:19877"));
        assert_eq!(
            payload(&a, "tok"),
            r#"{"v":1,"url":"https://mac.tail.ts.net","fallback":"http://100.1.2.3:19877","lan":"http://192.168.1.5:19877","token":"tok"}"#
        );
        // No tailnet: LAN only.
        let a = addresses(&ts(None, None), Some("10.0.0.2"), 1234, true).unwrap();
        assert_eq!(
            (a.url.as_str(), a.fallback.as_deref()),
            ("http://10.0.0.2:1234", None)
        );
        // Loopback-only daemon: only the HTTPS (tailscale serve) name.
        let a = addresses(
            &ts(Some("m.ts.net"), Some("100.1.1.1")),
            Some("10.0.0.2"),
            1,
            false,
        )
        .unwrap();
        assert_eq!((a.fallback, a.lan), (None, None));
        assert!(addresses(&ts(None, None), Some("10.0.0.2"), 1, false).is_none());
    }

    #[test]
    fn status_parsing() {
        let mut t = Tailscale::default();
        t.apply_status(&serde_json::json!({
            "BackendState": "Running",
            "Self": {"DNSName": "mac.tail.ts.net.", "TailscaleIPs": ["fd7a::1", "100.1.2.3"]}
        }));
        assert!(t.running());
        assert_eq!(t.dns_name.as_deref(), Some("mac.tail.ts.net"));
        assert_eq!(t.ipv4.as_deref(), Some("100.1.2.3"));
        let mut out = vec![];
        proxy_targets(
            &serde_json::json!({"Web": {"m:443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:19877"}}}}}),
            &mut out,
        );
        t.serve_targets = out;
        assert!(t.serves_port(19877) && !t.serves_port(1987));
    }

    #[test]
    fn qr_renders() {
        let q = qr_text("{\"v\":1}", false).unwrap();
        let lines: Vec<&str> = q.lines().collect();
        assert!(lines.len() > 10);
        assert!(lines
            .iter()
            .all(|l| l.chars().count() == lines[0].chars().count()));
    }
}
