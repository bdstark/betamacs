//! Local DNS filter for betamacsd (docs/site-filter.md).
//!
//! pf can only filter by IP, which is useless against CDN-hosted sites
//! (a "khanacademy.org" allowlist resolved to its apex A record misses
//! cdn.kastatic.org, kasandbox.org, the video hosts, …). So the daemon runs
//! a tiny DNS forwarder on loopback, points the system resolver at it, and
//! decides per *name*:
//!
//!   - **Allow mode** (earning-mode lockout): only names under the allowlist
//!     are forwarded upstream; every IP in an answer is added to a pf table
//!     the earning ruleset passes 80/443 to. Everything else is NXDOMAIN.
//!   - **Block mode** (balance available, blocklist configured): names under
//!     the blocklist are NXDOMAIN; the rest are forwarded untouched.
//!   - **Audit mode**: forward everything, log every name — discovery for
//!     what a site or app actually needs before allowlisting it.
//!
//! Public DoH provider names (and Firefox's DoH canary) are denied in every
//! enforcing mode so a browser cannot resolve around the filter; pf blocks
//! the well-known DoH resolver IPs and any port 53/853 that isn't ours.
//!
//! No DNS crate: the three things we need — the question name, the A/AAAA
//! records in an answer, and an NXDOMAIN reply — are a few dozen lines.
//! Everything else is forwarded byte-for-byte.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// What the forwarder does with each query.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Not engaged: forward everything, record nothing.
    Off,
    /// Forward everything, record every name (discovery).
    Audit,
    /// Deny `block`, forward the rest.
    Block { block: Vec<String> },
    /// Forward only `allow` (minus `block`), learn answer IPs into pf, deny
    /// the rest.
    Allow { allow: Vec<String>, block: Vec<String> },
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Audit => "audit",
            Mode::Block { .. } => "block",
            Mode::Allow { .. } => "allow",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Forward,
    /// Forward, and add the answer's IPs to the pf allow table.
    ForwardLearn,
    Deny,
}

/// Firefox disables its built-in DoH when this name returns NXDOMAIN.
const FIREFOX_CANARY: &str = "use-application-dns.net";

/// Public DoH/DoT provider names: never resolvable while enforcing, so a
/// browser's "secure DNS" cannot bootstrap around the filter.
const DOH_NAMES: [&str; 10] = [
    "dns.google",
    "cloudflare-dns.com",
    "one.one.one.one",
    "dns.quad9.net",
    "doh.opendns.com",
    "dns.adguard.com",
    "dns.adguard-dns.com",
    "dns.nextdns.io",
    "doh.dns.sb",
    "dns.controld.com",
];

/// Well-known public resolver IPs that also speak DoH on 443; the pf
/// dns-lock rules drop 443 to these so an IP-literal DoH template fails.
pub const DOH_IPS: [&str; 12] = [
    "1.1.1.1",
    "1.0.0.1",
    "8.8.8.8",
    "8.8.4.4",
    "9.9.9.9",
    "149.112.112.112",
    "208.67.222.222",
    "208.67.220.220",
    "94.140.14.14",
    "94.140.15.15",
    "76.76.2.0/24",
    "45.90.28.0/22",
];

/// Suffix match: `host` equals an entry or ends with `.entry`. Entries may be
/// written as `example.com`, `.example.com` or `*.example.com`; all mean the
/// domain and everything under it. Case-insensitive; trailing dots ignored.
pub fn matches(host: &str, list: &[String]) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    list.iter().any(|s| {
        let s = s
            .trim()
            .trim_start_matches("*.")
            .trim_start_matches('.')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        !s.is_empty() && (h == s || h.ends_with(&format!(".{s}")))
    })
}

