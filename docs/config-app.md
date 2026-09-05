# betamacs config app — standalone, store-backed configuration

Status: **design + initial scaffold** (2026-09-03). The existing `webapp/`
editor is untouched and still works against a live betamacs (`/api/package`
push/pull). This document plus the scaffold under `webapp/src/stores/` and
`webapp/src/config/` are the starting point for migrating that editor into a
standalone app that stages edits locally and **publishes signed, resolved
config to a store** (today: otactl) rather than pushing to one running app.

## Goals

- **One config, many devices.** Author policy once, publish it into otactl's
  signed pipeline; hausmeister delivers it to every entitled Mac
  (docs/managed-mode.md), instead of hand-pushing to a single app over
  `/api/package`.
- **Stage locally, publish deliberately.** Keep the current
  localStorage-staged editing model; "Publish" is an explicit, signed write to
  a store — not a live mutation.
- **Store abstraction.** Editing, staging, inheritance, and assignment are all
  independent of *where* config lives. otactl is the first (and only current)
  backend; `s3` and `file` are future backends behind the same interface.
- **Signatures cover exactly what the device runs.** The store holds the
  fully-resolved, author-signed bytes; nothing is merged at retrieval.
- **Room to grow.** Config→device assignment, and later live-status dashboards,
  slot into the same app without reworking the core.

## Non-goals (now)

- Ripping out or rewriting the existing module editors (`modules/*.ts`).
- Building `s3`/`file` stores (interface extension points only).
- Live device dashboards (see "Future" — betamacs already exposes status via
  its daemon/statusframe; the app leaves a seam for it).
- Users-as-assignment-targets (channels/devices only for now).

---

## Store abstraction

A `Store` is an arbitrary config storage location/instance, addressed by
`(app, channel)` plus impl-specific connection info. Scaffolded in
`webapp/src/stores/store.ts`:

```ts
type StoreKind = "otactl" | "s3" | "file";

interface StoreRef { app: string; channel: string; }

interface PublishInput {
  ref: StoreRef;
  resolved: ResolvedPackage;   // inheritance-flattened, time-layers retained
  version: string;             // artifact version string
  ttlSeconds?: number;         // author-signature validity window
  note?: string;
}

interface PublishResult {
  ref: StoreRef; version: string; sha256: string;
  epoch?: number; notAfter?: string; storedAt: string;
}

interface Store {
  readonly kind: StoreKind;
  describe(): string;
  publish(input: PublishInput): Promise<PublishResult>;
  fetchLatest?(ref: StoreRef): Promise<StoredConfig | undefined>;  // optional
  listChannels?(app: string): Promise<string[]>;                   // optional
}
```

Reads are **optional** on purpose: not every backend exposes retrieval to a
browser. otactl's resolve/download endpoints authenticate by *device* mTLS, so
the browser generally cannot read back a published config; `s3`/`file` will be
able to. `publish` is the one mandatory operation.

### Design decision: store holds the RESOLVED artifact

The store is content-agnostic bytes. It never merges at retrieval, for two
reasons (constraint 4):

1. **otactl stays dumb.** It ships signed blobs; it must not understand betamacs
   config semantics.
2. **The author signature must cover exactly what the device runs.** If the
   store merged on read, the signed bytes and the run bytes would differ.

So all merging (inheritance flattening) happens *before* publish, in
`config/inheritance.ts`. Time variation is the one thing NOT flattened (below).

### otactl store impl

`webapp/src/stores/otactl.ts` — `OtactlStore implements Store`. Config:

```ts
interface OtactlStoreConfig {
  publishEndpoint: string;  // typeserver backend (see below)
  backendUrl: string;       // otactl device-origin URL (--backend-url)
  publisherId: string;      // "betamacs-config" (--publisher-id)
  arch?: string;            // default "arm64"
  authorSecret?: string;    // typeserver signing-key secret name
}
```

