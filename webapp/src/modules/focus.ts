// Focus-limit editor: auto-lockout for staying actively on one browser tab too
// long (active scrolling; idle/video is exempt). Policy only.

import { css, html, LitElement } from "lit";
import { customElement } from "lit/decorators.js";
import { valueSource, type FocusLimitPatch } from "../schema.js";
import { store } from "../store.js";
import "../components/controls.js";

function setOverride<K extends keyof FocusLimitPatch>(
  key: K,
  value: FocusLimitPatch[K],
): void {
  store.update((pkg) => {
    pkg.overrides.focusLimit = { ...pkg.overrides.focusLimit, [key]: value };
  });
}

function clearOverride(key: keyof FocusLimitPatch): void {
  store.update((pkg) => {
    if (pkg.overrides.focusLimit) delete pkg.overrides.focusLimit[key];
  });
}

@customElement("bm-focus-module")
export class BmFocusModule extends LitElement {
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
    const d = store.effective.focusLimit;
    const src = (f: string) => valueSource(pkg, "focusLimit", f);

    return html`
      <bm-section heading="Focus limit">
        <p class="muted">
          Locks the machine out after too long actively on a single browser tab
          (active scrolling counts; idle time and video playback are exempt).
        </p>
        <bm-switch
          label="Enabled"
          .value=${d.enabled}
          source=${src("enabled")}
          @field-change=${(e: CustomEvent) => setOverride("enabled", e.detail)}
          @reset=${() => clearOverride("enabled")}
        ></bm-switch>
        <bm-slider
          label="Same-tab limit"
          hint="Active minutes on one tab before lockout"
          min="1"
          max="120"
          step="1"
          unit=" min"
          .value=${d.sameTabLimitMin}
          source=${src("sameTabLimitMin")}
          @field-change=${(e: CustomEvent) => setOverride("sameTabLimitMin", e.detail)}
          @reset=${() => clearOverride("sameTabLimitMin")}
        ></bm-slider>
        <bm-slider
          label="Lockout duration"
          hint="How long the lockout lasts once triggered"
          min="1"
          max="120"
          step="1"
          unit=" min"
          .value=${d.lockoutMin}
          source=${src("lockoutMin")}
          @field-change=${(e: CustomEvent) => setOverride("lockoutMin", e.detail)}
          @reset=${() => clearOverride("lockoutMin")}
        ></bm-slider>
        <bm-slider
          label="Idle reset"
          hint="Idle time that resets the same-tab timer"
          min="5"
          max="600"
          step="5"
          unit=" s"
          .value=${d.idleResetSec}
          source=${src("idleResetSec")}
          @field-change=${(e: CustomEvent) => setOverride("idleResetSec", e.detail)}
          @reset=${() => clearOverride("idleResetSec")}
        ></bm-slider>
      </bm-section>

      <bm-section heading="Host scoping">
        <bm-list
          label="Whitelist hosts"
          hint="Never triggers on these hosts. One per line."
          placeholder="khanacademy.org&#10;docs.google.com"
          .value=${d.whitelistHosts}
          source=${src("whitelistHosts")}
          @field-change=${(e: CustomEvent) => setOverride("whitelistHosts", e.detail)}
          @reset=${() => clearOverride("whitelistHosts")}
        ></bm-list>
        <bm-list
          label="Blacklist hosts"
          hint="If non-empty, ONLY these hosts are monitored. One per line."
          placeholder="youtube.com&#10;reddit.com"
          .value=${d.blacklistHosts}
          source=${src("blacklistHosts")}
          @field-change=${(e: CustomEvent) => setOverride("blacklistHosts", e.detail)}
          @reset=${() => clearOverride("blacklistHosts")}
        ></bm-list>
      </bm-section>
    `;
  }
}