/// The per-name decision. Pure — unit-tested.
pub fn decide(mode: &Mode, name: &str) -> Verdict {
    let n = name.trim_end_matches('.').to_ascii_lowercase();
    if *mode == Mode::Off {
        return Verdict::Forward;
    }
    if n == FIREFOX_CANARY {
        return Verdict::Deny;
    }
    let doh = DOH_NAMES.iter().any(|d| n == *d || n.ends_with(&format!(".{d}")));
    match mode {
        Mode::Off => Verdict::Forward,
        Mode::Audit => Verdict::Forward,
        Mode::Block { block } => {
            if doh || matches(&n, block) {
                Verdict::Deny
            } else {
                Verdict::Forward
            }
        }
        Mode::Allow { allow, block } => {
            if doh || matches(&n, block) {
                Verdict::Deny
            } else if matches(&n, allow) {
                Verdict::ForwardLearn
            } else {
                Verdict::Deny
            }
        }
    }
}

// ------------------------------------------------------------ wire format

/// Read a (possibly compressed) name at `pos`. Returns the name and the
/// offset just past it in the *original* stream.
fn read_name(buf: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut end: Option<usize> = None;
    let mut hops = 0;
    loop {
        let len = *buf.get(pos)? as usize;
        if len & 0xC0 == 0xC0 {
            let ptr = ((len & 0x3F) << 8) | *buf.get(pos + 1)? as usize;
            if end.is_none() {
                end = Some(pos + 2);
            }
            hops += 1;
            if hops > 16 || ptr >= pos {
                return None; // loop / forward pointer
            }
            pos = ptr;
            continue;
        }
        pos += 1;
        if len == 0 {
            break;
        }
        let label = buf.get(pos..pos + len)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        pos += len;
    }
    Some((labels.join("."), end.unwrap_or(pos)))
}

/// The first question: (name, qtype, offset past the question section).
pub fn question(buf: &[u8]) -> Option<(String, u16, usize)> {
    if buf.len() < 12 {
        return None;
    }
    let qd = u16::from_be_bytes([buf[4], buf[5]]);
    if qd == 0 {
        return None;
    }
    let (name, mut pos) = read_name(buf, 12)?;
    let qtype = u16::from_be_bytes([*buf.get(pos)?, *buf.get(pos + 1)?]);
    pos += 4; // qtype + qclass
    // Skip any further questions (QDCOUNT > 1 is a curiosity; be safe).
    for _ in 1..qd {
        let (_, p) = read_name(buf, pos)?;
        pos = p + 4;
    }
    Some((name, qtype, pos))
}

/// Every A/AAAA address in the answer section of a response.
pub fn answer_ips(buf: &[u8]) -> Vec<IpAddr> {
    let mut out = Vec::new();
    let Some((_, _, mut pos)) = question(buf) else { return out };
    let an = u16::from_be_bytes([buf[6], buf[7]]) as usize;
    for _ in 0..an {
        let Some((_, p)) = read_name(buf, pos) else { break };
        let Some(hdr) = buf.get(p..p + 10) else { break };
        let rtype = u16::from_be_bytes([hdr[0], hdr[1]]);
        let rdlen = u16::from_be_bytes([hdr[8], hdr[9]]) as usize;
        let rd_start = p + 10;
        let Some(rdata) = buf.get(rd_start..rd_start + rdlen) else { break };
        match (rtype, rdlen) {
            (1, 4) => out.push(IpAddr::V4(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]))),
            (28, 16) => {
                let mut o = [0u8; 16];
                o.copy_from_slice(rdata);
                out.push(IpAddr::V6(Ipv6Addr::from(o)));
            }
            _ => {}
        }
        pos = rd_start + rdlen;
    }
    out
}

/// An NXDOMAIN reply to `query`: same id/opcode/RD, QR+RA set, RCODE 3, the
/// question echoed, nothing else (EDNS/additional dropped).
pub fn nxdomain(query: &[u8], qend: usize) -> Vec<u8> {
    let mut r = Vec::with_capacity(qend);
    r.extend_from_slice(&query[..2]);
    r.push(0x80 | (query[2] & 0x79)); // QR=1, keep opcode + RD, clear AA/TC
    r.push(0x80 | 0x03); // RA=1, RCODE=NXDOMAIN
    r.extend_from_slice(&query[4..6]); // QDCOUNT as sent
    r.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // AN/NS/AR = 0
    r.extend_from_slice(&query[12..qend.min(query.len())]);
    r
}

// --------------------------------------------------------------- the filter