`publish()` does **not** talk to otactl directly (it can't — see next section);
it POSTs the resolved bytes to `publishEndpoint`. `fetchLatest()` returns
`undefined` (documented extension point for a future read proxy).

### Where s3 / file stores slot in

Both implement the same `Store` interface:

- **`file` store** — writes the resolved+signed wrapper to a path (dev, or a
  synced folder). Can implement `fetchLatest`/`listChannels` trivially. Signing
  still needs the author key (local `author-key.pem` path already supported by
  `scripts/author-key.sh`) — a `file` store can sign locally where a browser
  can't, or in a small helper.
- **`s3` store** — `PUT`/`GET` object per `(app, channel)`; presigned URLs or a
  thin backend for credentials. Read-back and listing come for free. Same
  resolved-artifact contract; anti-rollback epoch would have to be modeled in
  the object (otactl gives it for free).

Neither needs the typeserver publish backend that otactl requires; that
delegation is an otactl-specific detail hidden inside `OtactlStore`.

---

## Why otactl publish must route through a typeserver backend

A browser **cannot** publish to otactl directly (constraint 3). Two credentials
are required that a browser cannot present:

1. **Publisher client cert (mTLS).** otactl's upload endpoint authenticates the
   publisher by client certificate. Browsers can't present arbitrary client
   certs to `fetch`.
2. **Author signing.** A pinned fleet accepts config only as an *author-signed*
   wrapper. Signing goes through typeserver's oracle (`POST /api/secrets/sign`,
   authenticated by the `ts_session` cookie); the author key never leaves the
   server.

So `OtactlStore.publish()` delegates to a small **typeserver backend endpoint**
that holds the publisher identity and does, server-side, what
`scripts/publish.sh config` does at the CLI: author-sign the bytes, then
`otactl boot-usb upload` over mTLS.

### typeserver publish-backend contract

`POST {publishEndpoint}` (e.g. `.../api/betamacs/publish`) — same-site to
typeserver so the browser's `ts_session` cookie rides along
(`credentials: "include"`). Shapes mirrored in `stores/otactl.ts` as
`OtactlPublishRequest` / `OtactlPublishResponse`.

**Request (application/json):**

```jsonc
{
  "app": "betamacs-config",
  "channel": "stable",
  "arch": "arm64",
  "version": "2026.09.03-1",
  "publisherId": "betamacs-config",
  "backendUrl": "https://<device-origin>",
  "ttlSeconds": 3600,
  "authorSecret": "betamacs-author",   // typeserver secret name (optional; backend may pin)
  "note": "raise school-hours strictness",
  "package": { /* the fully-resolved ResolvedPackage — signed VERBATIM */ }
}
```

**Response 200 (application/json):**

```jsonc
{
  "version": "2026.09.03-1",
  "sha256": "<hex of the signed artifact otactl stored>",
  "epoch": 42,                 // otactl's monotonic anti-rollback epoch
  "notAfter": "2026-09-03T15:00:00Z",  // author-signature expiry
  "storedAt": "2026-09-03T14:00:00Z"
}
```

**Errors:** `401` no/invalid `ts_session`; `403` signing locked (typeserver
`LOCK_SECRET` timelock not yet elapsed — the integral change-lock, see
managed-mode.md) or publisher not entitled; `409` epoch/rollback conflict;
`502` otactl upload failed. Body is a plain-text reason (surface it in the UI).

**Backend responsibilities (server-side, must mirror `publish.sh`):**

1. Authenticate the caller via `ts_session`.
2. Reject an already-authored wrapper (the `{authorSignature, packageB64}`
   double-wrap guard from `publish.sh`).
3. Canonicalize `package` to bytes and author-sign **verbatim** via the oracle
   with `authorSecret` and `ttlSeconds` — never merge/reorder/re-derive.
4. `otactl boot-usb upload --app <app> --channel <channel> --arch <arch>
   --version <version> --role runtime --format betamacs-package-json
   --publisher-id <publisherId> --backend-url <backendUrl>` using the publisher
   client cert it holds.
5. Return the sha256 + assigned epoch.

This is the only new server component the migration needs. It is a thin
wrapper over the existing signing oracle + `otactl` CLI; it holds no new
secret that typeserver/otactl don't already hold.

> **IMPLEMENTED (Phase 2).** This endpoint is built in the typeserver repo:
> `cmd/server/betamacs.go`, registered from `cmd/server/main.go` via
> `registerBetamacsHandlers(auth, sm)`. It reuses the existing session gate
> (`auth.authorized`) and the signing oracle (`sm.SignWithSecret`). The
> signing input it builds — `betamacs-config-author-v1\n<authoredAt>\n
> <notAfter>\n<packageB64>` — is byte-identical to `scripts/author-key.sh`
> (verified: a wrapper it would produce validates against `author-pubkey.pem`
> with `openssl dgst -sha256 -verify`), and it uploads with the same
> `otactl boot-usb upload ... --format betamacs-package-json` invocation as
> `scripts/publish.sh config`.

### Read-back endpoint

