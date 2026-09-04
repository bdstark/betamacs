// Compile-time inheritance flattening + the resolved-artifact model.
//
// Two axes of variation, resolved at DIFFERENT times (design constraint 5):
//
//   Inheritance (base config + child deltas)  -> flattened HERE, at publish.
//       A base change means re-flatten + re-sign + re-publish every child.
//       The device never sees the base; it runs the merged bytes.
//
//   Time      (a policy that differs by day/hour) -> NOT flattened.
//       Time-predicated layers travel INTO the resolved artifact and are
//       resolved at runtime by the betamacs agent's clock. The stored,
//       author-signed bytes therefore cover every time-variant the device
//       may run — the signature is over exactly what the device runs.
//
// This module is pure (no DOM, no lit) so it can run in the browser at
// publish time and, once the mirror lands in Rust, be validated on-device.

import type {
  DetectionPatch,
  ModulePatches,
  Package,
  TextSet,
} from "../schema.js";

// --------------------------------------------------------------- new types
// Kept here rather than in schema.ts so the existing editor/modules are
// untouched; the betamacs Rust `Package`/`resolve()` gains the mirror of
// `timeLayers` as a documented follow-up (see docs/config-app.md).

/** When a time-layer is active. Mirrors the existing `Schedule` shape so the
 *  agent can reuse its schedule evaluator. Local clock, lowercase weekdays. */
export interface TimePredicate {
  days: string[]; // "mon".."sun"; empty = every day
  from: string; // "HH:MM" local, inclusive
  to: string; // "HH:MM" local, exclusive; may wrap past midnight
}

/** A patch the agent applies ONLY while `when` is active. Retained verbatim
 *  in the resolved artifact — never flattened into `overrides`. */
export interface TimeLayer {
  name: string;
  when: TimePredicate;
  settings: ModulePatches;
}

/** An authored, editable config document (localStorage-staged). It may
 *  inherit from another source by name; its `package` is a normal betamacs
 *  Package (defaults <- namedConfig layers <- overrides). */
export interface ConfigSource {
  /** Identity of this config; also the store address's config/app name. */
  name: string;
  /** Optional parent to inherit from (flattened at publish). */
  base?: string;
  package: Package;
  /** Time-predicated layers; retained in the resolved artifact. */
  timeLayers?: TimeLayer[];
}

/** What gets stored + signed: an inheritance-flattened Package with the
 *  time-layers still attached. A device with a plain (time-unaware) resolve
 *  ignores `timeLayers` and runs the base; a time-aware agent overlays the
 *  active layers by clock. */
export interface ResolvedPackage extends Package {
  timeLayers?: TimeLayer[];
}

// -------------------------------------------------------------- merge core

/** Expand a package into its ordered patch sequence: each referenced layer's
 *  patches in `layers` order, then the explicit overrides last. Identical to
 *  the order betamacs `resolve()` applies them onto module defaults. */
export function orderedPatches(pkg: Package): ModulePatches[] {
  const layerPatches = pkg.layers
    .map((name) => pkg.namedConfigs.find((c) => c.name === name)?.settings)
    .filter((s): s is ModulePatches => !!s);
  return [...layerPatches, pkg.overrides];
}

/** Last-write-wins per field. Undefined values never clobber. */
function mergeShallow<T>(
  base: Partial<T> | undefined,
  patch: Partial<T> | undefined,
): Partial<T> | undefined {
  if (!base && !patch) return undefined;
  const out: Record<string, unknown> = { ...(base ?? {}) };
  if (patch) {
    for (const [k, v] of Object.entries(patch)) {
      if (v !== undefined) out[k] = v;
    }
  }
  return out as Partial<T>;
}

/** Detection is special: `triggers` accumulates per class (matching
 *  `applyDetection` in schema.ts), every other field is last-write-wins. */
