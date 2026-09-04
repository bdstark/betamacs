// Device assignment (STUB in this pass).
//
// A strongly-typed layer over otactl's device API: list devices and point a
// device at a config by setting its otactl channel. The heavy caveat
// (surfaced in the UI): otactl resolves ONE channel per device for ALL apps
// (resolveChannel reads a single device.Channel), so assigning a config
// channel also moves that device's app + task-bank lanes. See
// docs/config-app.md "Config -> device assignment".
//
// Endpoints are derived from the configured publish endpoint's origin:
//   GET  {origin}/api/betamacs/devices          -> DeviceRow[]
//   POST {origin}/api/betamacs/assign {deviceId, channel}
// Both are documented stubs on the typeserver side in this pass; the UI
// degrades gracefully when they are absent (501 / unconfigured).

import { css, html, LitElement, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";
import { store } from "../store.js";

interface DeviceRow {
  deviceId: string;
  label?: string;
  channel?: string;
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
    .caveat {
      background: color-mix(in srgb, #ff9f0a 14%, transparent);
      border: 1px solid color-mix(in srgb, #ff9f0a 40%, transparent);
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
    }
    th {
      color: var(--muted);
      font-weight: 500;
    }
    input {
      background: var(--bg);
      color: var(--text);
      border: 1px solid var(--border);
      border-radius: 6px;
      padding: 4px 8px;
      font: inherit;
      font-size: 13px;
      width: 120px;
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
    .stub {
      display: inline-block;
      font-size: 11px;
      padding: 1px 7px;
      border-radius: 8px;
      background: color-mix(in srgb, var(--muted) 18%, transparent);
      color: var(--muted);
      margin-left: 8px;
    }
    .status {
      color: var(--muted);
      font-size: 12.5px;
      min-height: 1em;
      margin-top: 10px;
    }
  `;

  @state() private devices: DeviceRow[] = [];
  @state() private edits: Record<string, string> = {};
  @state() private status = "";
  @state() private loaded = false;

  private base(): string | null {
    const ep = store.storeConfig.publishEndpoint;
    if (!ep) return null;
    try {
      return new URL(ep).origin;
    } catch {
      return null;
    }
  }

  private async loadDevices() {
    const base = this.base();
    if (!base) {
      this.status = "configure a publish endpoint first (its origin serves the device API)";
      return;
    }
    this.status = "loading devices…";
    try {
      const res = await fetch(`${base}/api/betamacs/devices`, { credentials: "include" });
      if (res.status === 501) {
        this.status = "device API not wired yet (stub) — assignment is non-functional in this pass";
        this.loaded = true;
        return;
      }
      if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
      this.devices = (await res.json()) as DeviceRow[];
      this.loaded = true;
      this.status = `${this.devices.length} device(s)`;
    } catch (e) {
      this.status = `load failed: ${e}`;
    }
  }

  private async assign(deviceId: string) {
    const base = this.base();
    const channel = this.edits[deviceId] ?? "";
    if (!base || !channel) return;
    if (
      !confirm(
        `Set device ${deviceId} to channel "${channel}"?\n\n` +
          `This moves ALL apps on that device (app, config, tasks) to this channel — ` +
          `otactl has one channel per device.`,
      )
    )
      return;
    this.status = "assigning…";
    try {
      const res = await fetch(`${base}/api/betamacs/assign`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ deviceId, channel }),
      });
      if (res.status === 501) {
        this.status = "assign not wired yet (stub)";
        return;
      }
      if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
      this.status = `assigned ${deviceId} -> ${channel}`;
      await this.loadDevices();
    } catch (e) {
      this.status = `assign failed: ${e}`;
    }
  }

  render() {
    return html`
      <bm-section heading="Device assignment">
        <div class="caveat">
          <b>Single channel per device.</b> otactl resolves one
          <code>device.Channel</code> for every app. Assigning a config channel
          here also moves that device's betamacs app build and task bank to the
          same channel. Per-app channels are a proposed otactl change
          (docs/config-app.md); until then prefer a shared channel +
          per-kid entitlements, or accept the coupling.
        </div>
        <p class="muted">
          List devices from otactl (via the typeserver device API) and point one
          at a channel. <span class="stub">stub — may be non-functional</span>
        </p>
        <div style="margin:8px 0">
          <button @click=${this.loadDevices}>Load devices</button>
        </div>
        ${this.loaded && this.devices.length
          ? html`
              <table>
                <thead>
                  <tr>
                    <th>Device</th>
                    <th>Current channel</th>
                    <th>New channel</th>
                    <th></th>
                  </tr>
                </thead>
                <tbody>
                  ${this.devices.map(
                    (d) => html`
                      <tr>
                        <td>${d.label ?? d.deviceId}</td>
                        <td>${d.channel ?? "—"}</td>
                        <td>
                          <input
                            .value=${this.edits[d.deviceId] ?? d.channel ?? ""}
                            @change=${(e: Event) =>
                              (this.edits = {
                                ...this.edits,
                                [d.deviceId]: (e.target as HTMLInputElement).value,
                              })}
                          />
                        </td>
                        <td><button @click=${() => this.assign(d.deviceId)}>Assign</button></td>
                      </tr>
                    `,
                  )}
                </tbody>
              </table>
            `
          : nothing}
        <div class="status">${this.status}</div>
      </bm-section>
    `;
  }
}