/// Callback the daemon supplies to add learned IPs to the pf allow table
/// (dry-run aware on its side).
pub type PfHook = Arc<dyn Fn(&[IpAddr]) + Send + Sync>;

#[derive(Clone, Debug)]
struct Event {
    name: String,
    denied: bool,
}

struct Inner {
    mode: RwLock<Mode>,
    upstreams: RwLock<Vec<SocketAddr>>,
    /// Recent decisions, newest last, deduplicated by name (for `status`).
    recent: Mutex<VecDeque<Event>>,
    /// Names written to the audit log and when (rate-limits the log).
    logged: Mutex<HashMap<String, Instant>>,
    /// IPs already handed to pf this allow-session.
    learned: Mutex<HashSet<IpAddr>>,
    pf: PfHook,
    audit_path: PathBuf,
    port: u16,
    running: Mutex<bool>,
}

#[derive(Clone)]
pub struct Filter {
    inner: Arc<Inner>,
}

const RECENT_CAP: usize = 200;
const UPSTREAM_TIMEOUT: Duration = Duration::from_millis(2500);
const AUDIT_REPEAT: Duration = Duration::from_secs(300);

impl Filter {
    /// `port` is 53 in production; `BETAMACSD_DNS_PORT` overrides for tests.
    /// The listener starts lazily on the first non-Off mode.
    pub fn new(managed_dir: &Path, pf: PfHook) -> Self {
        let port = std::env::var("BETAMACSD_DNS_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(53);
        Self {
            inner: Arc::new(Inner {
                mode: RwLock::new(Mode::Off),
                upstreams: RwLock::new(Vec::new()),
                recent: Mutex::new(VecDeque::new()),
                logged: Mutex::new(HashMap::new()),
                learned: Mutex::new(HashSet::new()),
                pf,
                audit_path: managed_dir.join("site-audit.log"),
                port,
                running: Mutex::new(false),
            }),
        }
    }

    /// Switch modes; starts the listener when leaving Off. Returns false if
    /// the listener could not be started (port busy / not root).
    pub fn set_mode(&self, mode: Mode) -> bool {
        let changed = {
            let mut m = self.inner.mode.write().unwrap();
            let changed = *m != mode;
            *m = mode.clone();
            changed
        };
        if changed {
            tracing::info!("dns filter mode → {}", mode.as_str());
        }
        if mode != Mode::Off { self.ensure_running() } else { true }
    }

    pub fn set_upstreams(&self, ups: Vec<SocketAddr>) {
        let mut u = self.inner.upstreams.write().unwrap();
        if *u != ups {
            tracing::info!("dns filter upstreams → {:?}", ups);
            *u = ups;
        }
    }

    pub fn upstreams(&self) -> Vec<SocketAddr> {
        self.inner.upstreams.read().unwrap().clone()
    }

    /// Forget which IPs were handed to pf (call whenever the anchor's table
    /// is (re)created so they are re-added on the next lookup).
    pub fn reset_learned(&self) {
        self.inner.learned.lock().unwrap().clear();
    }

    /// Recent unique names, newest first: (denied, forwarded).
    pub fn recent(&self, limit: usize) -> (Vec<String>, Vec<String>) {
        let r = self.inner.recent.lock().unwrap();
        let mut denied = Vec::new();
        let mut forwarded = Vec::new();
        for e in r.iter().rev() {
            let v = if e.denied { &mut denied } else { &mut forwarded };
            if v.len() < limit && !v.contains(&e.name) {
                v.push(e.name.clone());
            }
        }
        (denied, forwarded)
    }

    fn ensure_running(&self) -> bool {
        let mut running = self.inner.running.lock().unwrap();
        if *running {
            return true;
        }
        let addr = SocketAddr::from(([127, 0, 0, 1], self.inner.port));
        let udp = match UdpSocket::bind(addr) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::error!("dns filter: cannot bind udp {addr}: {e}");
                return false;
            }
        };
        let tcp = match TcpListener::bind(addr) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("dns filter: cannot bind tcp {addr}: {e}");
                return false;
            }
        };
        for _ in 0..4 {
            let udp = udp.clone();
            let inner = self.inner.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    let Ok((n, from)) = udp.recv_from(&mut buf) else { continue };
                    if let Some(reply) = handle(&inner, &buf[..n], false) {
                        let _ = udp.send_to(&reply, from);
                    }
                }
            });
        }
        {
            let inner = self.inner.clone();
            std::thread::spawn(move || {
                for stream in tcp.incoming().flatten() {
                    let inner = inner.clone();
                    std::thread::spawn(move || serve_tcp(&inner, stream));
                }
            });
        }
        *running = true;
        tracing::info!("dns filter listening on {addr}");
        true
    }
}

