// Package state + connection to the app. Persisted in localStorage so the
// externally-hosted copy of the web app keeps its draft and connection.

import { emptyPackage, resolve, type Effective, type Package } from "./schema.js";

const PKG_KEY = "betamacs.package";
const CONN_KEY = "betamacs.connection";

export interface Connection {
  url: string;
  token: string;
}

type Listener = () => void;

class Store {
  pkg: Package;
  connection: Connection;
  status = "";
  private listeners = new Set<Listener>();

  constructor() {
    this.pkg = readJson<Package>(PKG_KEY) ?? emptyPackage();
    // Migrate drafts saved before textSets existed.
    this.pkg.textSets ??= [];
    this.connection =
      readJson<Connection>(CONN_KEY) ?? { url: defaultAppUrl(), token: "" };
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

  /** Pull the package currently stored in the app. */
  async pull(): Promise<void> {
    const res = await fetch(`${this.connection.url}/api/package`, {
      headers: this.headers(),
    });
    if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
    const pkg = (await res.json()) as Package;
    this.update((p) => Object.assign(p, pkg));
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

function readJson<T>(key: string): T | undefined {
  try {
    const raw = localStorage.getItem(key);
    return raw ? (JSON.parse(raw) as T) : undefined;
  } catch {
    return undefined;
  }
}

function defaultAppUrl(): string {
  // Hosted by the app itself -> same origin; hosted externally -> the
  // app's default local port.
  if (location.hostname === "127.0.0.1" || location.hostname === "localhost") {
    return location.origin;
  }
  return "http://127.0.0.1:8787";
}

export const store = new Store();
