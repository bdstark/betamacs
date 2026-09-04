// Earned-time editor: a gate on internet access unlocked by approved activity
// (docs/earned-time.md). The most complex editor — two dynamic lists (schedule
// windows and earn sources) plus the accounting scalars.

import { css, html, LitElement, nothing } from "lit";
import { customElement } from "lit/decorators.js";
import {
  valueSource,
  type EarnSource,
  type EarnedTimePatch,
  type Schedule,
} from "../schema.js";
import { store } from "../store.js";
import "../components/controls.js";

const DAYS: { key: string; label: string }[] = [
  { key: "mon", label: "Mon" },
  { key: "tue", label: "Tue" },
  { key: "wed", label: "Wed" },
  { key: "thu", label: "Thu" },
  { key: "fri", label: "Fri" },
  { key: "sat", label: "Sat" },
  { key: "sun", label: "Sun" },
];

function setOverride<K extends keyof EarnedTimePatch>(
  key: K,
  value: EarnedTimePatch[K],
): void {
  store.update((pkg) => {
    pkg.overrides.earnedTime = { ...pkg.overrides.earnedTime, [key]: value };
  });
}

function clearOverride(key: keyof EarnedTimePatch): void {
  store.update((pkg) => {
    if (pkg.overrides.earnedTime) delete pkg.overrides.earnedTime[key];
  });
}

/** Deep-copy the effective schedule, mutate, and write it back as an override
 *  (never mutate the effective/default array in place). */
function editSchedule(mut: (arr: Schedule[]) => void): void {
  const arr: Schedule[] = store.effective.earnedTime.schedule.map((s) => ({
    days: [...s.days],
    from: s.from,
    to: s.to,
  }));
  mut(arr);
  setOverride("schedule", arr);
}

function editSources(mut: (arr: EarnSource[]) => void): void {
  const arr: EarnSource[] = store.effective.earnedTime.sources.map((s) => ({
    name: s.name,
    match: { ...s.match },
    earnRatio: s.earnRatio,
  }));
  mut(arr);
  setOverride("sources", arr);
}

