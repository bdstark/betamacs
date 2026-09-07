# Site filter — internet allow/block lists

Steers a child with **no earned-time balance** to the sites that earn it, and
keeps a short list of sites off-limits regardless of balance. It is the
by-*name* enforcement the earned-time gate was missing: pf filters by IP, and
a modern site is a spread of CDN hosts that a "resolve `khanacademy.org` and
allow that address" rule never covers.

| Balance | `allowHosts` + earn sources | everything else | `blockHosts` |
|---|---|---|---|
| depleted (gate active) | reachable | **blocked** | blocked |
| available / no gate window | reachable | reachable | **blocked** |

Config module `siteFilter` in the signed `betamacs-config` package
(layerable like the others; **disabled by default**):

```jsonc
"siteFilter": {
  "enabled": true,
  "allowHosts": ["khanacademy.org", "kastatic.org", "kasandbox.org",
                 "youtube.com", "googlevideo.com", "ytimg.com"],
  "blockHosts": ["tiktok.com"],
  "auditOnly": false
}
```

Hosts are **domain suffixes**: `kastatic.org` matches `cdn.kastatic.org`;
`*.kastatic.org` and `.kastatic.org` mean the same thing. The earn sources'
`browserHostSuffix` values are always part of the allowlist, so the child can
always reach what earns credit even if `allowHosts` is empty.

Like earned time, the filter applies only to a **provisioned kid device**
(one with a root-owned `tasks.json`, delivered with the `ext:betamacs-tasks`
grant). A fleet-wide config never redirects the parent Mac's DNS.

## How it is enforced

The agent resolves the policy (it already parses the config and reports the
earned-time snapshot every ~20 s); the daemon enforces it. The `earn` socket
message carries `filterEnabled`, `filterAuditOnly`, `filterAllowHosts` and
`filterBlockHosts` alongside the earn sources' hosts.

betamacsd (root) does three things, all in `src/dnsfilter.rs` and the
quarantine code in `src/bin/betamacsd.rs`:

1. **A local DNS forwarder** on `127.0.0.1:53` (UDP + TCP). Each query is
   decided by name: forward it upstream, or answer NXDOMAIN. The upstreams
   are the DHCP-offered resolvers on the default interface (or the manual
   servers it replaced), with public fallbacks; `BETAMACSD_DNS_UPSTREAM`
   overrides. Public DoH provider names (`dns.google`, `cloudflare-dns.com`,
   …) and Firefox's DoH canary (`use-application-dns.net`) are always denied
   while enforcing, so a browser's "secure DNS" cannot bootstrap around it.
2. **The system resolver is pointed at it** (`networksetup -setdnsservers …
   127.0.0.1` on every service). The originals are saved root-owned first
   (`dns-saved.json`) and restored on release — and on daemon start, if a
   previous instance died mid-engagement. A standard user cannot change DNS
   settings back.
