// Named configurations, the layer stack, and package import/export.

import { css, html, LitElement } from "lit";
import { customElement } from "lit/decorators.js";
import type { ModulePatches, NamedConfig } from "../schema.js";
import { store } from "../store.js";
import "./controls.js";

function summarize(settings: ModulePatches): string {
  const parts: string[] = [];
  for (const [module, patch] of Object.entries(settings)) {
    if (!patch) continue;
    const fields = Object.keys(patch);
    if (fields.length) parts.push(`${module}: ${fields.join(", ")}`);
  }
  return parts.join(" · ") || "sets nothing";
}

@customElement("bm-layers")
export class BmLayers extends LitElement {
  static styles = css`
    :host {
      display: block;
    }
    .config {
      display: grid;
      grid-template-columns: auto 1fr auto;
      gap: 2px 12px;
      align-items: center;
      padding: 10px 0;
      border-bottom: 1px solid var(--border);
    }
    .config:last-child {
      border-bottom: none;
    }
    .name {
      font-weight: 600;
    }
    .desc,
    .sets {
      grid-column: 2;
      color: var(--muted);
      font-size: 12.5px;
    }
    .order button,
    .actions button {
      border: 1px solid var(--border);
      background: var(--bg);
      color: var(--text);
      border-radius: 6px;
      padding: 3px 10px;
      cursor: pointer;
      font: inherit;
      font-size: 13px;
    }
    .order button[disabled] {
      opacity: 0.35;
      cursor: default;
    }
    .actions {
      display: flex;
      gap: 10px;
      flex-wrap: wrap;
      padding: 14px 0;
    }
    input[type="checkbox"] {
      width: 18px;
      height: 18px;
      accent-color: var(--accent);
    }
    .muted {
      color: var(--muted);
      font-size: 12.5px;
    }
    .stack {
      color: var(--muted);
      font-size: 13px;
      padding: 8px 0 0;
    }
    .stack b {
      color: var(--text);
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

  private toggleLayer(name: string, on: boolean) {
    store.update((pkg) => {
      pkg.layers = pkg.layers.filter((l) => l !== name);
      if (on) pkg.layers.push(name);
    });
  }

  private move(name: string, delta: number) {
    store.update((pkg) => {
      const i = pkg.layers.indexOf(name);
      const j = i + delta;
      if (i < 0 || j < 0 || j >= pkg.layers.length) return;
      [pkg.layers[i], pkg.layers[j]] = [pkg.layers[j]!, pkg.layers[i]!];
    });
  }

  private exportPackage() {
    const blob = new Blob([JSON.stringify(store.pkg, null, 2)], {
      type: "application/json",
    });
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = "betamacs-package.json";
    a.click();
    URL.revokeObjectURL(a.href);
  }

  private importPackage() {
    const input = document.createElement("input");
    input.type = "file";
    input.accept = "application/json";
    input.onchange = async () => {
      const file = input.files?.[0];
      if (!file) return;
      try {
        const pkg = JSON.parse(await file.text());
        store.update((p) => {
          p.namedConfigs = pkg.namedConfigs ?? [];
          p.layers = pkg.layers ?? [];
          p.overrides = pkg.overrides ?? {};
          p.version = pkg.version ?? 1;
        });
        store.setStatus("package imported");
      } catch (e) {
        store.setStatus(`import failed: ${e}`);
      }
    };
    input.click();
  }

  private saveOverridesAsConfig() {
    const name = prompt("Name for the new named configuration:");
    if (!name) return;
    store.update((pkg) => {
      const config: NamedConfig = {
        name,
        description: "Saved from overrides",
        settings: JSON.parse(JSON.stringify(pkg.overrides)),
      };
      pkg.namedConfigs = [...pkg.namedConfigs.filter((c) => c.name !== name), config];
      pkg.overrides = {};
      pkg.layers.push(name);
    });
  }

  render() {
    const pkg = store.pkg;
    return html`
      <bm-section heading="Layer stack">
        <p class="muted">
          Effective settings = defaults, then each active named configuration in
          order (later wins), then your overrides on top. A named configuration
          only affects the options it sets.
        </p>
        <div class="stack">
          defaults
          ${pkg.layers.map((l) => html` → <b>${l}</b>`)}
          → <b>overrides</b>
        </div>
      </bm-section>

      <bm-section heading="Named configurations">
        ${pkg.namedConfigs.length === 0
          ? html`<p class="muted">
              None yet — pull from the app to get the starter set, or import a
              package.
            </p>`
          : ""}
        ${pkg.namedConfigs.map((config) => {
          const active = pkg.layers.includes(config.name);
          const idx = pkg.layers.indexOf(config.name);
          return html`
            <div class="config">
              <input
                type="checkbox"
                .checked=${active}
                title="Apply this configuration"
                @change=${(e: Event) =>
                  this.toggleLayer(config.name, (e.target as HTMLInputElement).checked)}
              />
              <span class="name">${config.name}</span>
              <span class="order">
                <button
                  ?disabled=${!active || idx === 0}
                  @click=${() => this.move(config.name, -1)}
                >
                  ↑
                </button>
                <button
                  ?disabled=${!active || idx === pkg.layers.length - 1}
                  @click=${() => this.move(config.name, 1)}
                >
                  ↓
                </button>
              </span>
              ${config.description
                ? html`<span class="desc">${config.description}</span>`
                : ""}
              <span class="sets">${summarize(config.settings)}</span>
            </div>
          `;
        })}
      </bm-section>

      <bm-section heading="Package">
        <div class="actions">
          <button @click=${this.exportPackage}>Export JSON</button>
          <button @click=${() => this.importPackage()}>Import JSON</button>
          <button @click=${() => this.saveOverridesAsConfig()}>
            Save overrides as named config
          </button>
          <button
            @click=${() =>
              confirm("Clear all overrides?") &&
              store.update((pkg) => (pkg.overrides = {}))}
          >
            Clear overrides
          </button>
        </div>
      </bm-section>
    `;
  }
}
