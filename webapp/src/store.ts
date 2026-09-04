// Package state + connections. Persisted in localStorage so the externally
// hosted copy of the web app keeps its draft, its on-device connection, and
// its store selection.
//
// Two distinct back-ends live here, on purpose:
//   1. The running-app connection (Connection / pull / push) — the ORIGINAL
//      dev affordance: edit against one live betamacs over /api/package.
//   2. The Store (StoreConfig / loadFromStore / publishToStore) — the NEW
//      fleet path: stage locally, publish a signed, resolved config into
//      otactl via the typeserver backend (docs/config-app.md).

import { emptyPackage, resolve, type Effective, type Package } from "./schema.js";
import { resolveInheritance, type ResolvedPackage } from "./config/inheritance.js";
import { OtactlStore, type OtactlStoreConfig } from "./stores/otactl.js";
import type { PublishResult } from "./stores/store.js";

const PKG_KEY = "betamacs.package";
const CONN_KEY = "betamacs.connection";
const STORE_KEY = "betamacs.storeConfig";
const BASELINE_KEY = "betamacs.baseline";

export interface Connection {
  url: string;
  token: string;
}

/** A configured store instance: which app/channel, which otactl backend, and
 *  the typeserver endpoints that broker publish/read (the browser can't reach
 *  otactl directly — see docs/config-app.md). */
export interface StoreConfig {
  app: string; // "betamacs-config"
  channel: string; // "stable"
  arch: string; // "arm64"
  backendUrl: string; // otactl device-origin URL
  publisherId: string; // "betamacs-config"
  /** typeserver POST endpoint that signs + mTLS-uploads. */
  publishEndpoint: string;
  /** typeserver GET endpoint that reads back the current published config. */
  readEndpoint: string;
}

export function defaultStoreConfig(): StoreConfig {
  // Default to SAME-ORIGIN typeserver endpoints so a page served from
  // typeserver at /betamacs/ needs zero endpoint config — its publish/read
  // calls (credentials:"include") hit the same origin that served it, and the
  // ts_session cookie authorizes them. publisherId/backendUrl are left blank:
  // the server holds the publisher identity + otactl backend URL (its own
  // env), so the browser never needs the publisher/mTLS details.
  const origin = typeof location !== "undefined" ? location.origin : "";
  return {
    app: "betamacs-config",
    channel: "stable",
    arch: "arm64",
    backendUrl: "", // server default (OTACTL_BACKEND_URL)
    publisherId: "", // server default (OTACTL_PUBLISHER_ID)
    publishEndpoint: origin ? `${origin}/api/betamacs/publish` : "",
    readEndpoint: origin ? `${origin}/api/betamacs/config` : "",
  };
}

type Listener = () => void;

class Store {
  pkg: Package;
  /** The last loaded/published package, to diff staged edits against. */
  baseline: Package | null;
  connection: Connection;
  storeConfig: StoreConfig;
  status = "";
  /** True when the app reports fleet-managed mode: pushes are refused. */
  managed = false;
  private listeners = new Set<Listener>();

  constructor() {
    this.pkg = readJson<Package>(PKG_KEY) ?? emptyPackage();
    // Migrate drafts saved before textSets existed.
    this.pkg.textSets ??= [];
    this.baseline = readJson<Package>(BASELINE_KEY) ?? null;
    this.storeConfig = { ...defaultStoreConfig(), ...(readJson<StoreConfig>(STORE_KEY) ?? {}) };
    this.connection =
      readJson<Connection>(CONN_KEY) ?? { url: defaultAppUrl(), token: "" };
    // Accept a token handed over in the URL fragment (the app's menu bar
    // "Open Settings…" deep link), persist it, and scrub it from the bar.
    const handoff = location.hash.match(/token=([0-9a-f]+)/)?.[1];
    if (handoff) {
      this.connection = { ...this.connection, token: handoff };
      localStorage.setItem(CONN_KEY, JSON.stringify(this.connection));
      history.replaceState(null, "", location.pathname + location.search);
    }
  }

