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
  /** Master switch: when false nothing is scanned or censored. */
  enabled: boolean;
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

export type CensorMode = "box" | "blur" | "mosaic" | "static" | "image";
export type BlurKind = "gaussian" | "box" | "average";
export type MosaicSampling = "average" | "gaussian" | "nearest";
export type ColorMap = "none" | "luminance" | "steps";
export type ImageFit = "stretch" | "contain" | "cover";

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

export interface ImageSettings {
  /** Base64 (std) of the image bytes, carried in the config. */
  data: string;
  /** Absolute path to an image file (local/dev use; ignored when data is set). */
  path: string;
  fit: ImageFit;
}

export interface CensorSettings {
  mode: CensorMode;
  opacityPct: number;
  blur: BlurSettings;
  mosaic: MosaicSettings;
  staticNoise: StaticSettings;
  image: ImageSettings;
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

// ------------------------------------------------------- activity challenge
// Policy lives here (betamacs-config); the task bank is a SEPARATE
// betamacs-tasks artifact (TaskBank), versioned/swapped independently.

export type Answer =
  | { type: "number"; value: number; tolerance?: number }
  | { type: "text"; value?: string; anyOf?: string[]; ignoreCase?: boolean }
  | { type: "line"; value: string }
  | { type: "choice"; options: string[]; value: string };

export interface Task {
  id: string;
  category: string;
  grade: number;
  weight?: number;
  prompt: string;
  hint?: string;
  answer: Answer;
  /** Salted answer hashes emitted by `publish.sh tasks`; present in the
   * shipped bank, absent in authored source. */
  answerHash?: string[];
}

/** The betamacs-tasks artifact: standalone, independently versioned. */
export interface TaskBank {
  version: number;
  name?: string;
  tasks: Task[];
}

export interface ChallengeSettings {
  enabled: boolean;
  intervalMinSec: number;
  intervalMaxSec: number;
  categories: string[];
  maxGrade: number;
  answerWindowSec: number;
  maxAttempts: number;
}
export type ChallengePatch = Partial<ChallengeSettings>;

// -------------------------------------------------------- exposure budget

export type ExposureMetric =
  | "events"
  | "activeSeconds"
  | "boxSeconds"
  | "areaSeconds";

export interface ExposureSettings {
  enabled: boolean;
  metric: ExposureMetric;
  warnThreshold: number;
  warnWindowSec: number;
  blockThreshold: number;
  blockWindowSec: number;
  penaltySec: number;
  warnCooldownSec: number;
}
export type ExposurePatch = Partial<ExposureSettings>;

// ------------------------------------------------------------- earned time
// A gate: during a scheduled window the internet is locked until the user
// has earned credit by active time on an allowlisted site/app. Bankable.
// Policy only (see docs/earned-time.md); disabled by default.

export interface Schedule {
  days: string[]; // lowercase "mon".."sun"
  from: string; // "HH:MM" local
  to: string;
}

export interface SourceMatch {
  bundleId?: string;
  browserHostSuffix?: string;
}

export interface EarnSource {
  name: string;
  match: SourceMatch;
  earnRatio: number;
}

export interface EarnedTimeSettings {
  enabled: boolean;
  schedule: Schedule[];
  sources: EarnSource[];
  spendRatio: number;
  dailyEarnCapMin: number;
  maxBankMin: number;
  minSessionMin: number;
  idleTimeoutSec: number;
}
export type EarnedTimePatch = Partial<EarnedTimeSettings>;

// -------------------------------------------------------------- focus limit
// Auto-lockout for staying actively on one browser tab too long (active
// scrolling; idle/video is exempt). Policy only; kids-only via the task bank.
export interface FocusLimitSettings {
  enabled: boolean;
  sameTabLimitMin: number;
  lockoutMin: number;
  idleResetSec: number;
  whitelistHosts: string[]; // exempt (never trigger)
  blacklistHosts: string[]; // if non-empty, only these are monitored
}
export type FocusLimitPatch = Partial<FocusLimitSettings>;

// Trust the clock behind all time-of-day policy: evaluate schedule windows
// against an ASSIGNED timezone applied to a trusted epoch (never the OS
// timezone/clock), and quarantine when the clock is changed under a running
// instance. A machine merely booted with the wrong time is resynced, not
// punished. Disabled by default; enable per-config once verified.
export interface ClockIntegritySettings {
  enabled: boolean;
  timezone?: string; // IANA, e.g. "America/Chicago"; empty = OS timezone
  skewToleranceSec: number;
  checkIntervalSec: number;
  anchorIntervalSec: number;
  ntpServers: string[];
  timeUrl?: string; // pinned-backend URL corroborating NTP; empty = NTP only
}
export type ClockIntegrityPatch = Partial<ClockIntegritySettings>;

// --------------------------------------------------------- coverage escalation
// When flagged content keeps accumulating over a window, grow the censor box
// scale so repeated/edge exposure is covered more aggressively; decays back to
// baseline when activity subsides. Reuses the exposure metrics. Disabled by
// default.
export interface CoverageEscalationSettings {
  enabled: boolean;
  metric: ExposureMetric;
  threshold: number;
  windowSec: number;
  startScale: number;
  growthPerUnit: number;
  maxScale: number;
  decayPerSec: number;
}
export type CoverageEscalationPatch = Partial<CoverageEscalationSettings>;

// ---------------------------------------------------------- capture exclusions
// Apps whose windows are never captured/scanned (by bundle id). Disabled by
// default with an empty list.
export interface CaptureExclusionSettings {
  enabled: boolean;
  bundleIds: string[];
}
export type CaptureExclusionPatch = Partial<CaptureExclusionSettings>;

export interface ModulePatches {
  detection?: DetectionPatch;
  censor?: CensorPatch;
  challenge?: ChallengePatch;
  exposure?: ExposurePatch;
  earnedTime?: EarnedTimePatch;
  focusLimit?: FocusLimitPatch;
  clockIntegrity?: ClockIntegrityPatch;
  coverageEscalation?: CoverageEscalationPatch;
  captureExclusions?: CaptureExclusionPatch;
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
  challenge: ChallengeSettings;
  exposure: ExposureSettings;
  earnedTime: EarnedTimeSettings;
  focusLimit: FocusLimitSettings;
  clockIntegrity: ClockIntegritySettings;
  coverageEscalation: CoverageEscalationSettings;
  captureExclusions: CaptureExclusionSettings;
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
    enabled: true,
    model: "640m",
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
    mode: "image",
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
    image: { data: "", path: "", fit: "cover" },
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
export function defaultChallenge(): ChallengeSettings {
  return {
    enabled: false,
    intervalMinSec: 2700,
    intervalMaxSec: 5400,
    categories: [],
    maxGrade: 6,
    answerWindowSec: 120,
    maxAttempts: 3,
  };
}

export function defaultExposure(): ExposureSettings {
  return {
    enabled: false,
    metric: "events",
    warnThreshold: 20,
    warnWindowSec: 300,
    blockThreshold: 40,
    blockWindowSec: 600,
    penaltySec: 900,
    warnCooldownSec: 120,
  };
}

export function defaultEarnedTime(): EarnedTimeSettings {
  return {
    enabled: false,
    schedule: [],
    sources: [],
    spendRatio: 1,
    dailyEarnCapMin: 120,
    maxBankMin: 240,
    minSessionMin: 5,
    idleTimeoutSec: 60,
  };
}

export function defaultFocusLimit(): FocusLimitSettings {
  return {
    enabled: false,
    sameTabLimitMin: 10,
    lockoutMin: 10,
    idleResetSec: 60,
    whitelistHosts: [],
    blacklistHosts: [],
  };
}

export function defaultClockIntegrity(): ClockIntegritySettings {
  return {
    enabled: false,
    skewToleranceSec: 300,
    checkIntervalSec: 15,
    anchorIntervalSec: 900,
    ntpServers: ["time.apple.com", "pool.ntp.org"],
  };
}

export function defaultCoverageEscalation(): CoverageEscalationSettings {
  return {
    enabled: false,
    metric: "events",
    threshold: 20,
    windowSec: 300,
    startScale: 1.5,
    growthPerUnit: 0.05,
    maxScale: 3.0,
    decayPerSec: 0.1,
  };
}

export function defaultCaptureExclusions(): CaptureExclusionSettings {
  return {
    enabled: false,
    bundleIds: [],
  };
}

export function resolve(pkg: Package): Effective {
  const effective: Effective = {
    detection: defaultDetection(),
    censor: defaultCensor(),
    challenge: defaultChallenge(),
    exposure: defaultExposure(),
    earnedTime: defaultEarnedTime(),
    focusLimit: defaultFocusLimit(),
    clockIntegrity: defaultClockIntegrity(),
    coverageEscalation: defaultCoverageEscalation(),
    captureExclusions: defaultCaptureExclusions(),
  };
  const layerPatches = pkg.layers
    .map((name) => pkg.namedConfigs.find((c) => c.name === name)?.settings)
    .filter((s): s is ModulePatches => !!s);
  for (const patches of [...layerPatches, pkg.overrides]) {
    if (patches.detection) applyDetection(effective.detection, patches.detection);
    if (patches.censor) applyCensor(effective.censor, patches.censor);
    if (patches.challenge) Object.assign(effective.challenge, patches.challenge);
    if (patches.exposure) Object.assign(effective.exposure, patches.exposure);
    if (patches.earnedTime) Object.assign(effective.earnedTime, patches.earnedTime);
    if (patches.focusLimit) Object.assign(effective.focusLimit, patches.focusLimit);
    if (patches.clockIntegrity) Object.assign(effective.clockIntegrity, patches.clockIntegrity);
    if (patches.coverageEscalation)
      Object.assign(effective.coverageEscalation, patches.coverageEscalation);
    if (patches.captureExclusions)
      Object.assign(effective.captureExclusions, patches.captureExclusions);
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
