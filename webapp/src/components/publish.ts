// The Store bar: front-and-center config-app control surface.
// Select/configure a Store (app + channel + otactl backend + typeserver
// endpoints), LOAD the current published config (via the typeserver read
// endpoint, or by pasting exported JSON), see a dirty/diff indicator vs the
// loaded baseline, and PUBLISH a signed, resolved config to the store.

import { css, html, LitElement, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";
import { store, type StoreConfig } from "../store.js";

@customElement("bm-store-bar")
export class BmStoreBar extends LitElement {
  static styles = css`
    :host {
      display: block;
      margin-bottom: 16px;
    }
    .panel {
      background: var(--panel);
      border: 1px solid var(--border);
      border-radius: 12px;
      padding: 12px 14px;
    }
    .head {
      display: flex;
      align-items: center;
      gap: 10px;
      flex-wrap: wrap;
    }
    .head h2 {
      font-size: 15px;
      margin: 0;
    }
    .spacer {
      flex: 1;
    }
    .target {
      color: var(--muted);
      font-size: 13px;
      font-variant-numeric: tabular-nums;
    }
    .target b {
      color: var(--text);
    }
    button {
      border: 1px solid var(--border);
      background: var(--bg);
      color: var(--text);
      border-radius: 6px;
      padding: 5px 12px;
      cursor: pointer;
      font: inherit;
      font-size: 13px;
    }
    button.primary {
      background: var(--accent);
      border-color: var(--accent);
      color: #fff;
      font-weight: 600;
    }
    button:disabled {
      opacity: 0.4;
      cursor: default;
    }
    .badge {
      font-size: 11.5px;
      padding: 2px 9px;
      border-radius: 8px;
      white-space: nowrap;
    }
    .badge.clean {
      background: color-mix(in srgb, var(--muted) 15%, transparent);
      color: var(--muted);
    }
    .badge.dirty {
      background: color-mix(in srgb, #ff9f0a 22%, transparent);
      color: #c77700;
    }
    .grid {
      display: grid;
      grid-template-columns: auto 1fr auto 1fr;
      gap: 8px 10px;
      align-items: center;
      margin-top: 12px;
      padding-top: 12px;
      border-top: 1px solid var(--border);
    }
    label {
      color: var(--muted);
      font-size: 12.5px;
    }
    input {
      background: var(--bg);
      color: var(--text);
      border: 1px solid var(--border);
      border-radius: 6px;
      padding: 5px 9px;
      font: inherit;
      font-size: 13px;
      width: 100%;
      box-sizing: border-box;
    }
    .full {
      grid-column: 2 / -1;
    }
    .diff {
      margin-top: 12px;
      padding-top: 10px;
      border-top: 1px solid var(--border);
      font-size: 12.5px;
      color: var(--muted);
    }
    .diff ul {
      margin: 6px 0 0;
      padding-left: 18px;
      columns: 2;
    }
    .diff code {
      color: var(--text);
    }
    textarea {
      width: 100%;
      box-sizing: border-box;
      min-height: 120px;
      background: var(--bg);
      color: var(--text);
      border: 1px solid var(--border);
      border-radius: 6px;
      padding: 8px;
      font: 12px/1.4 ui-monospace, monospace;
      margin-top: 10px;
    }
    .row {
      display: flex;
      gap: 8px;
      margin-top: 8px;
      align-items: center;
      flex-wrap: wrap;
    }
    .status {
      color: var(--muted);
      font-size: 12.5px;
      min-height: 1em;
      margin-top: 8px;
    }
  `;

  @state() private open = false;
  @state() private importing = false;
  @state() private importText = "";
  @state() private version = suggestedVersion();
  @state() private busy = false;

  private unsubscribe = () => {};
  connectedCallback() {
    super.connectedCallback();
    this.unsubscribe = store.subscribe(() => this.requestUpdate());
  }
  disconnectedCallback() {
    super.disconnectedCallback();
    this.unsubscribe();
  }

  private field(key: keyof StoreConfig, label: string, placeholder = "") {
    return html`
      <label>${label}</label>
      <input
        .value=${String(store.storeConfig[key])}
        placeholder=${placeholder}
        @change=${(e: Event) =>
          store.setStoreConfig({
            ...store.storeConfig,
            [key]: (e.target as HTMLInputElement).value,
          })}
      />
    `;
  }

  private async load() {
    this.busy = true;
    store.setStatus("loading from store…");
    try {
      await store.loadFromStore();
      store.setStatus("loaded current published config (baseline set)");
    } catch (e) {
      store.setStatus(`load failed: ${e}`);
    } finally {
      this.busy = false;
    }
  }

  private doImport() {
    try {
      store.importJson(this.importText);
      this.importing = false;
      this.importText = "";
    } catch (e) {
      store.setStatus(`import failed: ${e}`);
    }
  }

  private async publish() {
    if (!confirm(`Publish ${store.storeConfig.app} v${this.version} to channel "${store.storeConfig.channel}"?`))
      return;
    this.busy = true;
    store.setStatus("publishing (sign + upload)…");
    try {
      const r = await store.publishToStore(this.version);
      store.setStatus(
        `published v${r.version} · sha256 ${r.sha256.slice(0, 12)}…` +
          (r.epoch !== undefined ? ` · epoch ${r.epoch}` : "") +
          (r.notAfter ? ` · sig valid until ${r.notAfter}` : ""),
      );
      this.version = suggestedVersion();
    } catch (e) {
      store.setStatus(`publish failed: ${e}`);
    } finally {
      this.busy = false;
    }
  }

  render() {
    const cfg = store.storeConfig;
    const dirty = store.dirty;
    const diff = dirty ? store.diff() : [];
    const canPublish = !!cfg.publishEndpoint && !!cfg.backendUrl && !this.busy;

    return html`
      <div class="panel">
        <div class="head">
          <h2>Store</h2>
          <span class="target">
            <b>${cfg.app}</b> · channel <b>${cfg.channel}</b>
            ${cfg.backendUrl ? html`· <b>${hostOf(cfg.backendUrl)}</b>` : nothing}
          </span>
          <span class="badge ${dirty ? "dirty" : "clean"}">
            ${dirty ? `${diff.length} staged change${diff.length === 1 ? "" : "s"}` : "in sync"}
          </span>
          <span class="spacer"></span>
          <button @click=${() => (this.open = !this.open)}>
            ${this.open ? "Hide config" : "Configure"}
          </button>
          <button @click=${() => (this.importing = !this.importing)}>Import JSON</button>
          <button @click=${this.load} ?disabled=${!cfg.readEndpoint || this.busy}>
            Load from store
          </button>
        </div>

        ${this.open
          ? html`
              <div class="grid">
                ${this.field("app", "App", "betamacs-config")}
                ${this.field("channel", "Channel", "stable")}
                ${this.field("arch", "Arch", "arm64")}
                ${this.field("publisherId", "Publisher id", "betamacs-config")}
                <label>otactl backend URL</label>
                <input
                  class="full"
                  .value=${cfg.backendUrl}
                  placeholder="https://<otactl device origin>"
                  @change=${(e: Event) =>
                    store.setStoreConfig({
                      ...cfg,
                      backendUrl: (e.target as HTMLInputElement).value,
                    })}
                />
                <label>Publish endpoint</label>
                <input
                  class="full"
                  .value=${cfg.publishEndpoint}
                  placeholder="https://typeserver…/api/betamacs/publish"
                  @change=${(e: Event) =>
                    store.setStoreConfig({
                      ...cfg,
                      publishEndpoint: (e.target as HTMLInputElement).value,
                    })}
                />
                <label>Read endpoint</label>
                <input
                  class="full"
                  .value=${cfg.readEndpoint}
                  placeholder="https://typeserver…/api/betamacs/config"
                  @change=${(e: Event) =>
                    store.setStoreConfig({
                      ...cfg,
                      readEndpoint: (e.target as HTMLInputElement).value,
                    })}
                />
              </div>
            `
          : nothing}

        ${this.importing
          ? html`
              <textarea
                placeholder="Paste an exported package.json or an author-signed wrapper…"
                .value=${this.importText}
                @input=${(e: Event) => (this.importText = (e.target as HTMLTextAreaElement).value)}
              ></textarea>
              <div class="row">
                <button class="primary" @click=${this.doImport} ?disabled=${!this.importText.trim()}>
                  Import & set baseline
                </button>
                <button @click=${() => (this.importing = false)}>Cancel</button>
              </div>
            `
          : nothing}

        ${dirty && diff.length
          ? html`
              <div class="diff">
                Staged edits vs. baseline:
                <ul>
                  ${diff.slice(0, 40).map((p) => html`<li><code>${p}</code></li>`)}
                </ul>
              </div>
            `
          : nothing}

        <div class="row">
          <label>Version</label>
          <input
            style="width:180px;flex:none"
            .value=${this.version}
            @change=${(e: Event) => (this.version = (e.target as HTMLInputElement).value)}
          />
          <button class="primary" @click=${this.publish} ?disabled=${!canPublish}>
            Publish to store
          </button>
          ${!cfg.publishEndpoint
            ? html`<span class="target">set a publish endpoint to enable</span>`
            : nothing}
        </div>

        <div class="status">${store.status}</div>
      </div>
    `;
  }
}

function suggestedVersion(): string {
  const d = new Date();
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}.${p(d.getMonth() + 1)}.${p(d.getDate())}-${p(d.getHours())}${p(d.getMinutes())}`;
}

function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}
