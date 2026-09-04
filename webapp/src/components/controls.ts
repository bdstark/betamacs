// Shared form controls. Each field row shows a "source" badge: default /
// the named config that set it / override. Overridden fields get a reset
// affordance that removes the override so the layered value shows through.

import { css, html, LitElement, nothing } from "lit";
import { customElement, property } from "lit/decorators.js";

const fieldStyles = css`
  :host {
    display: block;
  }
  .row {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 2px 12px;
    align-items: center;
    padding: 10px 0;
    border-bottom: 1px solid var(--border);
  }
  .label {
    font-weight: 500;
  }
  .hint {
    grid-column: 1 / -1;
    color: var(--muted);
    font-size: 12.5px;
  }
  .control {
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 240px;
    justify-content: flex-end;
  }
  .badge {
    font-size: 11px;
    padding: 1px 7px;
    border-radius: 8px;
    background: color-mix(in srgb, var(--accent) 15%, transparent);
    color: var(--accent);
    white-space: nowrap;
  }
  .badge.default {
    background: color-mix(in srgb, var(--muted) 15%, transparent);
    color: var(--muted);
  }
  button.reset {
    border: none;
    background: none;
    color: var(--accent);
    cursor: pointer;
    font-size: 12px;
    padding: 0;
  }
  input[type="range"] {
    width: 160px;
    accent-color: var(--accent);
  }
  input[type="number"],
  input[type="text"],
  select {
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 4px 8px;
    font: inherit;
  }
  input[type="number"] {
    width: 72px;
  }
  input[type="color"] {
    width: 36px;
    height: 26px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: none;
    padding: 1px;
  }
  .value {
    min-width: 52px;
    text-align: right;
    font-variant-numeric: tabular-nums;
    color: var(--muted);
  }
`;

abstract class Field extends LitElement {
  @property() label = "";
  @property() hint = "";
  /** "default" | "override" | a named-config name */
  @property() source = "default";

  protected badge() {
    return html`<span class="badge ${this.source === "default" ? "default" : ""}"
      >${this.source}</span
    >`;
  }

  protected reset() {
    return this.source === "override"
      ? html`<button
          class="reset"
          title="Remove override; the layered value applies"
          @click=${() => this.dispatchEvent(new CustomEvent("reset"))}
        >
          reset
        </button>`
      : nothing;
  }

  protected row(control: unknown) {
    return html`<div class="row">
      <span class="label">${this.label} ${this.badge()} ${this.reset()}</span>
      <span class="control">${control}</span>
      ${this.hint ? html`<span class="hint">${this.hint}</span>` : nothing}
    </div>`;
  }

  protected emit(value: unknown) {
    this.dispatchEvent(new CustomEvent("field-change", { detail: value }));
  }
}

@customElement("bm-slider")
export class BmSlider extends Field {
  static styles = fieldStyles;
  @property({ type: Number }) value = 0;
  @property({ type: Number }) min = 0;
  @property({ type: Number }) max = 1;
  @property({ type: Number }) step = 0.01;
  @property() unit = "";

  render() {
    const fmt =
      this.step >= 1 ? String(Math.round(this.value)) : this.value.toFixed(2);
    return this.row(html`
      <input
        type="range"
        .value=${String(this.value)}
        min=${this.min}
        max=${this.max}
        step=${this.step}
        @input=${(e: Event) => this.emit(Number((e.target as HTMLInputElement).value))}
      />
      <span class="value">${fmt}${this.unit}</span>
    `);
  }
}

@customElement("bm-switch")
export class BmSwitch extends Field {
  static styles = [
    fieldStyles,
    css`
      .toggle {
        appearance: none;
        width: 40px;
        height: 24px;
        border-radius: 12px;
        background: var(--border);
        position: relative;
        cursor: pointer;
        transition: background 0.15s;
        margin: 0;
      }
      .toggle:checked {
        background: var(--accent);
      }
      .toggle::after {
        content: "";
        position: absolute;
        top: 2px;
        left: 2px;
        width: 20px;
        height: 20px;
        border-radius: 50%;
        background: #fff;
        transition: transform 0.15s;
      }
      .toggle:checked::after {
        transform: translateX(16px);
      }
    `,
  ];
  @property({ type: Boolean }) value = false;

  render() {
    return this.row(html`
      <input
        type="checkbox"
        class="toggle"
        .checked=${this.value}
        @change=${(e: Event) => this.emit((e.target as HTMLInputElement).checked)}
      />
    `);
  }
}

@customElement("bm-color")
export class BmColor extends Field {
  static styles = fieldStyles;
  @property() value = "#000000";

  render() {
    return this.row(html`
      <input
        type="color"
        .value=${this.value}
        @input=${(e: Event) => this.emit((e.target as HTMLInputElement).value)}
      />
      <span class="value">${this.value}</span>
    `);
  }
}

@customElement("bm-select")
export class BmSelect extends Field {
  static styles = fieldStyles;
  @property() value = "";
  @property({ type: Array }) options: { value: string; label: string }[] = [];

  render() {
    return this.row(html`
      <select
        .value=${this.value}
        @change=${(e: Event) => this.emit((e.target as HTMLSelectElement).value)}
      >
        ${this.options.map(
          (o) =>
            html`<option value=${o.value} ?selected=${o.value === this.value}>
              ${o.label}
            </option>`,
        )}
      </select>
    `);
  }
}

@customElement("bm-text")
export class BmText extends Field {
  static styles = fieldStyles;
  @property() value = "";
  @property() placeholder = "";

  render() {
    return this.row(html`
      <input
        type="text"
        .value=${this.value}
        placeholder=${this.placeholder}
        @change=${(e: Event) => this.emit((e.target as HTMLInputElement).value)}
      />
    `);
  }
}

@customElement("bm-list")
export class BmList extends Field {
  static styles = [
    fieldStyles,
    css`
      .row {
        grid-template-columns: 1fr;
      }
      textarea {
        width: 100%;
        box-sizing: border-box;
        min-height: 68px;
        background: var(--bg);
        color: var(--text);
        border: 1px solid var(--border);
        border-radius: 6px;
        padding: 8px;
        font: inherit;
        font-size: 13px;
      }
    `,
  ];
  /** One entry per line; blank lines are dropped. */
  @property({ type: Array }) value: string[] = [];
  @property() placeholder = "";

  render() {
    return this.row(html`
      <textarea
        placeholder=${this.placeholder}
        .value=${this.value.join("\n")}
        @change=${(e: Event) =>
          this.emit(
            (e.target as HTMLTextAreaElement).value
              .split("\n")
              .map((s) => s.trim())
              .filter(Boolean),
          )}
      ></textarea>
    `);
  }
}

@customElement("bm-section")
export class BmSection extends LitElement {
  static styles = css`
    :host {
      display: block;
      background: var(--panel);
      border: 1px solid var(--border);
      border-radius: 12px;
      padding: 4px 18px 8px;
      margin-bottom: 18px;
    }
    h3 {
      font-size: 14px;
      text-transform: uppercase;
      letter-spacing: 0.04em;
      color: var(--muted);
      margin: 14px 0 4px;
    }
    ::slotted(*:last-child) {
      border-bottom: none;
    }
  `;
  @property() heading = "";

  render() {
    return html`<h3>${this.heading}</h3>
      <slot></slot>`;
  }
}