  get effective(): Effective {
    return resolve(this.pkg);
  }

  subscribe(fn: Listener): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  update(mutate: (pkg: Package) => void): void {
    mutate(this.pkg);
    localStorage.setItem(PKG_KEY, JSON.stringify(this.pkg));
    this.notify();
  }

  setConnection(conn: Connection): void {
    this.connection = conn;
    localStorage.setItem(CONN_KEY, JSON.stringify(conn));
    this.notify();
  }

  setStoreConfig(cfg: StoreConfig): void {
    this.storeConfig = cfg;
    localStorage.setItem(STORE_KEY, JSON.stringify(cfg));
    this.notify();
  }

  setStatus(status: string): void {
    this.status = status;
    this.notify();
  }

  private notify(): void {
    for (const fn of this.listeners) fn();
  }

  private headers(): HeadersInit {
    return {
      "content-type": "application/json",
      authorization: `Bearer ${this.connection.token}`,
    };
  }

  // ------------------------------------------------------- baseline / diff

  /** Snapshot the current package as the clean baseline (after load/publish). */
  markBaseline(): void {
    this.baseline = deepClone(this.pkg);
    localStorage.setItem(BASELINE_KEY, JSON.stringify(this.baseline));
    this.notify();
  }

  /** True when staged edits differ from the loaded/published baseline. */
  get dirty(): boolean {
    if (!this.baseline) return isNonEmptyPackage(this.pkg);
    return stableStringify(this.pkg) !== stableStringify(this.baseline);
  }

  /** Dotted field paths that differ from the baseline, for the diff panel. */
  diff(): string[] {
    const base = this.baseline ?? emptyPackage();
    return diffPaths(base as unknown, this.pkg as unknown, "");
  }

  /** The bytes that would be published: inheritance-flattened, time-retained.
   *  With no base config in this MVP, this collapses the package's layer stack
   *  into a single overrides block (identical Effective on-device). */
  resolvedArtifact(): ResolvedPackage {
    return resolveInheritance(
      { name: this.storeConfig.app, package: this.pkg },
      () => undefined,
    );
  }

  private otactlStore(): OtactlStore {
    const cfg: OtactlStoreConfig = {
      publishEndpoint: this.storeConfig.publishEndpoint,
      backendUrl: this.storeConfig.backendUrl,
      publisherId: this.storeConfig.publisherId,
      arch: this.storeConfig.arch,
      readEndpoint: this.storeConfig.readEndpoint,
    };
    return new OtactlStore(cfg);
  }

  // ------------------------------------------------------- import / load

  /** Load a package from pasted/exported JSON. Accepts either a raw package or
   *  an author-signed wrapper ({packageB64,...}) and unwraps it. Sets the
   *  baseline so subsequent edits diff against what was imported. */
  importJson(text: string): void {
    const parsed = JSON.parse(text) as unknown;
    const pkg = coerceToPackage(parsed);
    this.update((p) => Object.assign(p, emptyPackage(), pkg));
    this.markBaseline();
    this.setStatus("imported config (baseline set)");
  }

  /** Read the current published config for the selected store, via the
   *  typeserver read endpoint (the browser cannot reach otactl directly). */
  async loadFromStore(): Promise<{ version: string; sha256: string }> {
    if (!this.storeConfig.readEndpoint) {
      throw new Error("no read endpoint configured (Import JSON instead)");
    }
    const stored = await this.otactlStore().fetchLatest({
      app: this.storeConfig.app,
      channel: this.storeConfig.channel,
    });
    if (!stored) throw new Error("no published config found for this store");
    this.update((p) => Object.assign(p, emptyPackage(), stored.resolved));
    this.markBaseline();
    return { version: stored.version, sha256: stored.sha256 };
  }

  // ------------------------------------------------------- publish

