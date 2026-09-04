// Coverage-escalation editor: when flagged content keeps accumulating over a
// window, grow the censor box scale so repeated/edge exposure is covered more
// aggressively; decay back to baseline when activity subsides.

import { css, html, LitElement } from "lit";
import { customElement } from "lit/decorators.js";
import { valueSource, type CoverageEscalationPatch } from "../schema.js";
import { store } from "../store.js";
import "../components/controls.js";

function setOverride<K extends keyof CoverageEscalationPatch>(
  key: K,
  value: CoverageEscalationPatch[K],
): void {
  store.update((pkg) => {
    pkg.overrides.coverageEscalation = {
      ...pkg.overrides.coverageEscalation,
      [key]: value,
    };
  });
}

function clearOverride(key: keyof CoverageEscalationPatch): void {
  store.update((pkg) => {
    if (pkg.overrides.coverageEscalation) delete pkg.overrides.coverageEscalation[key];
  });
}

@customElement("bm-coverage-module")
export class BmCoverageModule extends LitElement {
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
    const d = store.effective.coverageEscalation;
    const src = (f: string) => valueSource(pkg, "coverageEscalation", f);

    return html`
      <bm-section heading="Coverage escalation">
        <p class="muted">
          Grows the censor box scale as flagged activity accumulates over a
          window, so persistent or edge-of-frame exposure gets covered more
          aggressively. Decays back to the starting scale when activity stops.
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
          hint="What accumulates over the window (same metrics as the exposure budget)"
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
        <bm-slider
          label="Threshold"
          hint="Metric total within the window before scaling begins"
          min="0"
          max="200"
          step="1"
          .value=${d.threshold}
          source=${src("threshold")}
          @field-change=${(e: CustomEvent) => setOverride("threshold", e.detail)}
          @reset=${() => clearOverride("threshold")}
        ></bm-slider>
        <bm-slider
          label="Window"
          min="30"
          max="3600"
          step="30"
          unit=" s"
          .value=${d.windowSec}
          source=${src("windowSec")}
          @field-change=${(e: CustomEvent) => setOverride("windowSec", e.detail)}
          @reset=${() => clearOverride("windowSec")}
        ></bm-slider>
      </bm-section>

      <bm-section heading="Scaling curve">
        <bm-slider
          label="Start scale"
          hint="Box scale multiplier once the threshold is crossed"
          min="1"
          max="3"
          step="0.05"
          unit="×"
          .value=${d.startScale}
          source=${src("startScale")}
          @field-change=${(e: CustomEvent) => setOverride("startScale", e.detail)}
          @reset=${() => clearOverride("startScale")}
        ></bm-slider>
        <bm-slider
          label="Growth per unit"
          hint="Added to the scale for each additional metric unit over the threshold"
          min="0"
          max="0.5"
          step="0.01"
          unit="×"
          .value=${d.growthPerUnit}
          source=${src("growthPerUnit")}
          @field-change=${(e: CustomEvent) => setOverride("growthPerUnit", e.detail)}
          @reset=${() => clearOverride("growthPerUnit")}
        ></bm-slider>
        <bm-slider
          label="Max scale"
          hint="Ceiling on the box scale multiplier"
          min="1"
          max="6"
          step="0.1"
          unit="×"
          .value=${d.maxScale}
          source=${src("maxScale")}
          @field-change=${(e: CustomEvent) => setOverride("maxScale", e.detail)}
          @reset=${() => clearOverride("maxScale")}
        ></bm-slider>
        <bm-slider
          label="Decay per second"
          hint="How fast the scale relaxes back toward the start scale when idle"
          min="0"
          max="1"
          step="0.01"
          unit="×/s"
          .value=${d.decayPerSec}
          source=${src("decayPerSec")}
          @field-change=${(e: CustomEvent) => setOverride("decayPerSec", e.detail)}
          @reset=${() => clearOverride("decayPerSec")}
        ></bm-slider>
      </bm-section>
    `;
  }
}
