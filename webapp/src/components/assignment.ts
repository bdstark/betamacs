// Device assignment.
//
// A strongly-typed layer over otactl's device API (via typeserver): list
// devices and point one at a betamacs-config channel. otactl now supports a
// per-app channel override (AppChannels), so setting the config channel here
// moves ONLY the device's betamacs-config lane — its betamacs app build and
// betamacs-tasks bank stay on the device-wide channel. See docs/config-app.md
// "Config -> device assignment".
//
// Endpoints are derived from the configured publish endpoint's origin:
//   GET  {origin}/api/betamacs/devices          -> { devices: DeviceRow[] }
//   POST {origin}/api/betamacs/assign {deviceId, channel}   (empty clears)
// Session-gated on typeserver (the ts_session cookie rides along).

import { css, html, LitElement, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";
import { store } from "../store.js";

interface DeviceRow {
  deviceId: string;
  description?: string;
  arch?: string;
  /** Device-wide channel (governs the app + tasks lanes). */
  channel?: string;
  /** Per-app betamacs-config channel override; empty = follows `channel`. */
  appChannel?: string;
  lastSeen?: string;
}

@customElement("bm-assignment")
export class BmAssignment extends LitElement {
  static styles = css`
    :host {
      display: block;
    }
    .muted {
      color: var(--muted);
      font-size: 12.5px;
    }
    .note {
      background: color-mix(in srgb, var(--accent, #0a84ff) 10%, transparent);
      border: 1px solid color-mix(in srgb, var(--accent, #0a84ff) 32%, transparent);
      border-radius: 10px;
      padding: 10px 12px;
      font-size: 12.5px;
      margin-bottom: 14px;
    }
    table {
      width: 100%;
      border-collapse: collapse;
      font-size: 13px;
    }
    th,
    td {
      text-align: left;
      padding: 8px 6px;
      border-bottom: 1px solid var(--border);
      vertical-align: middle;
    }
    th {
      color: var(--muted);
      font-weight: 500;
    }
    code {
      font: 12px/1.4 ui-monospace, monospace;
    }
    .pill {
      display: inline-block;
      font-size: 12px;
      padding: 1px 8px;
      border-radius: 8px;
      background: color-mix(in srgb, var(--muted) 16%, transparent);
      color: var(--text);
    }
    .pill.override {
      background: color-mix(in srgb, var(--accent, #0a84ff) 22%, transparent);
    }
    input {
      background: var(--bg);
      color: var(--text);
      border: 1px solid var(--border);
      border-radius: 6px;
      padding: 4px 8px;
      font: inherit;
      font-size: 13px;
      width: 130px;
    }
    button {
      border: 1px solid var(--border);
      background: var(--bg);
      color: var(--text);
      border-radius: 6px;
      padding: 4px 10px;
      cursor: pointer;
      font: inherit;
      font-size: 13px;
    }
    button:disabled {
      opacity: 0.5;
      cursor: default;
    }
    .rowbtns {
      display: flex;
      gap: 6px;
    }
    .status {
      color: var(--muted);
      font-size: 12.5px;
      min-height: 1em;
      margin-top: 12px;
    }
    .status.ok {
      color: var(--ok, #34c759);
      font-weight: 500;
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
  `;

  @state() private devices: DeviceRow[] = [];
  @state() private edits: Record<string, string> = {};
  @state() private status = "";
  @state() private ok = false;
  @state() private error = "";
  @state() private loaded = false;
  @state() private busy = false;

  connectedCallback(): void {
    super.connectedCallback();
    // Load on open; degrade to a clear message if the endpoint is unset.
    void this.loadDevices();
  }

  private base(): string | null {
    const ep = store.storeConfig.publishEndpoint;
    if (!ep) return null;
    try {
      return new URL(ep, location.origin).origin;
    } catch {
      return null;
    }
  }

  private async loadDevices() {
    const base = this.base();
    if (!base) {
      this.status = "configure a publish endpoint first (its origin serves the device API)";
      this.ok = false;
      return;
    }
    this.busy = true;
    this.error = "";
    this.status = "loading devices…";
    this.ok = false;
    try {
      const res = await fetch(`${base}/api/betamacs/devices`, { credentials: "include" });
      if (!res.ok) {
        throw new Error(`${res.status} ${res.statusText}\n${(await res.text()).trim()}`);
      }
      const body = (await res.json()) as { devices?: DeviceRow[] };
      this.devices = body.devices ?? [];
      this.loaded = true;
      this.status = `${this.devices.length} device(s)`;
      this.ok = false;
    } catch (e) {
      this.error = String(e instanceof Error ? e.message : e);
      this.status = "load failed";
      this.ok = false;
    } finally {
      this.busy = false;
    }
  }

  private currentConfigChannel(d: DeviceRow): string {
    return (d.appChannel ?? "").trim();
  }

  private editValue(d: DeviceRow): string {
    return this.edits[d.deviceId] ?? this.currentConfigChannel(d);
  }

  private async assign(deviceId: string, channel: string) {
    const base = this.base();
    if (!base) return;
    const verb = channel ? `set to config channel "${channel}"` : "cleared (falls back to device channel)";
    this.busy = true;
    this.error = "";
    this.status = channel ? "assigning…" : "clearing…";
    this.ok = false;
    try {
      const res = await fetch(`${base}/api/betamacs/assign`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ deviceId, channel }),
      });
      if (!res.ok) {
        throw new Error(`${res.status} ${res.statusText}\n${(await res.text()).trim()}`);
      }
      this.status = `${deviceId} ${verb}`;
      this.ok = true;
      // Drop the local edit so the refreshed server value shows through.
      const { [deviceId]: _drop, ...rest } = this.edits;
      this.edits = rest;
      await this.loadDevices();
      this.ok = true;
    } catch (e) {
      this.error = String(e instanceof Error ? e.message : e);
      this.status = "assignment failed";
      this.ok = false;
    } finally {
      this.busy = false;
    }
  }

  render() {
    return html`
      <bm-section heading="Device assignment">
        <div class="note">
          <b>Per-app config channel.</b> Setting a device's
          <code>betamacs-config</code> channel here moves only its config lane —
          the device's betamacs app build and betamacs-tasks bank stay on its
          device-wide channel. Leave the field empty and Clear to remove the
          override so config follows the device channel again.
        </div>
        <div style="margin:8px 0; display:flex; gap:8px; align-items:center;">
          <button @click=${this.loadDevices} ?disabled=${this.busy}>
            ${this.busy ? "…" : "Refresh"}
          </button>
        </div>
        ${this.loaded && this.devices.length
          ? html`
              <table>
                <thead>
                  <tr>
                    <th>Device</th>
                    <th>Device channel</th>
                    <th>Config channel</th>
                    <th>Set config channel</th>
                    <th></th>
                  </tr>
                </thead>
                <tbody>
                  ${this.devices.map((d) => {
                    const cur = this.currentConfigChannel(d);
                    const edit = this.editValue(d);
                    const changed = edit.trim() !== cur;
                    return html`
                      <tr>
                        <td>
                          <div>${d.description || d.deviceId}</div>
                          ${d.description
                            ? html`<div class="muted"><code>${d.deviceId}</code></div>`
                            : nothing}
                        </td>
                        <td>${d.channel ? html`<span class="pill">${d.channel}</span>` : "—"}</td>
                        <td>
                          ${cur
                            ? html`<span class="pill override">${cur}</span>`
                            : html`<span class="muted">follows device</span>`}
                        </td>
                        <td>
                          <input
                            .value=${edit}
                            placeholder=${d.channel ?? "channel"}
                            ?disabled=${this.busy}
                            @input=${(e: Event) =>
                              (this.edits = {
                                ...this.edits,
                                [d.deviceId]: (e.target as HTMLInputElement).value,
                              })}
                          />
                        </td>
                        <td>
                          <div class="rowbtns">
                            <button
                              ?disabled=${this.busy || !changed}
                              @click=${() => this.assign(d.deviceId, edit.trim())}
                            >
                              ${edit.trim() ? "Assign" : "Clear"}
                            </button>
                            ${cur
                              ? html`<button
                                  ?disabled=${this.busy}
                                  @click=${() => this.assign(d.deviceId, "")}
                                >
                                  Clear
                                </button>`
                              : nothing}
                          </div>
                        </td>
                      </tr>
                    `;
                  })}
                </tbody>
              </table>
            `
          : this.loaded
            ? html`<p class="muted">No devices returned.</p>`
            : nothing}
        <div class="status ${this.ok ? "ok" : ""}">${this.status}</div>
        ${this.error
          ? html`<div class="errbox">
              <div class="title">Error</div>
              <pre>${this.error}</pre>
            </div>`
          : nothing}
      </bm-section>
    `;
  }
}
