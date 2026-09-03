// Configuration package contract — mirrors src/settings.rs in the Rust app.
// Effective settings = module defaults <- named-config layers in order <-
// explicit overrides, merged field by field; `triggers` merges per class.

export const NUDENET_CLASSES = [
  "FEMALE_GENITALIA_COVERED",
  "FACE_FEMALE",
  "BUTTOCKS_EXPOSED",
  "FEMALE_BREAST_EXPOSED",
  "FEMALE_GENITALIA_EXPOSED",
  "MALE_BREAST_EXPOSED",
  "ANUS_EXPOSED",
  "FEET_EXPOSED",
  "BELLY_COVERED",
  "FEET_COVERED",
  "ARMPITS_COVERED",
  "ARMPITS_EXPOSED",
  "FACE_MALE",
  "BELLY_EXPOSED",
  "MALE_GENITALIA_EXPOSED",
  "ANUS_COVERED",
  "FEMALE_BREAST_COVERED",
  "BUTTOCKS_COVERED",
] as const;
export type NudenetClass = (typeof NUDENET_CLASSES)[number];

/** UI grouping of trigger classes. */
export const TRIGGER_GROUPS: { label: string; classes: NudenetClass[] }[] = [
  {
    label: "Explicit",
    classes: [
      "FEMALE_GENITALIA_EXPOSED",
      "MALE_GENITALIA_EXPOSED",
      "FEMALE_BREAST_EXPOSED",
      "MALE_BREAST_EXPOSED",
      "BUTTOCKS_EXPOSED",
      "ANUS_EXPOSED",
    ],
  },
  {
    label: "Covered / suggestive",
    classes: [
      "FEMALE_GENITALIA_COVERED",
      "FEMALE_BREAST_COVERED",
      "BUTTOCKS_COVERED",
      "ANUS_COVERED",
      "BELLY_EXPOSED",
      "BELLY_COVERED",
    ],
  },
  {
    label: "Body parts",
    classes: ["ARMPITS_EXPOSED", "ARMPITS_COVERED", "FEET_EXPOSED", "FEET_COVERED"],
  },
  {
    label: "Faces",
    classes: ["FACE_FEMALE", "FACE_MALE"],
  },
];

export interface DetectionSettings {
  model: string;
  confidenceThreshold: number;
  iouThreshold: number;
  minRegionPx: number;
  captureFps: number;
  tileGrid: number;
  holdMs: number;
  borderlineMargin: number;
  debounceCount: number;
  debounceWindowMs: number;
  /** Outline flagged-but-not-blocked detections with their parameters. */
  highlightEnabled: boolean;
  /** Lowest confidence worth highlighting (0..1). */
  highlightFloor: number;
  triggers: Record<string, boolean>;
}

export interface TextOverlay {
  enabled: boolean;
  /** Names of Package.textSets to draw lines from. */
  sets: string[];
  /** Resolved lines (filled by resolve(); ignored on input). */
  lines?: string[];
  fontFamily: string;
  fontSizePt: number;
  fontColor: string;
}

export interface TextSet {
  name: string;
  description?: string;
  lines: string[];
}

export type CensorMode = "box" | "blur" | "mosaic" | "static";
export type BlurKind = "gaussian" | "box" | "average";
export type MosaicSampling = "average" | "gaussian" | "nearest";
export type ColorMap = "none" | "luminance" | "steps";

export interface BlurSettings {
  kind: BlurKind;
  intensity: number;
}

export interface MosaicSettings {
  cellSizePt: number;
  sampling: MosaicSampling;
  map: ColorMap;
  colorLow: string;
  colorHigh: string;
}

export interface StaticSettings {
  densityPct: number;
  speedHz: number;
  grainMm: number;
  colored: boolean;
  colorLow: string;
  colorHigh: string;
}

export interface CensorSettings {
  mode: CensorMode;
  opacityPct: number;
  blur: BlurSettings;
  mosaic: MosaicSettings;
  staticNoise: StaticSettings;
  fillColor: string;
  borderColor: string;
  borderWidth: number;
  xScalePct: number;
  yScalePct: number;
  showTriggerLabel: boolean;
  censorInCaptures: boolean;
  textOverlay: TextOverlay;
}

export type DetectionPatch = Partial<DetectionSettings>;
export type CensorPatch = Partial<CensorSettings>;

export interface ModulePatches {
  detection?: DetectionPatch;
  censor?: CensorPatch;
}

