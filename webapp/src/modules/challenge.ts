// Activity-challenge policy editor. Policy only — the questions live in the
// separate, independently-versioned betamacs-tasks artifact.

import { css, html, LitElement } from "lit";
import { customElement } from "lit/decorators.js";
import { valueSource, type ChallengePatch } from "../schema.js";
import { store } from "../store.js";
import "../components/controls.js";

function setOverride<K extends keyof ChallengePatch>(
  key: K,
  value: ChallengePatch[K],
): void {
  store.update((pkg) => {
    pkg.overrides.challenge = { ...pkg.overrides.challenge, [key]: value };
  });
}

function clearOverride(key: keyof ChallengePatch): void {
  store.update((pkg) => {
    if (pkg.overrides.challenge) delete pkg.overrides.challenge[key];
  });
}

@customElement("bm-challenge-module")
export class BmChallengeModule extends LitElement {
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
    const d = store.effective.challenge;
    const src = (f: string) => valueSource(pkg, "challenge", f);

    return html`
      <bm-section heading="Activity challenges">
        <p class="muted">
          Periodic tasks the user must answer, or the internet is cut until
          they do. Questions come from the separate, independently-versioned
          <strong>betamacs-tasks</strong> bank — this only sets policy.
        </p>
        <bm-switch
          label="Enabled"
          .value=${d.enabled}
          source=${src("enabled")}
          @field-change=${(e: CustomEvent) => setOverride("enabled", e.detail)}
          @reset=${() => clearOverride("enabled")}
        ></bm-switch>
        <bm-slider
          label="Interval minimum"
          hint="Lower bound of the random gap between challenges"
          min="60"
          max="14400"
          step="60"
          unit=" s"
          .value=${d.intervalMinSec}
          source=${src("intervalMinSec")}
          @field-change=${(e: CustomEvent) => setOverride("intervalMinSec", e.detail)}
          @reset=${() => clearOverride("intervalMinSec")}
        ></bm-slider>
        <bm-slider
          label="Interval maximum"
          min="60"
          max="14400"
          step="60"
          unit=" s"
          .value=${d.intervalMaxSec}
          source=${src("intervalMaxSec")}
          @field-change=${(e: CustomEvent) => setOverride("intervalMaxSec", e.detail)}
          @reset=${() => clearOverride("intervalMaxSec")}
        ></bm-slider>
        <bm-slider
          label="Maximum grade"
          hint="Never pick a task above this grade band"
          min="1"
          max="12"
          step="1"
          .value=${d.maxGrade}
          source=${src("maxGrade")}
          @field-change=${(e: CustomEvent) => setOverride("maxGrade", e.detail)}
          @reset=${() => clearOverride("maxGrade")}
        ></bm-slider>
        <bm-slider
          label="Answer window"
          hint="Time to answer before it counts as unprotected"
          min="15"
          max="600"
          step="15"
          unit=" s"
          .value=${d.answerWindowSec}
          source=${src("answerWindowSec")}
          @field-change=${(e: CustomEvent) => setOverride("answerWindowSec", e.detail)}
          @reset=${() => clearOverride("answerWindowSec")}
        ></bm-slider>
        <bm-slider
          label="Max attempts"
          hint="Wrong answers before a fresh task is picked"
          min="1"
          max="6"
          step="1"
          .value=${d.maxAttempts}
          source=${src("maxAttempts")}
          @field-change=${(e: CustomEvent) => setOverride("maxAttempts", e.detail)}
          @reset=${() => clearOverride("maxAttempts")}
        ></bm-slider>
        <bm-text
          label="Categories"
          hint="Comma-separated task categories to draw from (blank = none eligible)"
          .value=${d.categories.join(", ")}
          source=${src("categories")}
          @field-change=${(e: CustomEvent) =>
            setOverride(
              "categories",
              String(e.detail)
                .split(",")
                .map((s) => s.trim())
                .filter(Boolean),
            )}
          @reset=${() => clearOverride("categories")}
        ></bm-text>
      </bm-section>
    `;
  }
}