@customElement("bm-earned-module")
export class BmEarnedModule extends LitElement {
  static styles = css`
    :host {
      display: block;
    }
    .muted {
      color: var(--muted);
      font-size: 12.5px;
    }
    .rowlabel {
      display: flex;
      align-items: center;
      gap: 8px;
      margin: 12px 0 6px;
    }
    .rowlabel .badge {
      font-size: 11px;
      padding: 1px 7px;
      border-radius: 8px;
      background: color-mix(in srgb, var(--accent) 15%, transparent);
      color: var(--accent);
    }
    .rowlabel button.reset {
      border: none;
      background: none;
      color: var(--accent);
      cursor: pointer;
      font-size: 12px;
      padding: 0;
    }
    .card {
      border: 1px solid var(--border);
      border-radius: 10px;
      padding: 10px 12px;
      margin-bottom: 10px;
    }
    .days {
      display: flex;
      gap: 4px;
      flex-wrap: wrap;
      margin-bottom: 8px;
    }
    .day {
      border: 1px solid var(--border);
      background: var(--bg);
      color: var(--muted);
      border-radius: 6px;
      padding: 3px 9px;
      cursor: pointer;
      font: inherit;
      font-size: 12.5px;
    }
    .day.on {
      background: var(--accent);
      border-color: var(--accent);
      color: #fff;
    }
    .fields {
      display: flex;
      gap: 10px;
      align-items: center;
      flex-wrap: wrap;
    }
    .fields label {
      color: var(--muted);
      font-size: 12px;
    }
    input[type="time"],
    input[type="text"],
    input[type="number"] {
      background: var(--bg);
      color: var(--text);
      border: 1px solid var(--border);
      border-radius: 6px;
      padding: 4px 8px;
      font: inherit;
      font-size: 13px;
    }
    input[type="number"] {
      width: 76px;
    }
    input.name {
      width: 160px;
    }
    input.match {
      width: 190px;
    }
    button.add,
    button.remove {
      border: 1px solid var(--border);
      background: var(--bg);
      color: var(--text);
      border-radius: 6px;
      padding: 4px 10px;
      cursor: pointer;
      font: inherit;
      font-size: 13px;
    }
    button.remove {
      margin-left: auto;
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

  private listLabel(field: string, label: string) {
    const s = valueSource(store.pkg, "earnedTime", field);
    return html`
      <div class="rowlabel">
        <strong>${label}</strong>
        <span class="badge">${s}</span>
        ${s === "override"
          ? html`<button
              class="reset"
              @click=${() => clearOverride(field as keyof EarnedTimePatch)}
            >
              reset
            </button>`
          : nothing}
      </div>
    `;
  }

  private renderSchedule() {
    const windows = store.effective.earnedTime.schedule;
    return html`
      ${this.listLabel("schedule", "Gate windows (when the gate is active)")}
      <p class="muted">No windows means the gate never activates.</p>
      ${windows.map(
        (w, i) => html`
          <div class="card">
            <div class="days">
              ${DAYS.map(
                (d) => html`
                  <button
                    class="day ${w.days.includes(d.key) ? "on" : ""}"
                    @click=${() =>
                      editSchedule((arr) => {
                        const win = arr[i];
                        if (!win) return;
                        win.days = win.days.includes(d.key)
                          ? win.days.filter((x) => x !== d.key)
                          : [...win.days, d.key];
                      })}
                  >
                    ${d.label}
                  </button>
                `,
              )}
            </div>
            <div class="fields">
              <label>from</label>
              <input
                type="time"
                .value=${w.from}
                @change=${(e: Event) =>
                  editSchedule((arr) => {
                    const win = arr[i];
                    if (win) win.from = (e.target as HTMLInputElement).value;
                  })}
              />
              <label>to</label>
              <input
                type="time"
                .value=${w.to}
                @change=${(e: Event) =>
                  editSchedule((arr) => {
                    const win = arr[i];
                    if (win) win.to = (e.target as HTMLInputElement).value;
                  })}
              />
              <button
                class="remove"
                @click=${() => editSchedule((arr) => arr.splice(i, 1))}
              >
                Remove
              </button>
            </div>
          </div>
        `,
      )}
      <button
        class="add"
        @click=${() =>
          editSchedule((arr) =>
            arr.push({ days: ["mon", "tue", "wed", "thu", "fri"], from: "07:00", to: "20:00" }),
          )}
      >
        Add window
      </button>
    `;
  }

  private renderSources() {
    const sources = store.effective.earnedTime.sources;
    return html`
      ${this.listLabel("sources", "Earn sources (what earns credit)")}
      <p class="muted">
        A source matches by app bundle id and/or browser host suffix. Earn ratio
        is earned-minutes per active-minute.
      </p>
      ${sources.map(
        (s, i) => html`
          <div class="card">
            <div class="fields">
              <label>name</label>
              <input
                class="name"
                type="text"
                placeholder="Khan Academy"
                .value=${s.name}
                @change=${(e: Event) =>
                  editSources((arr) => {
                    const src = arr[i];
                    if (src) src.name = (e.target as HTMLInputElement).value;
                  })}
              />
              <label>earn ratio</label>
              <input
                type="number"
                step="0.1"
                min="0"
                .value=${String(s.earnRatio)}
                @change=${(e: Event) =>
                  editSources((arr) => {
                    const src = arr[i];
                    if (src) src.earnRatio = Number((e.target as HTMLInputElement).value);
                  })}
              />
              <button
                class="remove"
                @click=${() => editSources((arr) => arr.splice(i, 1))}
              >
                Remove
              </button>
            </div>
            <div class="fields" style="margin-top:8px">
              <label>bundle id</label>
              <input
                class="match"
                type="text"
                placeholder="org.khanacademy.kids"
                .value=${s.match.bundleId ?? ""}
                @change=${(e: Event) =>
                  editSources((arr) => {
                    const src = arr[i];
                    if (src) src.match.bundleId = (e.target as HTMLInputElement).value || undefined;
                  })}
              />
              <label>host suffix</label>
              <input
                class="match"
                type="text"
                placeholder="khanacademy.org"
                .value=${s.match.browserHostSuffix ?? ""}
                @change=${(e: Event) =>
                  editSources((arr) => {
                    const src = arr[i];
                    if (src)
                      src.match.browserHostSuffix =
                        (e.target as HTMLInputElement).value || undefined;
                  })}
              />
            </div>
          </div>
        `,
      )}
      <button
        class="add"
        @click=${() =>
          editSources((arr) => arr.push({ name: "", match: {}, earnRatio: 1 }))}
      >
        Add source
      </button>
    `;
  }

  render() {
    const pkg = store.pkg;
    const d = store.effective.earnedTime;
    const src = (f: string) => valueSource(pkg, "earnedTime", f);

    return html`
      <bm-section heading="Earned time">
        <p class="muted">
          Gates internet access during scheduled windows until the child has
          earned credit by active time on an approved source. Time is banked.
          Enforced by betamacsd; only applies on devices with a task bank.
        </p>
        <bm-switch
          label="Enabled"
          .value=${d.enabled}
          source=${src("enabled")}
          @field-change=${(e: CustomEvent) => setOverride("enabled", e.detail)}
          @reset=${() => clearOverride("enabled")}
        ></bm-switch>
      </bm-section>

