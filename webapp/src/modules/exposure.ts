// Exposure-budget editor: quantify censor activity over rolling windows and
// escalate — a warning popup at the soft limit, a timed internet lockout at
// the hard limit.

import { css, html, LitElement } from "lit";
import { customElement } from "lit/decorators.js";
import { valueSource, type ExposurePatch } from "../schema.js";
import { store } from "../store.js";
import "../components/controls.js";

function setOverride<K extends keyof ExposurePatch>(
  key: K,
  value: ExposurePatch[K],
): void {
  store.update((pkg) => {
    pkg.overrides.exposure = { ...pkg.overrides.exposure, [key]: value };
  });
}

function clearOverride(key: keyof ExposurePatch): void {
  store.update((pkg) => {
    if (pkg.overrides.exposure) delete pkg.overrides.exposure[key];
  });
}

@customElement("bm-exposure-module")
export class BmExposureModule extends LitElement {
  static styles = css`
    :host {
      display: block;
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

  render() {
    const pkg = store.pkg;
    const d = store.effective.exposure;
    const src = (f: string) => valueSource(pkg, "exposure", f);

    return html`
      <bm-section heading="Exposure budget">
        <p class="muted">
          Measures how much the censor is firing over a window. Crossing the
          warn limit shows a prompt; crossing the block limit cuts the
          internet for a fixed period (enforced by betamacsd).
        </p>
        <bm-switch
          label="Enabled"
          .value=${d.enabled}
          source=${src("enabled")}
          @field-change=${(e: CustomEvent) => setOverride("enabled", e.detail)}
          @reset=${() => clearOverride("enabled")}
        ></bm-switch>
        <bm-select
          label="Metric"
          hint="What accumulates over the window"
          .options=${[
            { value: "events", label: "Events — how often" },
            { value: "activeSeconds", label: "Active seconds" },
            { value: "boxSeconds", label: "Box-seconds — how many" },
            { value: "areaSeconds", label: "Area-seconds — how much space" },
          ]}
          .value=${d.metric}
          source=${src("metric")}
          @field-change=${(e: CustomEvent) => setOverride("metric", e.detail)}
          @reset=${() => clearOverride("metric")}
        ></bm-select>
      </bm-section>

      <bm-section heading="Warning (soft limit)">
        <bm-slider
          label="Warn threshold"
          hint="Metric total within the warn window that triggers the popup"
          min="0"
          max="200"
          step="1"
          .value=${d.warnThreshold}
          source=${src("warnThreshold")}
          @field-change=${(e: CustomEvent) => setOverride("warnThreshold", e.detail)}
          @reset=${() => clearOverride("warnThreshold")}
        ></bm-slider>
        <bm-slider
          label="Warn window"
          min="30"
          max="3600"
          step="30"
          unit=" s"
          .value=${d.warnWindowSec}
          source=${src("warnWindowSec")}
          @field-change=${(e: CustomEvent) => setOverride("warnWindowSec", e.detail)}
          @reset=${() => clearOverride("warnWindowSec")}
        ></bm-slider>
        <bm-slider
          label="Warn cooldown"
          hint="Minimum gap between warning popups"
          min="0"
          max="1800"
          step="30"
          unit=" s"
          .value=${d.warnCooldownSec}
          source=${src("warnCooldownSec")}
          @field-change=${(e: CustomEvent) => setOverride("warnCooldownSec", e.detail)}
          @reset=${() => clearOverride("warnCooldownSec")}
        ></bm-slider>
      </bm-section>

      <bm-section heading="Lockout (hard limit)">
        <bm-slider
          label="Block threshold"
          hint="Metric total within the block window that trips the lockout"
          min="0"
          max="400"
          step="1"
          .value=${d.blockThreshold}
          source=${src("blockThreshold")}
          @field-change=${(e: CustomEvent) => setOverride("blockThreshold", e.detail)}
          @reset=${() => clearOverride("blockThreshold")}
        ></bm-slider>
        <bm-slider
          label="Block window"
          min="60"
          max="7200"
          step="60"
          unit=" s"
          .value=${d.blockWindowSec}
          source=${src("blockWindowSec")}
          @field-change=${(e: CustomEvent) => setOverride("blockWindowSec", e.detail)}
          @reset=${() => clearOverride("blockWindowSec")}
        ></bm-slider>
        <bm-slider
          label="Penalty duration"
          hint="How long the internet stays cut once the hard limit trips"
          min="60"
          max="7200"
          step="60"
          unit=" s"
          .value=${d.penaltySec}
          source=${src("penaltySec")}
          @field-change=${(e: CustomEvent) => setOverride("penaltySec", e.detail)}
          @reset=${() => clearOverride("penaltySec")}
        ></bm-slider>
      </bm-section>
    `;
  }
}
