// App shell: connection bar, module navigation, module panes.

import { css, html, LitElement } from "lit";
import { customElement, state } from "lit/decorators.js";
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

const TABS = [
  { id: "detection", label: "Detection engine" },
  { id: "censor", label: "Black box censor" },
  { id: "challenge", label: "Activity challenges" },
  { id: "exposure", label: "Exposure budget" },
  { id: "coverage", label: "Coverage escalation" },
  { id: "exclusions", label: "Capture exclusions" },
  { id: "earned", label: "Earned time" },
  { id: "focus", label: "Focus limit" },
  { id: "clock", label: "Clock integrity" },
  { id: "layers", label: "Layers & package" },
  { id: "assignment", label: "Devices" },
] as const;
type TabId = (typeof TABS)[number]["id"];

@customElement("bm-app")
export class BmApp extends LitElement {
  static styles = css`
    :host {
      display: block;
      max-width: 860px;
      margin: 0 auto;
      padding: 24px 18px 60px;
    }
    header {
      display: flex;
      align-items: baseline;
      gap: 14px;
      margin-bottom: 10px;
    }
    h1 {
      font-size: 20px;
      margin: 0;
    }
    .tag {
      color: var(--muted);
      font-size: 13px;
    }
    details.dev {
      margin-bottom: 16px;
    }
    details.dev summary {
      color: var(--muted);
      font-size: 12.5px;
      cursor: pointer;
      padding: 4px 2px;
    }
    .connection {
      display: flex;
      gap: 8px;
      align-items: center;
      flex-wrap: wrap;
      background: var(--panel);
      border: 1px solid var(--border);
      border-radius: 12px;
      padding: 10px 14px;
      margin-top: 8px;
    }
    .connection input {
      background: var(--bg);
      color: var(--text);
      border: 1px solid var(--border);
      border-radius: 6px;
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
      border: 1px solid var(--border);
      background: var(--bg);
      color: var(--text);
      border-radius: 6px;
      padding: 5px 12px;
      cursor: pointer;
      font: inherit;
      font-size: 13px;
    }
    .connection button.push {
      background: var(--accent);
      border-color: var(--accent);
      color: #fff;
      font-weight: 600;
    }
    .status {
      flex-basis: 100%;
      color: var(--muted);
      font-size: 12.5px;
      min-height: 1em;
    }
    nav {
      display: flex;
      gap: 4px;
      margin-bottom: 16px;
    }
    nav button {
      border: none;
      background: none;
      color: var(--muted);
      font: inherit;
      font-weight: 500;
      padding: 7px 12px;
      border-radius: 8px;
      cursor: pointer;
    }
    nav button.active {
      background: var(--panel);
      color: var(--text);
      border: 1px solid var(--border);
    }
  `;

  @state() private tab: TabId = "detection";

  private unsubscribe = () => {};
  connectedCallback() {
    super.connectedCallback();
    this.unsubscribe = store.subscribe(() => this.requestUpdate());
    void store.probeManaged();
  }
  disconnectedCallback() {
    super.disconnectedCallback();
    this.unsubscribe();
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

  render() {
    const conn = store.connection;
    return html`
      <header>
        <h1>betamacs</h1>
        <span class="tag">config</span>
        ${store.managed
          ? html`<span class="tag" title="Settings are pushed by the fleet; local changes are refused.">managed — read-only</span>`
          : null}
      </header>

      <bm-store-bar></bm-store-bar>

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

      <nav>
        ${TABS.map(
          (t) => html`
            <button
              class=${this.tab === t.id ? "active" : ""}
              @click=${() => (this.tab = t.id)}
            >
              ${t.label}
            </button>
          `,
        )}
      </nav>

      ${this.tab === "detection" ? html`<bm-detection-module></bm-detection-module>` : ""}
      ${this.tab === "censor" ? html`<bm-censor-module></bm-censor-module>` : ""}
      ${this.tab === "challenge" ? html`<bm-challenge-module></bm-challenge-module>` : ""}
      ${this.tab === "exposure" ? html`<bm-exposure-module></bm-exposure-module>` : ""}
      ${this.tab === "coverage" ? html`<bm-coverage-module></bm-coverage-module>` : ""}
      ${this.tab === "exclusions" ? html`<bm-exclusions-module></bm-exclusions-module>` : ""}
      ${this.tab === "earned" ? html`<bm-earned-module></bm-earned-module>` : ""}
      ${this.tab === "focus" ? html`<bm-focus-module></bm-focus-module>` : ""}
      ${this.tab === "clock" ? html`<bm-clock-module></bm-clock-module>` : ""}
      ${this.tab === "layers" ? html`<bm-layers></bm-layers>` : ""}
      ${this.tab === "assignment" ? html`<bm-assignment></bm-assignment>` : ""}
    `;
  }
}