      <bm-section heading="Schedule"> ${this.renderSchedule()} </bm-section>

      <bm-section heading="Sources"> ${this.renderSources()} </bm-section>

      <bm-section heading="Accounting">
        <bm-slider
          label="Spend ratio"
          hint="Minutes of gated internet per earned minute"
          min="0.1"
          max="4"
          step="0.1"
          unit="×"
          .value=${d.spendRatio}
          source=${src("spendRatio")}
          @field-change=${(e: CustomEvent) => setOverride("spendRatio", e.detail)}
          @reset=${() => clearOverride("spendRatio")}
        ></bm-slider>
        <bm-slider
          label="Daily earn cap"
          hint="Most that can be earned per day"
          min="0"
          max="480"
          step="5"
          unit=" min"
          .value=${d.dailyEarnCapMin}
          source=${src("dailyEarnCapMin")}
          @field-change=${(e: CustomEvent) => setOverride("dailyEarnCapMin", e.detail)}
          @reset=${() => clearOverride("dailyEarnCapMin")}
        ></bm-slider>
        <bm-slider
          label="Max bank"
          hint="Ceiling on carried-over balance"
          min="0"
          max="960"
          step="10"
          unit=" min"
          .value=${d.maxBankMin}
          source=${src("maxBankMin")}
          @field-change=${(e: CustomEvent) => setOverride("maxBankMin", e.detail)}
          @reset=${() => clearOverride("maxBankMin")}
        ></bm-slider>
        <bm-slider
          label="Min session"
          hint="Ignore earning bursts shorter than this"
          min="0"
          max="60"
          step="1"
          unit=" min"
          .value=${d.minSessionMin}
          source=${src("minSessionMin")}
          @field-change=${(e: CustomEvent) => setOverride("minSessionMin", e.detail)}
          @reset=${() => clearOverride("minSessionMin")}
        ></bm-slider>
        <bm-slider
          label="Idle timeout"
          hint="Pause crediting after this much no input"
          min="10"
          max="600"
          step="10"
          unit=" s"
          .value=${d.idleTimeoutSec}
          source=${src("idleTimeoutSec")}
          @field-change=${(e: CustomEvent) => setOverride("idleTimeoutSec", e.detail)}
          @reset=${() => clearOverride("idleTimeoutSec")}
        ></bm-slider>
      </bm-section>
    `;
  }
}
