// Store abstraction: an arbitrary config storage location/instance, addressed
// by (app name, channel) plus impl-specific connection info.
//
// Design constraint 1: implement ONE store now (otactl, see ./otactl.ts) and
// leave clean extension points for future `s3` and `file` stores (interface
// only, below).
//
// Design constraint 4: a store holds the FULLY-RESOLVED (inheritance-flattened,
// author-signed) config. The store never merges at retrieval — it is
// content-agnostic bytes, and the author signature must cover exactly what the
// device runs. Merging/flattening happens in ../config/inheritance.ts before
// publish.

import type { ResolvedPackage } from "../config/inheritance.js";

export type StoreKind = "otactl" | "s3" | "file";

/** Address of a config instance within a store: which app, which channel. */
export interface StoreRef {
  /** otactl app name, e.g. "betamacs-config" or "betamacs-tasks". */
  app: string;
  /** Release lane, e.g. "stable" or a per-kid channel. See the single-channel
   *  caveat in docs/config-app.md — one otactl device.Channel governs ALL
   *  apps on that device. */
  channel: string;
}

export interface PublishInput {
  ref: StoreRef;
  /** Inheritance-flattened, time-layers-retained bytes to store. */
  resolved: ResolvedPackage;
  /** Artifact version string (otactl requires one; monotonic epoch is server
   *  side). Typically derived from the config version + a build stamp. */
  version: string;
  /** Author-signature validity window (see BETAMACS_AUTHOR_TTL). Default 3600. */
  ttlSeconds?: number;
  /** Human note carried into the store's audit trail, when supported. */
  note?: string;
}

export interface PublishResult {
  ref: StoreRef;
  version: string;
  /** sha256 the store bound to the stored artifact. */
  sha256: string;
  /** Monotonic anti-rollback epoch, when the store assigns one (otactl does). */
  epoch?: number;
  /** Expiry of the author signature (ISO-8601), when signed. */
  notAfter?: string;
  storedAt: string;
}

/** A resolved config read back from a store. Retrieval returns bytes as-is;
 *  the store does no merging. */
export interface StoredConfig {
  ref: StoreRef;
  version: string;
  resolved: ResolvedPackage;
  sha256: string;
  epoch?: number;
}

/** A place configs live. `publish` is mandatory; reads/listing are optional
 *  because not every backend exposes them to a browser (otactl reads are
 *  device-mTLS gated — see the impl). */
export interface Store {
  readonly kind: StoreKind;
  /** Short, human-readable identity for status/logging. */
  describe(): string;
  /** Write a signed, resolved config to (app, channel). */
  publish(input: PublishInput): Promise<PublishResult>;
  /** Read back the current artifact for a ref, when the backend permits. */
  fetchLatest?(ref: StoreRef): Promise<StoredConfig | undefined>;
  /** Enumerate known channels for an app, when the backend permits. */
  listChannels?(app: string): Promise<string[]>;
}

// ------------------------------------------------- assignment (constraint 6)
// Config->device assignment as a strongly-typed layer over otactl's device
// API. Interface only for now (users-as-targets are future); the caveat is
// documented in docs/config-app.md: otactl resolves a SINGLE device.Channel
// for ALL apps, with no per-app override and only the server default as
// fallback. So "assign config X to device D" means "set D.Channel = X's
// channel", which also moves D's app/tasks lanes.

export interface DeviceTarget {
  deviceId: string;
  label?: string;
}

export interface DeviceAssignment {
  target: DeviceTarget;
  /** The single channel this device follows across every app. */
  channel: string;
}

/** Optional capability a store impl (or a sibling service) may provide to read
 *  and set device channel assignments. Kept separate from Store because
 *  assignment is an otactl device-API concern, not a config-bytes concern. */
export interface AssignmentApi {
  listDevices(): Promise<DeviceTarget[]>;
  getAssignment(deviceId: string): Promise<DeviceAssignment | undefined>;
  /** Sets device.Channel. NOTE: affects all apps on the device (see caveat). */
  setChannel(deviceId: string, channel: string): Promise<void>;
}
