// Detection engine (NudeNet) module editor. Edits write to the package's
// override layer; badges show where each resolved value came from.

import { css, html, LitElement } from "lit";
import { customElement } from "lit/decorators.js";
import {
  TRIGGER_GROUPS,
  valueSource,
  type DetectionPatch,
  type Package,
} from "../schema.js";
import { store } from "../store.js";
import "../components/controls.js";

function setOverride<K extends keyof DetectionPatch>(
  key: K,
  value: DetectionPatch[K],
): void {
  store.update((pkg) => {
    pkg.overrides.detection = { ...pkg.overrides.detection, [key]: value };
  });
}

function clearOverride(key: keyof DetectionPatch): void {
  store.update((pkg) => {
    if (pkg.overrides.detection) delete pkg.overrides.detection[key];
  });
}

function triggerSource(pkg: Package, cls: string): string {
  if (pkg.overrides.detection?.triggers?.[cls] !== undefined) return "override";
  for (let i = pkg.layers.length - 1; i >= 0; i--) {
    const cfg = pkg.namedConfigs.find((c) => c.name === pkg.layers[i]);
    if (cfg?.settings.detection?.triggers?.[cls] !== undefined) return cfg.name;
  }
  return "default";
}

@customElement("bm-detection-module")
export class BmDetectionModule extends LitElement {
  static styles = css`
    :host {
      display: block;
    }
    .group-head {
      display: flex;
      justify-content: space-between;
      align-items: baseline;
      margin: 16px 0 2px;
    }
    .group-head h4 {
      margin: 0;
      font-size: 13.5px;
    }
    .group-head button {
      border: none;
      background: none;
      color: var(--accent);
      cursor: pointer;
      font-size: 12px;
    }
    .muted {
      color: var(--muted);
      font-size: 12.5px;
    }
  `;

  private unsubscribe = () => {};
  connectedCallback() {
    super.connectedCallback();
    this.unsubscribe = store.subscribe(() => this.requestUpdate());
  }
  disconnectedCallback() {
    super.disconnectedCallback();
    this.unsubscribe();
  }

  private setGroup(classes: string[], on: boolean) {
    store.update((pkg) => {
      const t = { ...pkg.overrides.detection?.triggers };
      for (const c of classes) t[c] = on;
      pkg.overrides.detection = { ...pkg.overrides.detection, triggers: t };
    });
  }

  render() {
    const pkg = store.pkg;
    const d = store.effective.detection;
    const src = (f: string) => valueSource(pkg, "detection", f);

    return html`
      <bm-section heading="Engine">
        <bm-select
          label="Model"
          hint="640m is more accurate but ~5x slower per frame"
          .options=${[
            { value: "320n", label: "320n — fast" },
            { value: "640m", label: "640m — accurate" },
          ]}
          .value=${d.model}
          source=${src("model")}
          @field-change=${(e: CustomEvent) => setOverride("model", e.detail)}
          @reset=${() => clearOverride("model")}
        ></bm-select>
        <bm-slider
          label="Confidence threshold"
          hint="Detections below this confidence are ignored"
          min="0.05"
          max="0.95"
          step="0.01"
          .value=${d.confidenceThreshold}
          source=${src("confidenceThreshold")}
          @field-change=${(e: CustomEvent) => setOverride("confidenceThreshold", e.detail)}
          @reset=${() => clearOverride("confidenceThreshold")}
        ></bm-slider>
        <bm-slider
          label="Overlap threshold (NMS IoU)"
          min="0.1"
          max="0.9"
          step="0.05"
          .value=${d.iouThreshold}
          source=${src("iouThreshold")}
          @field-change=${(e: CustomEvent) => setOverride("iouThreshold", e.detail)}
          @reset=${() => clearOverride("iouThreshold")}
        ></bm-slider>
        <bm-slider
          label="Minimum region size"
          hint="Detections smaller than this in either dimension are ignored"
          min="0"
          max="200"
          step="5"
          unit=" px"
          .value=${d.minRegionPx}
          source=${src("minRegionPx")}
          @field-change=${(e: CustomEvent) => setOverride("minRegionPx", e.detail)}
          @reset=${() => clearOverride("minRegionPx")}
        ></bm-slider>
      </bm-section>

      <bm-section heading="Scanning">
        <bm-slider
          label="Capture rate"
          hint="Max frames per second per display (restart to apply)"
          min="0.5"
          max="10"
          step="0.5"
          unit=" fps"
          .value=${d.captureFps}
          source=${src("captureFps")}
          @field-change=${(e: CustomEvent) => setOverride("captureFps", e.detail)}
          @reset=${() => clearOverride("captureFps")}
        ></bm-slider>
        <bm-slider
          label="Tile grid"
          hint="Extra tiled passes per screen so small content isn't missed; 0 = whole-frame only"
          min="0"
          max="3"
          step="1"
          .value=${d.tileGrid}
          source=${src("tileGrid")}
          @field-change=${(e: CustomEvent) => setOverride("tileGrid", e.detail)}
          @reset=${() => clearOverride("tileGrid")}
        ></bm-slider>
        <bm-slider
          label="Hold time"
          hint="Grace period before a censor box is released"
          min="0"
          max="5000"
          step="100"
          unit=" ms"
          .value=${d.holdMs}
          source=${src("holdMs")}
          @field-change=${(e: CustomEvent) => setOverride("holdMs", e.detail)}
          @reset=${() => clearOverride("holdMs")}
        ></bm-slider>
      </bm-section>

      <bm-section heading="Triggers">
        <p class="muted">
          Which NudeNet detections cause censoring. Grouped for sanity; use the
          group links to flip a whole group.
        </p>
        ${TRIGGER_GROUPS.map(
          (group) => html`
            <div class="group-head">
              <h4>${group.label}</h4>
              <span>
                <button @click=${() => this.setGroup(group.classes, true)}>all on</button>
                ·
                <button @click=${() => this.setGroup(group.classes, false)}>all off</button>
              </span>
            </div>
            ${group.classes.map(
              (cls) => html`
                <bm-switch
                  label=${cls.replaceAll("_", " ").toLowerCase()}
                  .value=${d.triggers[cls] ?? false}
                  source=${triggerSource(pkg, cls)}
                  @field-change=${(e: CustomEvent) =>
                    store.update((pkg) => {
                      pkg.overrides.detection = {
                        ...pkg.overrides.detection,
                        triggers: {
                          ...pkg.overrides.detection?.triggers,
                          [cls]: e.detail as boolean,
                        },
                      };
                    })}
                  @reset=${() =>
                    store.update((pkg) => {
                      delete pkg.overrides.detection?.triggers?.[cls];
                    })}
                ></bm-switch>
              `,
            )}
          `,
        )}
      </bm-section>
    `;
  }
}