`GET {readEndpoint}?app=&channel=&arch=` (e.g. `.../api/betamacs/config`) —
same-site, `credentials:"include"`. Lets the app load the currently published
config for a store instead of pasting JSON.

**Response 200:** `{ version, sha256, epoch?, notAfter?, storedAt?, package }`
where `package` is the raw `ResolvedPackage` JSON — the backend fetches the
otactl artifact (the author-signed wrapper), unwraps it
(`base64decode(wrapper.packageB64)`), and returns the inner package the editor
edits. `version`/`sha256`/`epoch` are the artifact's release metadata;
`notAfter`/`storedAt` come from the wrapper. **404** when no config is
published for that app/channel/arch (a clean empty; the app then offers Import
JSON).

> **IMPLEMENTED.** `handleBetamacsConfigRead` (typeserver
> `cmd/server/betamacs.go`) downloads over the **same betamacs-config publisher
> mTLS identity** it publishes with — no new credential — by calling otactl's
> new `publisher.Download` against `GET /firmware/artifacts/download`. See
> "Read-back auth model" below. The app's **Load from store** button
> (`OtactlStore.fetchLatest`) populates the editor from the returned package and
> shows the loaded version + sha256; Import JSON remains as a manual fallback.

#### Read-back auth model (why the publisher cert)

otactl's device artifact route (`/firmware/artifacts/current`) is device-mTLS
gated and the operator download (`/admin/firmware/download`) needs a human web
session (`withRole(RoleViewer)`) — typeserver has neither toward otactl; it
authenticates purely as the **betamacs-config publisher**. So otactl gained an
additive `GET /firmware/artifacts/download` that authorizes exactly like
`/firmware/upload`: an operator session **or** a scoped publisher client
certificate. A publisher that may *upload* runtime for an app may now *read*
that same app's current artifact back — read is strictly less privileged than
the write the cert already holds, and the cert's app/arch/channel scope is
enforced, so it can never read another app. This reuses the cert typeserver
already mounts at `/pki/publisher-betamacs-config.*`; no new PKI, scope field,
or service identity. otactld listens with `tls.VerifyClientCertIfGiven`, so the
client cert is verified on this GET just as on the upload POST.

### Device assignment endpoints

`GET {origin}/api/betamacs/devices` → `[{ deviceId, label?, channel? }]`;
`POST {origin}/api/betamacs/assign` `{ deviceId, channel }`. The app derives
the origin from the publish endpoint.

> **STILL STUBBED — 501, by design (not yet signed off).** Unlike read-back,
> device assignment is a **fleet-admin mutation**, not something the
> betamacs-config *publisher* identity should be able to do: a publisher cert is
> scoped to publishing one app's artifacts, and letting it re-point devices
> would be a privilege escalation. otactl's device routes (`GET /admin/devices`,
> `PUT /admin/devices/{id}/policy`) are already gated by an operator **web
> session** (`withRole`), which typeserver does **not** hold toward otactl — so
> even read-only device *listing* is **not** trivially available over the
> publisher cert and stays 501 for now. Wiring it needs the operator/service
> auth decided in "Config → device assignment" below. The UI (Devices tab) loads
> against these and degrades gracefully.

### Deploying / testing the typeserver backend

The endpoint runs inside the existing typeserver process. Environment:

| Var | Purpose |
|---|---|
| `BETAMACS_AUTHOR_SECRET` | typeserver secret name of the author signing key (per-call `authorSecret` overrides) |
| `BETAMACS_AUTHOR_TTL` | default signature TTL seconds (default 3600) |
| `BETAMACS_AUTHOR_PASSPHRASE` | optional, if the author secret is passphrase-protected |
| `OTACTL_BIN` | path to the `otactl` CLI (default `otactl`) |
| otactl publisher env | `OTACTL_BACKEND_URL` and the enrolled publisher cert/config the CLI reads (same as running `publish.sh` on that host) |

To test: sign into typeserver in a browser (gets `ts_session`), point the
config app's **Publish endpoint** at `https://<typeserver>/api/betamacs/publish`
and **otactl backend URL** at the device origin, then Publish. The backend
signs and runs `otactl boot-usb upload`; the response carries `sha256` and
(best-effort) `epoch`. Before otactl publisher creds are present on the host,
publish fails at the upload step with a `502` carrying otactl's stderr, but the
sign step still exercises the oracle (and its timelock).

---

## Stage → publish flow

Unchanged staging model; new publish target.