export interface NamedConfig {
  name: string;
  description?: string;
  settings: ModulePatches;
}

export interface Package {
  version: number;
  namedConfigs: NamedConfig[];
  layers: string[];
  overrides: ModulePatches;
  textSets: TextSet[];
}

export interface Effective {
  detection: DetectionSettings;
  censor: CensorSettings;
}

export function defaultDetection(): DetectionSettings {
  const onByDefault = new Set<string>([
    "FEMALE_BREAST_EXPOSED",
    "FEMALE_GENITALIA_EXPOSED",
    "MALE_GENITALIA_EXPOSED",
    "BUTTOCKS_EXPOSED",
    "ANUS_EXPOSED",
  ]);
  return {
    model: "320n",
    confidenceThreshold: 0.35,
    iouThreshold: 0.45,
    minRegionPx: 0,
    captureFps: 4,
    tileGrid: 2,
    holdMs: 1500,
    borderlineMargin: 0.1,
    debounceCount: 2,
    debounceWindowMs: 3000,
    highlightEnabled: false,
    highlightFloor: 0.15,
    triggers: Object.fromEntries(NUDENET_CLASSES.map((c) => [c, onByDefault.has(c)])),
  };
}

export function defaultCensor(): CensorSettings {
  return {
    mode: "box",
    opacityPct: 100,
    blur: { kind: "gaussian", intensity: 16 },
    mosaic: {
      cellSizePt: 16,
      sampling: "average",
      map: "none",
      colorLow: "#000000",
      colorHigh: "#ffffff",
    },
    staticNoise: {
      densityPct: 60,
      speedHz: 12,
      grainMm: 1,
      colored: false,
      colorLow: "#000000",
      colorHigh: "#ffffff",
    },
    fillColor: "#000000",
    borderColor: "#000000",
    borderWidth: 0,
    xScalePct: 130,
    yScalePct: 130,
    showTriggerLabel: false,
    censorInCaptures: false,
    textOverlay: {
      enabled: false,
      sets: [],
      fontFamily: "Helvetica",
      fontSizePt: 18,
      fontColor: "#ffffff",
    },
  };
}

export function emptyPackage(): Package {
  return { version: 1, namedConfigs: [], layers: [], overrides: {}, textSets: [] };
}

/** Pool the lines of the referenced text sets, in reference order. */
export function resolveTextLines(pkg: Package, overlay: TextOverlay): string[] {
  return overlay.sets
    .map((name) => pkg.textSets.find((s) => s.name === name))
    .filter((s): s is TextSet => !!s)
    .flatMap((s) => s.lines);
}

function applyDetection(base: DetectionSettings, patch: DetectionPatch): void {
  const { triggers, ...rest } = patch;
  Object.assign(base, stripUndefined(rest));
  if (triggers) Object.assign(base.triggers, triggers);
}

function applyCensor(base: CensorSettings, patch: CensorPatch): void {
  Object.assign(base, stripUndefined(patch));
}

function stripUndefined<T extends object>(o: T): Partial<T> {
  return Object.fromEntries(
    Object.entries(o).filter(([, v]) => v !== undefined),
  ) as Partial<T>;
}

/** Same resolution the Rust app performs. */
export function resolve(pkg: Package): Effective {
  const effective: Effective = { detection: defaultDetection(), censor: defaultCensor() };
  const layerPatches = pkg.layers
    .map((name) => pkg.namedConfigs.find((c) => c.name === name)?.settings)
    .filter((s): s is ModulePatches => !!s);
  for (const patches of [...layerPatches, pkg.overrides]) {
    if (patches.detection) applyDetection(effective.detection, patches.detection);
    if (patches.censor) applyCensor(effective.censor, patches.censor);
  }
  effective.censor.textOverlay.lines = resolveTextLines(pkg, effective.censor.textOverlay);
  return effective;
}

/** Where a resolved value came from, for UI hinting. */
export function valueSource(
  pkg: Package,
  module: keyof ModulePatches,
  field: string,
): string {
  const has = (p?: ModulePatches) =>
    p?.[module] !== undefined && (p[module] as Record<string, unknown>)[field] !== undefined;
  if (has(pkg.overrides)) return "override";
  for (let i = pkg.layers.length - 1; i >= 0; i--) {
    const cfg = pkg.namedConfigs.find((c) => c.name === pkg.layers[i]);
    if (has(cfg?.settings)) return cfg!.name;
  }
  return "default";
}
