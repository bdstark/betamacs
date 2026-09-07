// App shell: the ui-kit frame around the policy modules. The sidebar lists
// the sections, the page header names the open one, the store bar and the
// dev connection panel travel with every section.

import "@newtonhaus/ui-kit/fonts.css";
import "@newtonhaus/ui-kit/tokens.css";
import "@newtonhaus/ui-kit/base.css";
import "./styles/kit.css";

import { css, html, LitElement, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";
import { initTheme, type NhNavItem, type NhNavigateDetail } from "@newtonhaus/ui-kit";
import { store } from "./store.js";
import "./components/controls.js";
import "./components/layers.js";
import "./components/publish.js";
import "./components/assignment.js";
import "./modules/detection.js";
import "./modules/censor.js";
import "./modules/challenge.js";
import "./modules/exposure.js";
import "./modules/clock.js";
import "./modules/coverage.js";
import "./modules/exclusions.js";
import "./modules/earned.js";
import "./modules/focus.js";
import "./modules/sites.js";

initTheme();

const SECTIONS = [
  { id: "detection", label: "Detection engine", group: "Policy", description: "Model, triggers, and what counts as a hit" },
  { id: "censor", label: "Black box censor", group: "Policy", description: "How a detected region is covered" },
  { id: "challenge", label: "Activity challenges", group: "Policy", description: "Prompts that interrupt a session" },
  { id: "exposure", label: "Exposure budget", group: "Policy", description: "How much uncovered time a day allows" },
  { id: "coverage", label: "Coverage escalation", group: "Policy", description: "How coverage tightens after repeated hits" },
  { id: "exclusions", label: "Capture exclusions", group: "Policy", description: "Windows and apps the censor ignores" },
  { id: "earned", label: "Earned time", group: "Policy", description: "Activities that earn uncovered time" },
  { id: "focus", label: "Focus limit", group: "Policy", description: "Lockout after too long on one tab" },
  { id: "sites", label: "Site filter", group: "Policy", description: "Sites allowed with no balance, and sites always blocked" },
  { id: "clock", label: "Clock integrity", group: "Policy", description: "Defences against clock tampering" },
  { id: "layers", label: "Layers & package", group: "Rollout", description: "Named configs, inheritance, and the package that ships" },
  { id: "assignment", label: "Devices", group: "Rollout", description: "Which machines take this package" },
] as const;
type TabId = (typeof SECTIONS)[number]["id"];

function sectionFromHash(): TabId {
  const raw = location.hash.replace(/^#/, "").trim();
  return SECTIONS.some((s) => s.id === raw) ? (raw as TabId) : "detection";
}

const NAV_ITEMS: NhNavItem[] = SECTIONS.map((s) => ({
  key: s.id,
  label: s.label,
  href: `#${s.id}`,
  group: s.group,
  description: s.description,
}));

@customElement("bm-app")
export class BmApp extends LitElement {
  static styles = css`
    :host {
      display: block;
    }
    .content {
      display: grid;
      gap: 16px;
      max-width: 900px;
    }
    details.dev summary {
      color: var(--nh-muted);
      font-size: 12.5px;
      cursor: pointer;
      padding: 4px 2px;
    }
    .connection {
      display: flex;
      gap: 8px;
      align-items: center;
      flex-wrap: wrap;
      background: var(--nh-panel);
      border: 1px solid var(--nh-line);
      border-radius: var(--nh-radius-lg);
      padding: 10px 14px;
      margin-top: 8px;
    }
    .connection input {
      background: var(--nh-bg);
      color: var(--nh-text);
      border: 1px solid var(--nh-line-strong);
      border-radius: var(--nh-radius-md);
      padding: 5px 9px;
      font: inherit;
      font-size: 13px;
    }
    .connection input.url {
      width: 190px;
    }
    .connection input.token {
      width: 210px;
    }
    .connection button {
      border: 1px solid var(--nh-line-strong);
      background: var(--nh-panel);
      color: var(--nh-text);
      border-radius: var(--nh-radius-md);
      padding: 5px 12px;
      cursor: pointer;
      font: inherit;
      font-size: 13px;
      font-weight: 600;
    }
    .connection button.push {
      background: var(--nh-accent);
      border-color: var(--nh-accent);
      color: var(--nh-accent-ink);
    }
    .connection button:disabled {
      opacity: 0.5;
      cursor: not-allowed;
    }
    .status {
      flex-basis: 100%;
      color: var(--nh-muted);
      font-size: 12.5px;
      min-height: 1em;
    }
  `;

  @state() private tab: TabId = sectionFromHash();

  private unsubscribe = () => {};
  private readonly onHash = (): void => {
    this.tab = sectionFromHash();
  };

  connectedCallback() {
    super.connectedCallback();
    this.unsubscribe = store.subscribe(() => this.requestUpdate());
    window.addEventListener("hashchange", this.onHash);
    void store.probeManaged();
  }
  disconnectedCallback() {
    super.disconnectedCallback();
    this.unsubscribe();
    window.removeEventListener("hashchange", this.onHash);
  }

  private navigate(event: Event): void {
    event.preventDefault();
    location.hash = `#${(event as CustomEvent<NhNavigateDetail>).detail.key}`;
  }

  private async pull() {
    store.setStatus("pulling…");
    try {
      await store.pull();
      store.setStatus("pulled package from app");
    } catch (e) {
      store.setStatus(`pull failed: ${e}`);
    }
  }

  private async push() {
    store.setStatus("pushing…");
    try {
      await store.push();
      store.setStatus("pushed — applied live");
    } catch (e) {
      store.setStatus(`push failed: ${e}`);
    }
  }

  private section() {
    switch (this.tab) {
      case "detection": return html`<bm-detection-module></bm-detection-module>`;
      case "censor": return html`<bm-censor-module></bm-censor-module>`;
      case "challenge": return html`<bm-challenge-module></bm-challenge-module>`;
      case "exposure": return html`<bm-exposure-module></bm-exposure-module>`;
      case "coverage": return html`<bm-coverage-module></bm-coverage-module>`;
      case "exclusions": return html`<bm-exclusions-module></bm-exclusions-module>`;
      case "earned": return html`<bm-earned-module></bm-earned-module>`;
      case "focus": return html`<bm-focus-module></bm-focus-module>`;
      case "sites": return html`<bm-sites-module></bm-sites-module>`;
      case "clock": return html`<bm-clock-module></bm-clock-module>`;
      case "layers": return html`<bm-layers></bm-layers>`;
      case "assignment": return html`<bm-assignment></bm-assignment>`;
    }
  }

  render() {
    const conn = store.connection;
    const current = SECTIONS.find((s) => s.id === this.tab) ?? SECTIONS[0];
    return html`
      <nh-shell product="betamacs" tagline="Policy console">
        <nh-nav slot="nav" .items=${NAV_ITEMS} .current=${this.tab} @nh-navigate=${this.navigate}></nh-nav>
        <nh-theme-toggle slot="sidebar-footer"></nh-theme-toggle>

        <nh-page-header slot="header" heading=${current.label} description=${current.description}>
          ${store.managed
            ? html`<nh-badge slot="meta" tone="warn" dot title="Settings are pushed by the fleet; local changes are refused.">managed · read-only</nh-badge>`
            : nothing}
        </nh-page-header>

        <div class="content">
          <bm-store-bar></bm-store-bar>

          ${this.section()}

          <details class="dev">
            <summary>Live app (dev): push/pull a running betamacs</summary>
            <div class="connection">
              <input
                class="url"
                type="text"
                placeholder="app url"
                .value=${conn.url}
                @change=${(e: Event) =>
                  store.setConnection({ ...conn, url: (e.target as HTMLInputElement).value })}
              />
              <input
                class="token"
                type="password"
                placeholder="api token (config/api-token)"
                .value=${conn.token}
                @change=${(e: Event) =>
                  store.setConnection({ ...conn, token: (e.target as HTMLInputElement).value })}
              />
              <button @click=${this.pull}>Pull</button>
              <button class="push" @click=${this.push} ?disabled=${store.managed}>
                ${store.managed ? "Managed by fleet" : "Push to app"}
              </button>
              <span class="status">${store.status}</span>
            </div>
          </details>
        </div>
      </nh-shell>
    `;
  }
}