1. **Edit** modules; state stays in `localStorage` (existing `store.ts`,
   `betamacs.package`). Add a `betamacs.configSource` draft that carries the
   `base` reference and `timeLayers` alongside the `Package`.
2. **Resolve** on publish: `resolveInheritance(leaf, lookup)` walks the base
   chain and flattens to a `ResolvedPackage` (inheritance gone, time-layers
   kept).
3. **Publish**: `store.publish({ ref, resolved, version, ttlSeconds })`. The
   `OtactlStore` POSTs to the typeserver backend, which signs + uploads.
4. **Feedback**: show `sha256`, `epoch`, `notAfter` from `PublishResult`.

The current live push (`PUT /api/package`) can remain as a separate
**"push to a running app"** dev affordance (staging against a local betamacs);
it is orthogonal to store publish and need not be removed.

---

## Inheritance / merge model

Two axes of variation, resolved at **different times** (constraint 5):

| Axis | Resolved | By whom | In the stored artifact |
|---|---|---|---|
| **Inheritance** (base + child deltas) | compile-time (publish) | the config app | gone — flattened into `overrides` |
| **Time** (day/hour-varying policy) | runtime | the betamacs agent's clock | retained — as `timeLayers` |

Consequence: **a base change means re-flatten + re-sign + re-publish every
child.** That is intentional — the device never sees the base, so the signature
always covers concrete bytes.

### Flattening (scaffolded in `config/inheritance.ts`)

- `orderedPatches(pkg)` → the exact patch order betamacs `resolve()` applies:
  each referenced layer in `layers` order, then `overrides` last.
- `combineModulePatches(seq)` → collapses the whole ordered sequence into ONE
  `ModulePatches`. Safe because `resolve()` is field-independent last-write-wins
  (with `detection.triggers` accumulating per class); applying the collapsed
  patch to module defaults yields the identical `Effective`. This is why the
  stored artifact is a single flat `overrides` block, not a re-derivable stack.
- `flattenChain(root→leaf)` / `resolveInheritance(leaf, lookup)` → walk the
  base chain (cycle- and missing-base-guarded), concatenate time-layers (base
  first so an active leaf layer wins), merge text sets by name (leaf wins),
  emit a `ResolvedPackage`.

### Worked example

**Base** `family-base` (a `ConfigSource`, `base` unset):

```jsonc
{ "name": "family-base",
  "package": { "version": 1, "namedConfigs": [], "layers": [],
    "overrides": {
      "detection": { "enabled": true, "confidenceThreshold": 0.35 },
      "earnedTime": { "enabled": false } },
    "textSets": [] } }
```

**Child** `kid-alex` inherits it and adds a challenge, plus a **"school hours"
time-delta** that tightens detection and turns on the focus limit on weekday
mornings:

```jsonc
{ "name": "kid-alex", "base": "family-base",
  "package": { "version": 3, "namedConfigs": [], "layers": [],
    "overrides": { "challenge": { "enabled": true } },
    "textSets": [] },
  "timeLayers": [
    { "name": "school-hours",
      "when": { "days": ["mon","tue","wed","thu","fri"], "from": "07:00", "to": "15:00" },
      "settings": {
        "detection": { "confidenceThreshold": 0.25 },
        "focusLimit": { "enabled": true } } } ] }
```

`resolveInheritance(kid-alex, lookup)` stores exactly these bytes (the
`ResolvedPackage`):

```jsonc
{ "version": 3,
  "namedConfigs": [], "layers": [],
  "overrides": {
    "detection":  { "enabled": true, "confidenceThreshold": 0.35 },
    "earnedTime": { "enabled": false },
    "challenge":  { "enabled": true }
  },
  "textSets": [],
  "timeLayers": [
    { "name": "school-hours",
      "when": { "days": ["mon","tue","wed","thu","fri"], "from": "07:00", "to": "15:00" },
      "settings": {
        "detection": { "confidenceThreshold": 0.25 },
        "focusLimit": { "enabled": true } } } ] }
```

Note what happened to each axis:

- **Inheritance flattened**: base's `detection`/`earnedTime` and child's
  `challenge` are merged into one `overrides` block. `family-base` is gone from
  the artifact.
- **Time retained**: `school-hours` is NOT baked into `overrides`. Off-hours the
  device runs `confidenceThreshold: 0.35` and `focusLimit.enabled: false`
  (from the flat base); weekday 07:00–15:00 the agent overlays the layer →
  `0.25` and focus limit on. Both variants are inside the one signed artifact.

