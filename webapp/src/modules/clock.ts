// Clock-integrity editor: trust the clock behind all time-of-day policy.
// Evaluates schedule windows against an ASSIGNED timezone over a trusted epoch
// (never the OS clock), and quarantines when the clock is moved under a running
// instance. Booting with a wrong time is resynced, not punished.

import { css, html, LitElement } from "lit";
import { customElement } from "lit/decorators.js";
import { valueSource, type ClockIntegrityPatch } from "../schema.js";
import { store } from "../store.js";
import "../components/controls.js";

function setOverride<K extends keyof ClockIntegrityPatch>(
  key: K,
  value: ClockIntegrityPatch[K],
): void {
  store.update((pkg) => {
    pkg.overrides.clockIntegrity = { ...pkg.overrides.clockIntegrity, [key]: value };
  });
}

function clearOverride(key: keyof ClockIntegrityPatch): void {
  store.update((pkg) => {
    if (pkg.overrides.clockIntegrity) delete pkg.overrides.clockIntegrity[key];
  });
}

@customElement("bm-clock-module")
export class BmClockModule extends LitElement {
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
    const d = store.effective.clockIntegrity;
    const src = (f: string) => valueSource(pkg, "clockIntegrity", f);

    return html`
      <bm-section heading="Clock integrity">
        <p class="muted">
          All time-of-day policy (schedules, earned-time windows) is only as
          trustworthy as the clock. When enabled, betamacs anchors to a trusted
          epoch and an assigned timezone rather than the OS clock, and treats a
          clock change under a running instance as tampering.
        </p>
        <bm-switch
          label="Enabled"
          .value=${d.enabled}
          source=${src("enabled")}
          @field-change=${(e: CustomEvent) => setOverride("enabled", e.detail)}
          @reset=${() => clearOverride("enabled")}
        ></bm-switch>
        <bm-text
          label="Timezone"
          hint="IANA name, e.g. America/Chicago. Blank = use the OS timezone."
          placeholder="America/Chicago"
          .value=${d.timezone ?? ""}
          source=${src("timezone")}
          @field-change=${(e: CustomEvent) =>
            setOverride("timezone", e.detail === "" ? undefined : e.detail)}
          @reset=${() => clearOverride("timezone")}
        ></bm-text>
      </bm-section>

      <bm-section heading="Skew & cadence">
        <bm-slider
          label="Skew tolerance"
          hint="How far the OS clock may drift from the trusted epoch before it counts as tampering"
          min="0"
          max="1800"
          step="15"
          unit=" s"
          .value=${d.skewToleranceSec}
          source=${src("skewToleranceSec")}
          @field-change=${(e: CustomEvent) => setOverride("skewToleranceSec", e.detail)}
          @reset=${() => clearOverride("skewToleranceSec")}
        ></bm-slider>
        <bm-slider
          label="Check interval"
          hint="How often the running clock is compared against the anchor"
          min="5"
          max="300"
          step="5"
          unit=" s"
          .value=${d.checkIntervalSec}
          source=${src("checkIntervalSec")}
          @field-change=${(e: CustomEvent) => setOverride("checkIntervalSec", e.detail)}
          @reset=${() => clearOverride("checkIntervalSec")}
        ></bm-slider>
        <bm-slider
          label="Anchor interval"
          hint="How often a fresh trusted epoch is fetched from the sources below"
          min="60"
          max="7200"
          step="60"
          unit=" s"
          .value=${d.anchorIntervalSec}
          source=${src("anchorIntervalSec")}
          @field-change=${(e: CustomEvent) => setOverride("anchorIntervalSec", e.detail)}
          @reset=${() => clearOverride("anchorIntervalSec")}
        ></bm-slider>
      </bm-section>

      <bm-section heading="Trusted time sources">
        <bm-list
          label="NTP servers"
          hint="One host per line, tried in order."
          placeholder="time.apple.com&#10;pool.ntp.org"
          .value=${d.ntpServers}
          source=${src("ntpServers")}
          @field-change=${(e: CustomEvent) => setOverride("ntpServers", e.detail)}
          @reset=${() => clearOverride("ntpServers")}
        ></bm-list>
        <bm-text
          label="Corroborating time URL"
          hint="Optional pinned-backend URL cross-checked against NTP. Blank = NTP only."
          placeholder="https://time.newton.haus/now"
          .value=${d.timeUrl ?? ""}
          source=${src("timeUrl")}
          @field-change=${(e: CustomEvent) =>
            setOverride("timeUrl", e.detail === "" ? undefined : e.detail)}
          @reset=${() => clearOverride("timeUrl")}
        ></bm-text>
      </bm-section>
    `;
  }
}
