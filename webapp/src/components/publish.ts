// The Store bar: front-and-center config-app control surface.
// Select/configure a Store (app + channel + otactl backend + typeserver
// endpoints), LOAD the current published config (via the typeserver read
// endpoint, or by pasting exported JSON), see a dirty/diff indicator vs the
// loaded baseline, and PUBLISH a signed, resolved config to the store.

import { css, html, LitElement, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";
import { store, type StoreConfig } from "../store.js";
import type { PublishResult } from "../stores/store.js";

const LAST_VERSION_KEY = "betamacs.lastVersion";

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
    .errbox {
      margin-top: 12px;
      border: 1px solid color-mix(in srgb, #ff3b30 55%, transparent);
      background: color-mix(in srgb, #ff3b30 12%, transparent);
      border-radius: 10px;
      padding: 10px 12px;
      font-size: 12.5px;
    }
    .errbox .title {
      font-weight: 600;
      color: #c00;
      margin-bottom: 6px;
    }
    .errbox pre {
      margin: 0;
      white-space: pre-wrap;
      word-break: break-word;
      font: 12px/1.45 ui-monospace, monospace;
      color: var(--text);
      max-height: 220px;
      overflow: auto;
    }
    .okbox {
      margin-top: 12px;
      border: 1px solid color-mix(in srgb, #34c759 55%, transparent);
      background: color-mix(in srgb, #34c759 12%, transparent);
      border-radius: 10px;
      padding: 10px 12px;
      font-size: 12.5px;
    }
    .okbox .title {
      font-weight: 600;
      color: #1a8a3a;
      margin-bottom: 6px;
    }
    .okbox dl {
      display: grid;
      grid-template-columns: auto 1fr;
      gap: 2px 12px;
      margin: 0;
    }
    .okbox dt {
      color: var(--muted);
    }
    .okbox dd {
      margin: 0;
      font-variant-numeric: tabular-nums;
      word-break: break-all;
    }
  `;

  @state() private open = false;
  @state() private importing = false;
  @state() private importText = "";
  @state() private version = nextVersion();
  @state() private busy = false;
  @state() private lastError = "";
  @state() private lastResult: PublishResult | null = null;

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
      const { version, sha256 } = await store.loadFromStore();
      const shortSha = sha256 ? sha256.slice(0, 12) : "?";
      store.setStatus(
        `loaded published config v${version || "?"} (sha256 ${shortSha}…, baseline set)`,
      );
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
    const version = this.version.trim();
    if (!version) {
      this.lastResult = null;
      this.lastError = "Version is required (e.g. 0.1.11). otactl rejects a blank version.";
      return;
    }
    if (!confirm(`Publish ${store.storeConfig.app} v${version} to channel "${store.storeConfig.channel}"?`))
      return;
    this.busy = true;
    this.lastError = "";
    this.lastResult = null;
    store.setStatus("publishing (sign + upload)…");
    try {
      const r = await store.publishToStore(version);
      this.lastResult = r;
      store.setStatus("published");
      // Remember the version so the next suggestion increments past it.
      try {
        localStorage.setItem(LAST_VERSION_KEY, version);
      } catch {
        /* ignore */
      }
      this.version = nextVersion();
    } catch (e) {
      // Surface the FULL server error (status + body) prominently — the
      // backend puts otactl's stderr in the body on a 502.
      this.lastError = e instanceof Error ? e.message : String(e);
      store.setStatus("publish failed — see details below");
    } finally {
      this.busy = false;
    }
  }

  render() {
    const cfg = store.storeConfig;
    const dirty = store.dirty;
    const diff = dirty ? store.diff() : [];
    const canPublish = !!cfg.publishEndpoint && !!this.version.trim() && !this.busy;

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
                ${this.field("publisherId", "Publisher id", "(server default)")}
                <label>otactl backend URL</label>
                <input
                  class="full"
                  .value=${cfg.backendUrl}
                  placeholder="(server default — leave blank to use OTACTL_BACKEND_URL)"
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
            placeholder="0.1.11"
            .value=${this.version}
            @input=${(e: Event) => (this.version = (e.target as HTMLInputElement).value)}
          />
          <button class="primary" @click=${this.publish} ?disabled=${!canPublish}>
            ${this.busy ? "Publishing…" : "Publish to store"}
          </button>
          ${!cfg.publishEndpoint
            ? html`<span class="target">set a publish endpoint to enable</span>`
            : !this.version.trim()
              ? html`<span class="target">enter a version to enable</span>`
              : nothing}
        </div>

        <div class="status">${store.status}</div>

        ${this.lastError
          ? html`
              <div class="errbox">
                <div class="title">Publish failed</div>
                <pre>${this.lastError}</pre>
              </div>
            `
          : nothing}
        ${this.lastResult
          ? html`
              <div class="okbox">
                <div class="title">Published ✓</div>
                <dl>
                  <dt>version</dt><dd>${this.lastResult.version}</dd>
                  <dt>sha256</dt><dd>${this.lastResult.sha256}</dd>
                  ${this.lastResult.epoch !== undefined
                    ? html`<dt>epoch</dt><dd>${this.lastResult.epoch}</dd>`
                    : nothing}
                  ${this.lastResult.notAfter
                    ? html`<dt>sig valid until</dt><dd>${this.lastResult.notAfter}</dd>`
                    : nothing}
                  <dt>stored at</dt><dd>${this.lastResult.storedAt}</dd>
                </dl>
              </div>
            `
          : nothing}
      </div>
    `;
  }
}

/** Suggest the next artifact version: increment the last published one, or
 *  start at 0.1.11 (past the current 0.1.10). User-editable in the UI. */
function nextVersion(): string {
  let last = "";
  try {
    last = localStorage.getItem(LAST_VERSION_KEY) ?? "";
  } catch {
    /* ignore */
  }
  return last ? bumpVersion(last) : "0.1.11";
}

function bumpVersion(v: string): string {
  const semver = v.match(/^(\d+)\.(\d+)\.(\d+)$/);
  if (semver) return `${semver[1]}.${semver[2]}.${Number(semver[3]) + 1}`;
  const trailing = v.match(/^(.*?)(\d+)$/);
  if (trailing) return `${trailing[1]}${Number(trailing[2]) + 1}`;
  return `${v}.1`;
}

function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}