The author signature is computed over these bytes verbatim (the wrapper from
`author-key.sh`), so the device verifies exactly what it runs, all time-variants
included.

---

## betamacs schema / `resolve()` addition (follow-up, NOT in this change)

The stored artifact carries `timeLayers`, which today's betamacs `Package`
(both TS `schema.ts` and Rust `settings.rs`) does not know. Required follow-ups,
specified here, implemented separately:

1. **Schema.** Add an optional `timeLayers: TimeLayer[]` to `Package` (TS) and
   the Rust mirror, where:
   ```
   TimePredicate { days: string[]; from: "HH:MM"; to: "HH:MM" }   // local clock
   TimeLayer     { name; when: TimePredicate; settings: ModulePatches }
   ```
   (Currently defined only in `config/inheritance.ts` to avoid touching the live
   editor.) `from`/`to` may wrap past midnight; `days` empty = every day.
2. **Runtime resolve.** betamacs `resolve()` already merges layers/overrides
   and already evaluates `earnedTime`/`focusLimit` schedules against the clock
   (`src/earned.rs` resolves schedules via `/bin/date`). Extend it to, after
   computing the flat `Effective` from `overrides`, apply each `timeLayer` whose
   `when` predicate is active **now**, in array order, as an additional patch
   layer. This reuses the existing schedule-evaluation code path.
3. **Re-resolve on the clock.** The agent must recompute `Effective` when a
   time boundary is crossed (it already ticks for earned-time/focus); a layer
   activating/deactivating is just another recompute trigger. No new signed
   delivery — the bytes already on the device contain every variant.

Until this lands, a device running the older `resolve()` simply ignores
`timeLayers` and runs the flat base — a safe, forward-compatible degradation.

---

## Config → device assignment (over otactl)

Scaffolded as interface only in `store.ts` (`DeviceTarget`, `DeviceAssignment`,
`AssignmentApi`). The app models assignment as a strongly-typed layer over
otactl's device API.

### The single-channel caveat (constraint 6)

