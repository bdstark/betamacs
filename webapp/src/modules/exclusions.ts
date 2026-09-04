// Capture-exclusions editor: apps whose windows are never captured or scanned,
// by bundle id. Disabled with an empty list by default.

import { css, html, LitElement } from "lit";
import { customElement } from "lit/decorators.js";
import { valueSource, type CaptureExclusionPatch } from "../schema.js";
import { store } from "../store.js";
import "../components/controls.js";

function setOverride<K extends keyof CaptureExclusionPatch>(
  key: K,
  value: CaptureExclusionPatch[K],
): void {
  store.update((pkg) => {
    pkg.overrides.captureExclusions = {
      ...pkg.overrides.captureExclusions,
      [key]: value,
    };
  });
}

function clearOverride(key: keyof CaptureExclusionPatch): void {
  store.update((pkg) => {
    if (pkg.overrides.captureExclusions) delete pkg.overrides.captureExclusions[key];
  });
}

@customElement("bm-exclusions-module")
export class BmExclusionsModule extends LitElement {
  static styles = css`
    :host {
      display: block;
    }
    .muted {
      color: var(--muted);
      font-size: 12.5px;
    }
    .warn {
      color: #c77700;
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
    const d = store.effective.captureExclusions;
    const src = (f: string) => valueSource(pkg, "captureExclusions", f);

    return html`
      <bm-section heading="Capture exclusions">
        <p class="muted">
          Apps listed here are never captured or scanned — their windows are
          skipped entirely by the detection pipeline.
        </p>
        <p class="warn">
          Excluding an app turns off all censoring inside it. Use sparingly
          (e.g. a password manager), not as a general allowlist.
        </p>
        <bm-switch
          label="Enabled"
          .value=${d.enabled}
          source=${src("enabled")}
          @field-change=${(e: CustomEvent) => setOverride("enabled", e.detail)}
          @reset=${() => clearOverride("enabled")}
        ></bm-switch>
        <bm-list
          label="Excluded bundle ids"
          hint="One macOS bundle id per line, e.g. com.apple.Passwords"
          placeholder="com.apple.Passwords&#10;com.1password.1password"
          .value=${d.bundleIds}
          source=${src("bundleIds")}
          @field-change=${(e: CustomEvent) => setOverride("bundleIds", e.detail)}
          @reset=${() => clearOverride("bundleIds")}
        ></bm-list>
      </bm-section>
    `;
  }
}