fn serve_tcp(inner: &Inner, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    loop {
        let mut len = [0u8; 2];
        if stream.read_exact(&mut len).is_err() {
            return;
        }
        let n = u16::from_be_bytes(len) as usize;
        let mut q = vec![0u8; n];
        if stream.read_exact(&mut q).is_err() {
            return;
        }
        let Some(reply) = handle(inner, &q, true) else { return };
        let l = (reply.len() as u16).to_be_bytes();
        if stream.write_all(&l).is_err() || stream.write_all(&reply).is_err() {
            return;
        }
    }
}

fn record(inner: &Inner, name: &str, denied: bool, mode: &Mode) {
    let now = Instant::now();
    {
        let mut r = inner.recent.lock().unwrap();
        // Collapse a repeat of the most recent entry for this name.
        if let Some(pos) = r.iter().rposition(|e| e.name == name && e.denied == denied) {
            r.remove(pos);
        }
        r.push_back(Event { name: name.to_string(), denied });
        while r.len() > RECENT_CAP {
            r.pop_front();
        }
    }
    // Audit log: one line per name per 5 minutes, so a chatty app doesn't
    // fill the disk but a parent reading the file sees every distinct host.
    let should_log = {
        let mut l = inner.logged.lock().unwrap();
        if l.len() > 4000 {
            l.retain(|_, t| t.elapsed() < AUDIT_REPEAT);
        }
        let key = format!("{}|{}", denied as u8, name);
        match l.get(&key) {
            Some(t) if t.elapsed() < AUDIT_REPEAT => false,
            _ => {
                l.insert(key, now);
                true
            }
        }
    };
    if should_log {
        let stamp = std::process::Command::new("/bin/date")
            .args(["+%FT%T"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let line = format!(
            "{stamp} {} {} {name}\n",
            mode.as_str(),
            if denied { "DENY " } else { "allow" },
        );
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&inner.audit_path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// Decide and (if forwarding) resolve one query. `None` = drop it.
fn handle(inner: &Inner, query: &[u8], tcp: bool) -> Option<Vec<u8>> {
    let (name, _qtype, qend) = question(query)?;
    let mode = inner.mode.read().unwrap().clone();
    let verdict = decide(&mode, &name);
    if mode != Mode::Off {
        record(inner, &name, verdict == Verdict::Deny, &mode);
    }
    if verdict == Verdict::Deny {
        return Some(nxdomain(query, qend));
    }
    let ups = inner.upstreams.read().unwrap().clone();
    let reply = forward(&ups, query, tcp)?;
    if verdict == Verdict::ForwardLearn {
        let ips = answer_ips(&reply);
        let fresh: Vec<IpAddr> = {
            let mut l = inner.learned.lock().unwrap();
            ips.into_iter().filter(|ip| l.insert(*ip)).collect()
        };
        if !fresh.is_empty() {
            // Add to pf BEFORE answering: the client connects the instant it
            // has the address.
            (inner.pf)(&fresh);
        }
    }
    Some(reply)
}

/// Forward a raw query to the first upstream that answers.
fn forward(upstreams: &[SocketAddr], query: &[u8], tcp: bool) -> Option<Vec<u8>> {
    for up in upstreams {
        if tcp {
            if let Some(r) = forward_tcp(*up, query) {
                return Some(r);
            }
            continue;
        }
        let Ok(sock) = UdpSocket::bind(if up.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }) else {
            continue;
        };
        let _ = sock.set_read_timeout(Some(UPSTREAM_TIMEOUT));
        if sock.send_to(query, up).is_err() {
            continue;
        }
        let mut buf = vec![0u8; 4096];
        // Match the transaction id so a stale packet can't be mis-delivered.
        for _ in 0..3 {
            match sock.recv_from(&mut buf) {
                Ok((n, from)) if from.ip() == up.ip() && n >= 2 && buf[..2] == query[..2] => {
                    buf.truncate(n);
                    return Some(buf);
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
    }
    tracing::debug!("dns filter: no upstream answered");
    None
}

fn forward_tcp(up: SocketAddr, query: &[u8]) -> Option<Vec<u8>> {
    let mut s = TcpStream::connect_timeout(&up, UPSTREAM_TIMEOUT).ok()?;
    let _ = s.set_read_timeout(Some(UPSTREAM_TIMEOUT));
    let _ = s.set_write_timeout(Some(UPSTREAM_TIMEOUT));
    s.write_all(&(query.len() as u16).to_be_bytes()).ok()?;
    s.write_all(query).ok()?;
    let mut len = [0u8; 2];
    s.read_exact(&mut len).ok()?;
    let mut r = vec![0u8; u16::from_be_bytes(len) as usize];
    s.read_exact(&mut r).ok()?;
    Some(r)
}

// ------------------------------------------------------ system integration

/// The resolvers the forwarder should use: `BETAMACSD_DNS_UPSTREAM` (comma
/// list) if set; else the DHCP-offered servers on the default interface plus
/// any manual servers we replaced (`saved`), then public fallbacks. Loopback
/// (that would be us) is skipped.
pub fn discover_upstreams(saved: &[String]) -> Vec<SocketAddr> {
    let mut ips: Vec<String> = Vec::new();
    if let Ok(env) = std::env::var("BETAMACSD_DNS_UPSTREAM") {
        ips.extend(env.split(',').map(|s| s.trim().to_string()));
    } else {
        if let Some(iface) = default_interface() {
            ips.extend(dhcp_dns_servers(&iface));
        }
        ips.extend(saved.iter().cloned());
        ips.extend(["1.1.1.1", "8.8.8.8"].map(str::to_string));
    }
    let mut out: Vec<SocketAddr> = Vec::new();
    for ip in ips {
        let Ok(ip) = ip.parse::<IpAddr>() else { continue };
        if ip.is_loopback() {
            continue;
        }
        let sa = SocketAddr::new(ip, 53);
        if !out.contains(&sa) {
            out.push(sa);
        }
    }
    out
}

fn default_interface() -> Option<String> {
    let out = std::process::Command::new("/sbin/route")
        .args(["-n", "get", "default"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("interface:").map(|s| s.trim().to_string()))
}

/// `ipconfig getpacket en0` → `domain_name_server (ip_mult): {a, b}`.
fn dhcp_dns_servers(iface: &str) -> Vec<String> {
    let Ok(out) = std::process::Command::new("/usr/sbin/ipconfig")
        .args(["getpacket", iface])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("domain_name_server"))
        .flat_map(|l| {
            l.split_once('{')
                .map(|(_, rest)| rest.trim_end_matches('}').to_string())
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Points every network service's DNS at the forwarder and restores it
/// afterwards. The originals are saved root-owned first (`dns-saved.json`)
/// so a daemon restart mid-engagement restores them on startup.
pub struct SystemDns {
    saved_path: PathBuf,
    dry_run: bool,
}

impl SystemDns {
    pub fn new(managed_dir: &Path, dry_run: bool) -> Self {
        Self {
            saved_path: managed_dir.join("dns-saved.json"),
            dry_run,
        }
    }

    pub fn is_engaged(&self) -> bool {
        self.saved_path.exists()
    }

    fn services() -> Vec<String> {
        let Ok(out) = std::process::Command::new("/usr/sbin/networksetup")
            .arg("-listallnetworkservices")
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .skip(1) // "An asterisk (*) denotes ... disabled."
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('*'))
            .map(str::to_string)
            .collect()
    }

    fn get_dns(service: &str) -> Vec<String> {
        let Ok(out) = std::process::Command::new("/usr/sbin/networksetup")
            .args(["-getdnsservers", service])
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| l.parse::<IpAddr>().is_ok()) // "There aren't any…" → none
            .map(str::to_string)
            .collect()
    }

    fn set_dns(service: &str, servers: &[String]) {
        let mut cmd = std::process::Command::new("/usr/sbin/networksetup");
        cmd.args(["-setdnsservers", service]);
        if servers.is_empty() {
            cmd.arg("empty");
        } else {
            cmd.args(servers);
        }
        match cmd.output() {
            Ok(o) if o.status.success() => {}
            Ok(o) => tracing::warn!(
                "networksetup -setdnsservers {service:?} failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => tracing::warn!("networksetup spawn failed: {e}"),
        }
    }

    fn flush_caches() {
        let _ = std::process::Command::new("/usr/bin/dscacheutil")
            .arg("-flushcache")
            .output();
        let _ = std::process::Command::new("/usr/bin/killall")
            .args(["-HUP", "mDNSResponder"])
            .output();
    }

    /// Redirect system DNS to 127.0.0.1. Returns the manual servers that
    /// were configured (to use as upstreams); a no-op if already engaged.
    pub fn engage(&self) -> Vec<String> {
        if let Ok(s) = std::fs::read_to_string(&self.saved_path) {
            let saved: HashMap<String, Vec<String>> = serde_json::from_str(&s).unwrap_or_default();
            return saved.into_values().flatten().collect();
        }
        if self.dry_run {
            tracing::warn!("DRY RUN: would point system DNS at 127.0.0.1");
            return Vec::new();
        }
        let mut saved: HashMap<String, Vec<String>> = HashMap::new();
        for svc in Self::services() {
            saved.insert(svc.clone(), Self::get_dns(&svc));
        }
        if saved.is_empty() {
            tracing::warn!("no network services found; system DNS left alone");
            return Vec::new();
        }
        if let Ok(json) = serde_json::to_string(&saved) {
            if let Err(e) = std::fs::write(&self.saved_path, json) {
                tracing::error!("cannot save DNS settings ({e}); not redirecting");
                return Vec::new();
            }
        }
        for svc in saved.keys() {
            Self::set_dns(svc, &["127.0.0.1".to_string()]);
        }
        Self::flush_caches();
        tracing::warn!("system DNS redirected to the local filter ({} services)", saved.len());
        saved.into_values().flatten().collect()
    }

    /// Put the saved servers back; a no-op if not engaged.
    pub fn restore(&self) {
        let Ok(s) = std::fs::read_to_string(&self.saved_path) else { return };
        if self.dry_run {
            tracing::warn!("DRY RUN: would restore system DNS");
            let _ = std::fs::remove_file(&self.saved_path);
            return;
        }
        let saved: HashMap<String, Vec<String>> = serde_json::from_str(&s).unwrap_or_default();
        for (svc, servers) in &saved {
            Self::set_dns(svc, servers);
        }
        let _ = std::fs::remove_file(&self.saved_path);
        Self::flush_caches();
        tracing::warn!("system DNS restored ({} services)", saved.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn suffix_matching() {
        let list = l(&["khanacademy.org", "*.kastatic.org", ".kasandbox.org"]);
        assert!(matches("khanacademy.org", &list));
        assert!(matches("WWW.KhanAcademy.org.", &list));
        assert!(matches("cdn.kastatic.org", &list));
        assert!(matches("a.b.kasandbox.org", &list));
        assert!(!matches("notkhanacademy.org", &list));
        assert!(!matches("khanacademy.org.evil.com", &list));
        assert!(!matches("youtube.com", &list));
        assert!(!matches("anything", &l(&["", " "])));
    }

    #[test]
    fn verdicts() {
        let allow = Mode::Allow { allow: l(&["khanacademy.org"]), block: l(&["youtube.com"]) };
        assert_eq!(decide(&allow, "www.khanacademy.org"), Verdict::ForwardLearn);
        assert_eq!(decide(&allow, "www.youtube.com"), Verdict::Deny);
        assert_eq!(decide(&allow, "reddit.com"), Verdict::Deny);
        assert_eq!(decide(&allow, "dns.google"), Verdict::Deny);
        assert_eq!(decide(&allow, "use-application-dns.net"), Verdict::Deny);

        let block = Mode::Block { block: l(&["tiktok.com"]) };
        assert_eq!(decide(&block, "www.tiktok.com"), Verdict::Deny);
        assert_eq!(decide(&block, "m.tiktok.com"), Verdict::Deny);
        assert_eq!(decide(&block, "reddit.com"), Verdict::Forward);
        assert_eq!(decide(&block, "mozilla.cloudflare-dns.com"), Verdict::Deny);

        assert_eq!(decide(&Mode::Audit, "anything.example"), Verdict::Forward);
        assert_eq!(decide(&Mode::Audit, "use-application-dns.net"), Verdict::Deny);
        assert_eq!(decide(&Mode::Off, "use-application-dns.net"), Verdict::Forward);
    }

    /// A query for www.example.com A, then a response with a CNAME + A + AAAA.
    fn sample_query() -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in ["www", "example", "com"] {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.extend_from_slice(&[0, 0, 1, 0, 1]);
        q
    }

    #[test]
    fn parses_question_and_answers() {
        let q = sample_query();
        let (name, qtype, qend) = question(&q).unwrap();
        assert_eq!(name, "www.example.com");
        assert_eq!(qtype, 1);
        assert_eq!(qend, q.len());

        let mut r = q.clone();
        r[2] = 0x81;
        r[3] = 0x80;
        r[7] = 3; // ANCOUNT
        // CNAME www.example.com -> ptr(12) "cdn" + ptr to "example.com" at 16
        r.extend_from_slice(&[0xC0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 6, 3, b'c', b'd', b'n', 0xC0, 16]);
        // A cdn.example.com (name as pointer to the CNAME rdata at offset qend+12)
        let cname_rdata = (qend + 12) as u16;
        r.extend_from_slice(&[0xC0 | (cname_rdata >> 8) as u8, cname_rdata as u8]);
        r.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 93, 184, 216, 34]);
        // AAAA
        r.extend_from_slice(&[0xC0 | (cname_rdata >> 8) as u8, cname_rdata as u8]);
        r.extend_from_slice(&[0, 28, 0, 1, 0, 0, 0, 60, 0, 16]);
        r.extend_from_slice(&[0x26, 0x06, 0x28, 0, 0x02, 0x20, 0, 1, 0x2, 0x48, 0x18, 0x93, 0x25, 0xc8, 0x19, 0x46]);
        let ips = answer_ips(&r);
        assert_eq!(ips.len(), 2);
        assert_eq!(ips[0], "93.184.216.34".parse::<IpAddr>().unwrap());
        assert!(matches!(ips[1], IpAddr::V6(_)));
    }

    #[test]
    fn nxdomain_reply_shape() {
        let q = sample_query();
        let (_, _, qend) = question(&q).unwrap();
        let r = nxdomain(&q, qend);
        assert_eq!(&r[..2], &q[..2]); // id
        assert_eq!(r[2] & 0x80, 0x80); // QR
        assert_eq!(r[2] & 0x01, 0x01); // RD kept
        assert_eq!(r[3] & 0x0F, 3); // NXDOMAIN
        assert_eq!(&r[4..12], &[0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&r[12..], &q[12..]);
        let (name, _, _) = question(&r).unwrap();
        assert_eq!(name, "www.example.com");
    }

    #[test]
    fn read_name_rejects_pointer_loops() {
        let buf = [0xC0, 0x00];
        assert!(read_name(&buf, 0).is_none());
    }

    #[test]
    fn upstream_env_override_and_loopback_skip() {
        // Can't set env safely across parallel tests; exercise the parser
        // path through `saved` + fallbacks instead.
        let ups = discover_upstreams(&l(&["127.0.0.1", "192.0.2.53", "bogus"]));
        assert!(ups.contains(&"192.0.2.53:53".parse().unwrap()));
        assert!(!ups.iter().any(|u| u.ip().is_loopback()));
        assert!(ups.contains(&"1.1.1.1:53".parse().unwrap()));
    }
}