function mergeDetection(
  base: DetectionPatch | undefined,
  patch: DetectionPatch | undefined,
): DetectionPatch | undefined {
  if (!base && !patch) return undefined;
  const out: Record<string, unknown> = { ...(base ?? {}) };
  delete out.triggers;
  const triggers: Record<string, boolean> = { ...(base?.triggers ?? {}) };
  if (patch) {
    for (const [k, v] of Object.entries(patch)) {
      if (k === "triggers" || v === undefined) continue;
      out[k] = v;
    }
    if (patch.triggers) Object.assign(triggers, patch.triggers);
  }
  if (Object.keys(triggers).length > 0) out.triggers = triggers;
  return out as DetectionPatch;
}

/** Collapse a whole ordered patch sequence into ONE ModulePatches.
 *
 *  Safe because betamacs `resolve()` applies every field independently and
 *  last-write-wins (triggers accumulate per class): applying the collapsed
 *  patch to module defaults yields the exact same `Effective` as applying
 *  the sequence in order. This is what makes the stored artifact a single
 *  flat `overrides` block instead of a re-derivable layer stack. */
export function combineModulePatches(seq: ModulePatches[]): ModulePatches {
  // Iterate module keys generically off the union present in the sequence, so
  // any module added to ModulePatches is folded in without editing this
  // function. `detection` is the only one with non-shallow (per-trigger) merge.
  const merged: Record<string, unknown> = {};
  for (const p of seq) {
    for (const key of Object.keys(p) as (keyof ModulePatches)[]) {
      const patch = p[key];
      if (!patch) continue;
      if (key === "detection") {
        const next = mergeDetection(
          merged.detection as DetectionPatch | undefined,
          patch as DetectionPatch,
        );
        if (next) merged.detection = next;
      } else {
        const next = mergeShallow(
          merged[key] as Record<string, unknown> | undefined,
          patch as Record<string, unknown>,
        );
        if (next) merged[key] = next;
      }
    }
  }
  return merged as ModulePatches;
}

// --------------------------------------------------------- inheritance API

/** Flatten a root->leaf chain of config sources into a single resolved
 *  package. Later sources win (child deltas over base). Text sets merge by
 *  name (leaf wins); time-layers concatenate (base first, so an active leaf
 *  layer wins over an active base layer at the same clock). */
export function flattenChain(chain: ConfigSource[]): ResolvedPackage {
  const seq: ModulePatches[] = [];
  const textSets = new Map<string, TextSet>();
  const timeLayers: TimeLayer[] = [];
  let version = 1;
  for (const src of chain) {
    seq.push(...orderedPatches(src.package));
    for (const ts of src.package.textSets) textSets.set(ts.name, ts);
    if (src.timeLayers) timeLayers.push(...src.timeLayers);
    version = src.package.version;
  }
  const resolved: ResolvedPackage = {
    version,
    namedConfigs: [],
    layers: [],
    overrides: combineModulePatches(seq),
    textSets: [...textSets.values()],
  };
  if (timeLayers.length > 0) resolved.timeLayers = timeLayers;
  return resolved;
}

/** Walk `leaf`'s base chain via `lookup`, guarding cycles and missing bases,
 *  then flatten. `lookup` resolves a config name to its source (from the
 *  store, or a local library of drafts). */
export function resolveInheritance(
  leaf: ConfigSource,
  lookup: (name: string) => ConfigSource | undefined,
): ResolvedPackage {
  const chain: ConfigSource[] = [];
  const seen = new Set<string>();
  let cur: ConfigSource | undefined = leaf;
  while (cur) {
    if (seen.has(cur.name)) {
      throw new Error(`inheritance cycle through "${cur.name}"`);
    }
    seen.add(cur.name);
    chain.unshift(cur); // root ends up first
    if (!cur.base) break;
    const parent = lookup(cur.base);
    if (!parent) {
      throw new Error(`base config "${cur.base}" of "${cur.name}" not found`);
    }
    cur = parent;
  }
  return flattenChain(chain);
}