  /** Publish the resolved artifact to the store through the typeserver backend. */
  async publishToStore(version: string, ttlSeconds?: number): Promise<PublishResult> {
    const result = await this.otactlStore().publish({
      ref: { app: this.storeConfig.app, channel: this.storeConfig.channel },
      resolved: this.resolvedArtifact(),
      version,
      ttlSeconds,
    });
    this.markBaseline();
    return result;
  }

  // ------------------------------------------------- running-app (dev) path

  /** Ask the app whether it is fleet-managed (read-only UI). */
  async probeManaged(): Promise<void> {
    try {
      const res = await fetch(`${this.connection.url}/api/status`, {
        headers: this.headers(),
      });
      if (res.ok) {
        const status = (await res.json()) as { managed?: boolean };
        this.managed = !!status.managed;
        this.notify();
      }
    } catch {
      // Unreachable app: leave the last known state.
    }
  }

  /** Pull the package currently stored in the app. */
  async pull(): Promise<void> {
    const res = await fetch(`${this.connection.url}/api/package`, {
      headers: this.headers(),
    });
    if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
    const pkg = (await res.json()) as Package;
    this.update((p) => Object.assign(p, pkg));
    this.markBaseline();
  }

  /** Push the local package to the app; applied live there. */
  async push(): Promise<Effective> {
    const res = await fetch(`${this.connection.url}/api/package`, {
      method: "PUT",
      headers: this.headers(),
      body: JSON.stringify(this.pkg),
    });
    if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
    return (await res.json()) as Effective;
  }
}

// --------------------------------------------------------------- helpers

function readJson<T>(key: string): T | undefined {
  try {
    const raw = localStorage.getItem(key);
    return raw ? (JSON.parse(raw) as T) : undefined;
  } catch {
    return undefined;
  }
}

function defaultAppUrl(): string {
  if (location.hostname === "127.0.0.1" || location.hostname === "localhost") {
    return location.origin;
  }
  return "http://127.0.0.1:8787";
}

function deepClone<T>(v: T): T {
  return JSON.parse(JSON.stringify(v)) as T;
}

function isNonEmptyPackage(p: Package): boolean {
  return (
    p.namedConfigs.length > 0 ||
    p.layers.length > 0 ||
    p.textSets.length > 0 ||
    Object.keys(p.overrides).length > 0
  );
}

/** Accept a raw Package or an author-signed wrapper and return a Package. */
function coerceToPackage(parsed: unknown): Partial<Package> {
  if (parsed && typeof parsed === "object" && "packageB64" in parsed) {
    const b64 = (parsed as { packageB64: string }).packageB64;
    return JSON.parse(atob(b64)) as Partial<Package>;
  }
  return parsed as Partial<Package>;
}

/** Deterministic JSON (sorted keys) so dirty-detection ignores key order. */
function stableStringify(v: unknown): string {
  return JSON.stringify(sortKeys(v));
}

function sortKeys(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(sortKeys);
  if (v && typeof v === "object") {
    const out: Record<string, unknown> = {};
    for (const k of Object.keys(v as Record<string, unknown>).sort()) {
      out[k] = sortKeys((v as Record<string, unknown>)[k]);
    }
    return out;
  }
  return v;
}

/** Recursive diff producing dotted paths where two objects differ. */
function diffPaths(a: unknown, b: unknown, prefix: string): string[] {
  if (stableStringify(a) === stableStringify(b)) return [];
  const aObj = a && typeof a === "object" && !Array.isArray(a);
  const bObj = b && typeof b === "object" && !Array.isArray(b);
  if (!aObj || !bObj) return [prefix || "(root)"];
  const keys = new Set([
    ...Object.keys(a as Record<string, unknown>),
    ...Object.keys(b as Record<string, unknown>),
  ]);
  const out: string[] = [];
  for (const k of keys) {
    const path = prefix ? `${prefix}.${k}` : k;
    out.push(
      ...diffPaths(
        (a as Record<string, unknown>)[k],
        (b as Record<string, unknown>)[k],
        path,
      ),
    );
  }
  return out;
}

export const store = new Store();