otactl resolves **one** channel per device for **all** apps: `resolveChannel`
(otactl `internal/firmware/service.go`) reads a single `device.Channel` and
otherwise returns the server `defaultChannel` — there is no per-app channel and
no per-app fallback. (`ResolveManifestVersion`'s comment is explicit: "Channels
gate what publishers may upload, not what devices may read.")

So "assign config X to device D" can only mean "set `D.Channel` = X's channel",
and that same channel then governs D's `betamacs` app build and `betamacs-tasks`
bank too. Two workable models:

- **Model A — shared lane + entitlement (recommended, matches today).** Keep a
  single fleet-wide `stable` config channel and differentiate kids by
  *entitlement*, exactly as earned-time already does: the `ext:betamacs-tasks`
  grant marks a device as a managed/kid Mac (docs/earned-time.md). Per-kid
  *policy* differences then ride on inheritance (a child `ConfigSource` per
  kid) — but note: with one shared channel, all kids receive the same published
  artifact, so per-kid *config* still needs per-kid channels. Use A when kids
  share policy and differ only by entitlement.
- **Model B — per-kid channels.** Give each kid device its own `device.Channel`
  (e.g. `kid-alex`) and publish that kid's flattened config to that channel.
  Works within otactl as-is, but because the channel is device-wide it also
  pins that device's app/tasks lanes to the same name — so those artifacts must
  be published to every kid channel too (or otactl must fall back, which it does
  not). This is the friction point.

### Recommendation: Model B+ — per-app channel override (`AppChannels`)

**Recommended target.** Add a per-app channel override to otactl so
`betamacs-config` can follow a per-kid channel while a device's `betamacs` /
`betamacs-tasks` lanes stay on `stable`, with no cross-app coupling. The hook is
already in place: `resolveChannel(ctx, app, arch, deviceID)`
(`internal/firmware/service.go`) **already takes `app`** but ignores it. The
change:

- Extend the device record with an optional `AppChannels map[string]string`
  (persisted as a JSON column / side table keyed by device id).
- In `resolveChannel`, consult in order: `AppChannels[app]` →
  `device.Channel` → server `defaultChannel`. This is a pure read-path change;
  uploads and the manifest signature are untouched.

This keeps Model A (shared lane + `ext:betamacs-tasks` entitlement) working
unchanged for fleets where kids share policy, and makes true per-kid *config*
assignment a single-field write that does not disturb the app/tasks lanes —
resolving the Model B friction point (channel-wide pinning) described above.

If the `AppChannels` change is not wanted, the fallback is **Model B as-is**:
assignment writes `device.Channel` and every app for that device must publish to
the per-kid channel. The endpoints below are written to work for either model —
they set an (app, channel) pair per device; with `AppChannels` that lands in the
map, without it the `app=betamacs-config` write degrades to setting
`device.Channel` (and the UI warns about cross-app pinning).

### otactl operator endpoints (proposed — needs sign-off before writes)

Assignment is a **fleet-admin** action, so these are **operator/web-session**
gated (`withRole`), NOT publisher-cert gated. Two of the three already exist and
can be reused as-is; only the setter is new.

| Method / path | Auth | Status | Purpose |
|---|---|---|---|
| `GET /admin/devices` | `withRole(RoleViewer)` | **exists** | list devices (id, app, arch, channel, description, lastSeen) |
| `GET /admin/devices/{id}` | `withRole(RoleViewer)` | **exists** | one device's record |
| `PUT /admin/devices/{id}/app-channel` | `withRole(RoleAdmin)` | **NEW** | body `{ app, channel }`; sets `AppChannels[app]=channel` (or, without the map change, sets `device.Channel` only when `app` is the device's own app) — audited |

The new setter mirrors the existing `PUT /admin/devices/{id}/policy` handler and
writes through a new `store.SetDeviceAppChannel(ctx, deviceID, app, channel)`
(or `SetDeviceChannel` in the fallback model), appending an audit event.

**Auth decision to confirm:** these must be reached with an otactl **operator
identity**, which typeserver does not currently have. Two options for the
typeserver→otactl hop, pick one:

1. **Issue typeserver a service/operator identity** (a `CN=typeserver` client
   cert mapped to an operator role, or a service bearer token otactl accepts on
   `/admin/*`). typeserver then calls `/admin/devices*` on the operator's behalf,
   still gated behind the human `ts_session` at typeserver's own edge. **This is
   the recommended path** — it keeps the fleet-mutating credential (operator)
   distinct from the publishing credential (publisher cert), so a compromised
   publisher cert cannot re-point devices.
2. **Forward the operator's own session** — only viable if typeserver and otactl
   share the same operator SSO and otactl will accept a forwarded/introspected
   token. More coupling; not recommended.

Until one is chosen and provisioned, `/api/betamacs/devices` and
`/api/betamacs/assign` stay **501** (device listing included — it rides the same
operator gate). **Do not implement the assignment writes without this sign-off.**

### typeserver contracts

- `GET {origin}/api/betamacs/devices?app=&arch=` → `200 [{ deviceId, app, arch,
  channel, appChannel?, description?, lastSeen? }]`. Session-gated; proxies
  otactl `GET /admin/devices` over the chosen operator identity.
- `POST {origin}/api/betamacs/assign` `{ deviceId, app, channel }` → `200 { ok:
  true, deviceId, app, channel }`. Session-gated; the fleet-mutating call,
  proxies otactl `PUT /admin/devices/{id}/app-channel`. `app` defaults to
  `betamacs-config`. **This is the write that requires sign-off.**

### UI (Devices tab)

A read-only list first: device id, description, current channel, and (with
`AppChannels`) the effective `betamacs-config` channel. Each row gets a channel
picker whose "Assign" action calls `/api/betamacs/assign` — disabled/behind a
confirm until the write path is signed off, matching the loud-error publish
style. With the fallback model, the picker shows a cross-app-pinning warning.

Users-as-targets are future (a user → device(s) mapping layer above this).

---

## Hosting & deploy (v1 — typeserver-hosted, same-origin)

The config app ships as a static bundle **served by typeserver itself**, behind
the same session gate as the publish endpoint. Same-origin is the point: the
page's `fetch` calls use `credentials:"include"`, so the `ts_session` cookie
that authorizes the page also authorizes its publish/read calls — no CORS, no
second login, and the browser never handles the publisher cert or backend URL.

### Route

`GET /betamacs` (→ redirects to `/betamacs/`) serves the app. Unauthenticated
requests redirect to `/login.html`, matching typeserver's other operator pages.
Assets are served under `/betamacs/assets/…`, also gated. Full page URL in
production: `https://<typeserver-origin>/betamacs/`.

Wired in `cmd/server/main.go` as a `http.StripPrefix("/betamacs/",
http.FileServer(http.Dir("./web/betamacs")))` guarded by `auth.authorized`.

### Zero-config for a fresh page

`defaultStoreConfig()` (`webapp/src/store.ts`) defaults the publish/read
endpoints to **same-origin** (`${location.origin}/api/betamacs/publish` and
`/api/betamacs/config`) and leaves `publisherId`/`backendUrl` **blank** — the
server supplies those from its own env. So a freshly opened page needs only:
app name (`betamacs-config`, pre-filled), channel (`stable`, pre-filled), and
the config content (Import JSON or edit). Publish works with no manual endpoint
setup. (Returning browsers with a previously persisted store config keep it.)

### Build the static bundle

Two independent Vite outputs from the one `webapp/` source, differing only in
base path and output dir (the on-device build is unchanged):

```
cd webapp
npm ci                 # first time (node_modules already present in this repo)

# On-device build (base "/", -> webapp/dist) — bundled into betamacs.app. UNCHANGED.
npm run build

# Store build (base "/betamacs/", -> webapp/dist-store) — for typeserver.
npm run build:store
```

`build:store` runs `tsc --noEmit && vite build --base=/betamacs/ --outDir
dist-store --emptyOutDir`. Then place the bundle where typeserver serves it:

```
rm -rf ../../typeserver/web/betamacs        # path to your typeserver checkout
mkdir -p ../../typeserver/web/betamacs
cp -R dist-store/. ../../typeserver/web/betamacs/
```

`webapp/dist-store` is gitignored in this repo; the built bundle is committed in
the **typeserver** repo under `web/betamacs/` (typeserver serves `./web` at
runtime and its Dockerfile copies `/web` into the image). Re-run these steps and
re-commit typeserver whenever the app changes.

### Build & run typeserver

```
cd typeserver
go build ./...                 # sanity
make typeserver                # builds web + the server binary into ./build
# production image + deploy (from the Makefile):
make commit-docker-build       # commits, pushes, builds the image on the remote
make docker-deploy             # docker compose up -d on the compose host
```

### Environment checklist (publish endpoint)

The publish endpoint uploads **in-process** via the
`github.com/bdstark/otactl/publisher` Go package — **no `otactl` binary
in the image**. It author-signs with the oracle, then does the publisher-mTLS
firmware upload itself, reading the client cert/key/CA from mounted PEM files.
Give it:

| Var | Value / purpose |
|---|---|
| `BETAMACS_AUTHOR_SECRET` | `betamacs author` — typeserver secret holding the author signing key. **In docker-compose list syntax, do NOT quote it** (`- BETAMACS_AUTHOR_SECRET=betamacs author`); `="betamacs author"` passes the quotes literally and the secret name won't match. |
| `BETAMACS_AUTHOR_TTL` | author-signature validity window, seconds (default 3600) |
| `BETAMACS_AUTHOR_PASSPHRASE` | only if that secret is passphrase-protected |
| `OTACTL_BACKEND_URL` | `https://otactl-device.docker.newton.haus` — otactl device origin the upload targets (per-call `backendUrl` overrides; endpoint 400s if neither is set) |
| `BETAMACS_PUBLISHER_CERT` | publisher mTLS cert PEM (default `/pki/publisher-betamacs-config.crt`) |
| `BETAMACS_PUBLISHER_KEY` | publisher mTLS key PEM (default `/pki/publisher-betamacs-config.key`) |
| `BETAMACS_PUBLISHER_CA` | backend CA PEM the device origin's server cert chains to (default `/pki/publisher-betamacs-config-ca.crt`; optional but the private otactl PKI needs it — absent → system trust store → x509 error) |

`OTACTL_BIN` / `OTACTL_PUBLISHER_ID` are no longer used by the endpoint
(`publisherId` is carried in the request; `OTACTL_PUBLISHER_ID` may still be set
and is used only as a fallback for the request field).

**Mount the three PEMs** — they live on the publishing Mac at
`~/Library/Application Support/otactl-boot-usb/publisher-betamacs-config.crt`,
`.key`, and `publisher-betamacs-config-ca.crt`. Drop them into the container's
existing `/pki` mount (or any dir) so the default paths above resolve; no
`$HOME`/`XDG_CONFIG_HOME` gymnastics needed since the endpoint reads explicit
files. The browser needs none of this — it only talks to `/api/betamacs/publish`
on the same origin.

Docker build: typeserver now requires the `github.com/bdstark/otactl`
module; the existing `--mount=type=ssh` in the Dockerfile fetches it (it's a
private GitHub repo, covered by `GOPRIVATE`).

Example compose delta (server `docker-compose.yaml`):

```yaml
    environment:
      - BETAMACS_AUTHOR_SECRET=betamacs author        # no quotes!
      - OTACTL_BACKEND_URL=https://otactl-device.docker.newton.haus
      - BETAMACS_PUBLISHER_CERT=/pki/publisher-betamacs-config.crt
      - BETAMACS_PUBLISHER_KEY=/pki/publisher-betamacs-config.key
      - BETAMACS_PUBLISHER_CA=/pki/publisher-betamacs-config-ca.crt
      # (remove OTACTL_BIN)
    volumes:
      - /path/to/pki:/pki:ro   # must now also contain the 3 publisher-betamacs-config PEMs
```

### Author-key rotation caveat

`author-pubkey.pem` is baked into managed betamacs builds. A config signed by a
**new** author key is only accepted by devices whose betamacs build carries the
matching pinned pubkey. New-key configs need betamacs **>= 0.2.11** on the
device; the fleet is currently on **0.2.16**, so rotation is safe now. If you
rotate the author key, re-publish (re-sign) after the fleet has the build that
pins the new key — a device on an older build would reject the new-key config
and fail closed to strict defaults.

### What v1 does NOT include

- **Read-back** (`GET /api/betamacs/config`) is a documented `501` stub —
  otactl exposes no operator resolve/download API to proxy yet. Load via
  **Import JSON** (paste exported package or author wrapper); the UI degrades
  gracefully.
- **Device assignment** (`/api/betamacs/devices`, `/assign`) are `501` stubs —
  need an otactl operator assignment API. The Devices tab loads against them
  and shows the single-channel caveat.

## Future seams (not built now)

- **Dashboards.** betamacs exposes live status via its daemon/statusframe and
  `/api/status`; hausmeister reports health to otactl. A read-only status view
  slots beside the editor, reading those sources — no change to the store
  interface (status is not config).
- **s3 / file stores.** As above — same `Store` interface, no typeserver
  backend needed, read-back/listing available.
- **Read-back / diff.** Once a store exposes `fetchLatest`, show a diff between
  the staged resolved artifact and what's live before publishing.

---

## What is built vs. documented-next-step

**Phase 1 scaffold (typechecked):**

- `webapp/src/stores/store.ts` — `Store` interface, `StoreRef`,
  `PublishInput`/`PublishResult`/`StoredConfig`, assignment interfaces.
- `webapp/src/stores/otactl.ts` — `OtactlStore` (publish + read) + the
  publish/read request/response types.
- `webapp/src/config/inheritance.ts` — pure inheritance flatten/merge +
  `TimeLayer`/`ResolvedPackage`/`ConfigSource` types + `resolveInheritance`.

**Phase 2 MVP (built, typechecked, uncommitted):**

- `webapp/src/store.ts` — extended: `StoreConfig` (app/channel/backend/
  endpoints, persisted), baseline tracking with `dirty`/`diff`, `importJson`
  (accepts raw package or author wrapper), `loadFromStore`, `publishToStore`
  (resolves via `inheritance.ts`, publishes via `OtactlStore`).
- `webapp/src/components/publish.ts` — the **Store bar**: configure the store,
  Import JSON / Load from store, live dirty/diff indicator, version + Publish.
- `webapp/src/components/assignment.ts` — Devices tab (assignment UI over the
  stub endpoints, single-channel caveat surfaced).
- `webapp/src/modules/clock.ts` — **clockIntegrity** editor (enabled, timezone,
  skew/cadence, ntpServers, timeUrl).
- `webapp/src/components/controls.ts` — new `bm-list` control (string arrays).
- `webapp/src/main.ts` — Store bar front-and-center; Clock/Devices tabs; the
  original live-app push/pull demoted to a collapsed "Live app (dev)" section.
- typeserver `cmd/server/betamacs.go` (+ `main.go` registration) — the
  **publish endpoint**, fully implemented; read/devices/assign endpoints
  registered as `501` stubs.

**Documented next steps (not implemented here):**

- betamacs schema + `resolve()` `timeLayers` support (TS + Rust) — owned by the
  Rust side; the config app already emits `timeLayers` in the resolved artifact.
- Read-back endpoint: needs an otactl operator resolve/download API to proxy.
- Device assignment: otactl operator credentials + assignment API; then
  `/api/betamacs/devices` + `/assign` become functional.
- otactl per-app channel override (for clean per-kid config assignment).
- Inheritance UI (base picker) + time-layer editor onto `inheritance.ts`.
- `s3`/`file` stores; live-status dashboard.