3. **pf**, in one of two shapes:
   - **Earning mode (balance depleted):** the lockdown ruleset passes 80/443
     only to a pf table `<betamacs_allow>` that the forwarder fills with
     whatever the *allowed names resolve to* (added before the answer is
     returned, so the client's connection is already permitted). Port 53
     passes only to the upstreams. Everything else drops, exactly like the
     tamper quarantine (loopback, DHCP, SSH-in and the otactl origins stay
     open). The table is seeded with the earn hosts' apex addresses on load
     so an already-open page keeps working.
   - **DNS lock (balance available, blocklist non-empty):** no lockdown —
     just `port 53/853` dropped except to the upstreams, 443 dropped to the
     well-known public DoH resolver IPs, and UDP 443 (QUIC) dropped, so the
     blocklist can't be resolved around.

Both are the same anchor (`com.apple/250.BetamacsQuarantine`) the tamper
quarantine uses; a full block (tamper/exposure/challenge) still takes
precedence and turns the filter off while it stands.

The daemon composes `pf mode × DNS mode` purely (`compose()` in betamacsd,
unit-tested):

| full block | balance depleted | filter | pf | DNS |
|---|---|---|---|---|
| yes | – | – | Full | off |
| no | yes | on | EarningTable | Allow(sources ∪ allowHosts ∪ mgmt; minus blockHosts) |
| no | yes | audit | EarningStatic (legacy) | Audit |
| no | yes | off | EarningStatic (legacy) | off |
| no | no | on, blockHosts set | DnsLock | Block(blockHosts) |
| no | no | audit | open | Audit |
| no | no | off / nothing to block | open | off |

If the forwarder cannot bind port 53 (another local resolver), the daemon
falls back to the legacy static earning gate rather than leaving the child
open, and logs it.

## Discovery: which hosts does a site or app need?

Khan Academy publishes its list (`kastatic.org`, `kasandbox.org`, plus
`youtube.com`/`googlevideo.com` for videos). **Khan Academy Kids does not.**
Set `auditOnly: true`: the forwarder runs and logs every name without
blocking anything (and the earned-time gate falls back to the legacy static
allowlist meanwhile). Then read what the app resolved:

- `/Library/Application Support/betamacs/site-audit.log` — one line per
  distinct name per 5 minutes: `2026-09-06T14:02:11 audit allow api.khankids.example`.
- The status HUD's `Sites:` line and the daemon `status` reply's
  `siteFilter.{mode, deniedRecent, forwardedRecent}` — the last 20 unique
  names each way, so a parent at the machine can see what to add.

In enforcing modes the same log records every `DENY`, so a page that breaks
in earning mode tells you which host it was missing.

Allowlisting `youtube.com` + `googlevideo.com` for Khan's videos also opens
YouTube itself in earning mode; that is a parent's call. The `focusLimit`
module (same-tab scrolling lockout) is the intended counterweight.

## Limitations

- **Names, not content.** Anything hosted under an allowed suffix is allowed;
  a CDN address shared by other sites is reachable in earning mode once an
  allowed name resolves to it. Fine for a child, not a security boundary.
- **IP literals.** A URL typed as an address bypasses DNS; in earning mode
  pf still drops it (not in the table), in DNS-lock mode it is not stopped.
- **Proxies/VPNs** are out of scope here (the UniFi VLAN handles that).
- **Apps that pin their own resolver** (rare on macOS; most use the system
  resolver) would see port 53 dropped and fail closed.
- Uses `networksetup`, which is visible in System Settings → Network → DNS
  while engaged. Cosmetic; a standard user cannot edit it.

## Testing without root

Run the daemon in dry run with the forwarder on an alternate port:

```sh
P=/tmp/bmd; mkdir -p "$P/Library/Application Support/betamacs" "$P/var/run"
echo '{}' > "$P/Library/Application Support/betamacs/tasks.json"   # "kid device"
BETAMACSD_PREFIX=$P BETAMACSD_QUARANTINE_DRYRUN=1 BETAMACSD_QUARANTINE_GRACE_SECS=3600 \
  BETAMACSD_DNS_PORT=5300 target/release/betamacsd &
S=$P/var/run/betamacsd.sock
printf '{"type":"heartbeat","pid":1,"captureOk":true,"enabled":true}\n' | nc -U $S -w1
printf '{"type":"earn","secs":0,"gateActive":true,"allowHosts":["khanacademy.org"],"filterEnabled":true,"filterAllowHosts":["kastatic.org"],"filterBlockHosts":["tiktok.com"]}\n' | nc -U $S -w1
sleep 16   # one watchdog tick: balance 0 + gate → earning mode
dig -p 5300 @127.0.0.1 cdn.kastatic.org     # answers; log: "would add to pf table"
dig -p 5300 @127.0.0.1 www.youtube.com      # NXDOMAIN
printf '{"type":"status"}\n' | nc -U $S -w1  # siteFilter.mode = "allow"
```
