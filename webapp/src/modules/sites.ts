// Site-filter editor: the internet allow/block lists betamacsd enforces with
// its local DNS filter + pf (docs/site-filter.md). With no earned-time
// balance only the allowlist (plus the earn sources) is reachable; the
// blocklist is never reachable while the filter is on.

import { css, html, LitElement } from "lit";
import { customElement } from "lit/decorators.js";
import { valueSource, type SiteFilterPatch } from "../schema.js";
import { store } from "../store.js";
import "../components/controls.js";

function setOverride<K extends keyof SiteFilterPatch>(
  key: K,
  value: SiteFilterPatch[K],
): void {
  store.update((pkg) => {
    pkg.overrides.siteFilter = { ...pkg.overrides.siteFilter, [key]: value };
  });
}

function clearOverride(key: keyof SiteFilterPatch): void {
  store.update((pkg) => {
    if (pkg.overrides.siteFilter) delete pkg.overrides.siteFilter[key];
  });
}

@customElement("bm-sites-module")
export class BmSitesModule extends LitElement {
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
    const d = store.effective.siteFilter;
    const sources = store.effective.earnedTime.sources
      .map((s) => s.match.browserHostSuffix)
      .filter((h): h is string => !!h);
    const src = (f: string) => valueSource(pkg, "siteFilter", f);

    return html`
      <bm-section heading="Site filter">
        <p class="muted">
          Steers a child with no earned-time balance to the sites that earn it:
          while the balance is empty, only the allowlist below (plus the earn
          sources${sources.length ? `: ${sources.join(", ")}` : ""}) resolves.
          The blocklist never resolves while the filter is on, balance or not.
          Enforced on the device by name (a local DNS filter feeding pf), so
          "kastatic.org" covers every host under it. Applies only to
          provisioned kid devices, like earned time.
        </p>
        <bm-switch
          label="Enabled"
          .value=${d.enabled}
          source=${src("enabled")}
          @field-change=${(e: CustomEvent) => setOverride("enabled", e.detail)}
          @reset=${() => clearOverride("enabled")}
        ></bm-switch>
        <bm-switch
          label="Audit only"
          hint="Log every lookup, block nothing — use this first to discover which hosts a site or app needs (site-audit.log on the device, and the status HUD)"
          .value=${d.auditOnly}
          source=${src("auditOnly")}
          @field-change=${(e: CustomEvent) => setOverride("auditOnly", e.detail)}
          @reset=${() => clearOverride("auditOnly")}
        ></bm-switch>
      </bm-section>

      <bm-section heading="Lists">
        <bm-list
          label="Allowed with no balance"
          hint="Domain suffixes reachable while the balance is empty. Khan Academy needs khanacademy.org, kastatic.org, kasandbox.org, and youtube.com + googlevideo.com + ytimg.com for its videos. One per line."
          placeholder="khanacademy.org&#10;kastatic.org&#10;kasandbox.org"
          .value=${d.allowHosts}
          source=${src("allowHosts")}
          @field-change=${(e: CustomEvent) => setOverride("allowHosts", e.detail)}
          @reset=${() => clearOverride("allowHosts")}
        ></bm-list>
        <bm-list
          label="Always blocked"
          hint="Domain suffixes that never resolve while the filter is on, even with balance. One per line."
          placeholder="tiktok.com&#10;discord.com"
          .value=${d.blockHosts}
          source=${src("blockHosts")}
          @field-change=${(e: CustomEvent) => setOverride("blockHosts", e.detail)}
          @reset=${() => clearOverride("blockHosts")}
        ></bm-list>
      </bm-section>
    `;
  }
}
