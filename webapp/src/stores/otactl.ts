// otactl store: publishes a signed, resolved config into otactl's firmware
// pipeline, from where hausmeister's BetamacsPlugin delivers it to entitled
// Macs (docs/managed-mode.md).
//
// WHY THIS DELEGATES (design constraint 3): a browser CANNOT publish to otactl
// directly. Two credentials are required that no browser can present:
//   1. the publisher CLIENT CERT (mTLS) otactl's upload endpoint demands, and
//   2. the typeserver `ts_session` needed to author-sign via the signing
//      oracle (POST /api/secrets/sign — the author key never leaves the server).
// So this impl POSTs the resolved bytes to a small typeserver BACKEND endpoint
// that holds the publisher identity, author-signs, and performs the mTLS
// upload (equivalent to scripts/publish.sh, but server-side and web-triggered).
//
// The frontend may be hosted anywhere, but publish is inert without that
// endpoint. See docs/config-app.md "typeserver publish backend" for the
// endpoint contract this file mirrors.

import type {
  PublishInput,
  PublishResult,
  Store,
  StoreKind,
  StoreRef,
  StoredConfig,
} from "./store.js";

export interface OtactlStoreConfig {
  /** typeserver backend that owns the publisher cert + signing oracle, e.g.
   *  "https://typeserver.docker.newton.haus/api/betamacs/publish". */
  publishEndpoint: string;
  /** otactl device-origin URL the backend uploads to (maps to
   *  `otactl boot-usb upload --backend-url`). */
  backendUrl: string;
  /** Publisher identity for otactl mTLS + audit (--publisher-id). */
  publisherId: string;
  /** Target architecture; otactl artifacts are arch-scoped. Default arm64. */
  arch?: string;
  /** typeserver secret name of the author signing key (BETAMACS_AUTHOR_SECRET).
   *  The backend may pin this itself; sent so one endpoint can serve several. */
  authorSecret?: string;
  /** typeserver GET endpoint that reads back the current published config.
   *  Reads route through typeserver too: otactl's resolve/download is device-
   *  mTLS gated, unreachable from a browser. Empty = read unsupported. */
  readEndpoint?: string;
}

/** Read response from GET {readEndpoint}?app=&channel=&arch= (see
 *  docs/config-app.md). The backend fetches the current otactl artifact,
 *  unwraps the author signature, and returns the resolved package. */
export interface OtactlReadResponse {
  version: string;
  sha256: string;
  epoch?: number;
  package: unknown; // the ResolvedPackage
}

// ------------------------------------------------ publish backend contract
// Request/response shapes for POST {publishEndpoint}. Documented in full in
// docs/config-app.md; kept in sync here so callers are typed.

export interface OtactlPublishRequest {
  app: string; // ref.app, e.g. "betamacs-config"
  channel: string; // ref.channel
  arch: string; // e.g. "arm64"
  version: string; // artifact version string
  publisherId: string;
  backendUrl: string;
  ttlSeconds: number; // author-signature validity window
  authorSecret?: string;
  note?: string;
  /** The raw, fully-resolved package the backend must author-sign VERBATIM.
   *  The backend must not merge, reorder, or re-derive — it signs these bytes
   *  and uploads, so the signature covers exactly what the device runs. */
  package: unknown;
}

export interface OtactlPublishResponse {
  version: string;
  sha256: string;
  epoch?: number;
  notAfter?: string;
  storedAt: string;
}

export class OtactlStore implements Store {
  readonly kind: StoreKind = "otactl";

  constructor(private readonly cfg: OtactlStoreConfig) {}

  describe(): string {
    return `otactl:${this.cfg.publisherId} -> ${this.cfg.backendUrl}`;
  }

  async publish(input: PublishInput): Promise<PublishResult> {
    const body: OtactlPublishRequest = {
      app: input.ref.app,
      channel: input.ref.channel,
      arch: this.cfg.arch ?? "arm64",
      version: input.version,
      publisherId: this.cfg.publisherId,
      backendUrl: this.cfg.backendUrl,
      ttlSeconds: input.ttlSeconds ?? 3600,
      authorSecret: this.cfg.authorSecret,
      note: input.note,
      package: input.resolved,
    };

    // credentials:"include" sends the browser's typeserver ts_session cookie
    // to the (same-site) backend, which uses it to reach the signing oracle.
    const res = await fetch(this.cfg.publishEndpoint, {
      method: "POST",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!res.ok) {
      throw new Error(`otactl publish failed: ${res.status} ${await res.text()}`);
    }
    const out = (await res.json()) as OtactlPublishResponse;
    return {
      ref: input.ref,
      version: out.version,
      sha256: out.sha256,
      epoch: out.epoch,
      notAfter: out.notAfter,
      storedAt: out.storedAt,
    };
  }

  // Reads route through the typeserver read endpoint: otactl's resolve/download
  // authenticates by DEVICE mTLS, unreachable from a browser. When no read
  // endpoint is configured this returns undefined (use Import JSON instead).
  async fetchLatest(ref: StoreRef): Promise<StoredConfig | undefined> {
    if (!this.cfg.readEndpoint) return undefined;
    const url = new URL(this.cfg.readEndpoint);
    url.searchParams.set("app", ref.app);
    url.searchParams.set("channel", ref.channel);
    url.searchParams.set("arch", this.cfg.arch ?? "arm64");
    const res = await fetch(url.toString(), { credentials: "include" });
    if (res.status === 404) return undefined;
    if (!res.ok) {
      throw new Error(`otactl read failed: ${res.status} ${await res.text()}`);
    }
    const out = (await res.json()) as OtactlReadResponse;
    return {
      ref,
      version: out.version,
      sha256: out.sha256,
      epoch: out.epoch,
      resolved: out.package as StoredConfig["resolved"],
    };
  }
}
