// Black-box censor module editor, with a live preview of the box style.

import { css, html, LitElement } from "lit";
import { customElement } from "lit/decorators.js";
import {
  valueSource,
  type BlurSettings,
  type CensorPatch,
  type CensorSettings,
  type MosaicSettings,
  type StaticSettings,
  type TextOverlay,
} from "../schema.js";
import { store } from "../store.js";
import "../components/controls.js";

function setOverride<K extends keyof CensorPatch>(key: K, value: CensorPatch[K]): void {
  store.update((pkg) => {
    pkg.overrides.censor = { ...pkg.overrides.censor, [key]: value };
  });
}

function clearOverride(key: keyof CensorPatch): void {
  store.update((pkg) => {
    if (pkg.overrides.censor) delete pkg.overrides.censor[key];
  });
}

@customElement("bm-censor-module")
export class BmCensorModule extends LitElement {
  static styles = css`
    :host {
      display: block;
    }
    .preview {
      display: grid;
      place-items: center;
      padding: 20px 0 26px;
    }
    .preview .box {
      width: 220px;
      height: 130px;
      display: grid;
      place-items: center;
      border-radius: 2px;
      text-align: center;
      overflow: hidden;
    }
    .preview .label {
      font-size: 11px;
      opacity: 0.85;
      letter-spacing: 0.05em;
    }
    textarea {
      width: 100%;
      min-height: 90px;
      background: var(--bg);
      color: var(--text);
      border: 1px solid var(--border);
      border-radius: 6px;
      padding: 8px;
      font: inherit;
      box-sizing: border-box;
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

  private setText(patch: Partial<TextOverlay>) {
    // Strip resolved lines; they're derived from the sets on resolve.
    const { lines, ...current } = store.effective.censor.textOverlay;
    setOverride("textOverlay", { ...current, ...patch });
  }

  private setBlur(patch: Partial<BlurSettings>) {
    setOverride("blur", { ...store.effective.censor.blur, ...patch });
  }

  private setMosaic(patch: Partial<MosaicSettings>) {
    setOverride("mosaic", { ...store.effective.censor.mosaic, ...patch });
  }

  private setStatic(patch: Partial<StaticSettings>) {
    setOverride("staticNoise", { ...store.effective.censor.staticNoise, ...patch });
  }

  /** CSS approximation of the interior for the preview box. */
  private previewInterior(c: CensorSettings): string {
    switch (c.mode) {
      case "blur":
        return `background:linear-gradient(115deg,#c98,#967 40%,#789 60%,#546);filter:blur(${Math.min(c.blur.intensity / 3, 12)}px)`;
      case "mosaic": {
        const px = Math.max(c.mosaic.cellSizePt / 2, 3);
        const [a, b] =
          c.mosaic.map === "none" ? ["#c98", "#546"] : [c.mosaic.colorLow, c.mosaic.colorHigh];
        return `background:repeating-conic-gradient(${a} 0% 25%, ${b} 0% 50%) 0 0/${px * 2}px ${px * 2}px`;
      }
      case "static": {
        const [a, b] = c.staticNoise.colored
          ? [c.staticNoise.colorLow, c.staticNoise.colorHigh]
          : ["#111", "#eee"];
        const g = Math.max(c.staticNoise.grainMm * 3, 2);
        return `background:repeating-conic-gradient(${a} 0% 25%, ${b} 0% 50%) 0 0/${g * 2}px ${g * 2}px`;
      }
      default:
        return `background:${c.fillColor}`;
    }
  }

  render() {
    const pkg = store.pkg;
    const c = store.effective.censor;
    const src = (f: string) => valueSource(pkg, "censor", f);
    const t = c.textOverlay;

    return html`
      <bm-section heading="Preview">
        <div class="preview">
          <div
            class="box"
            style="${this.previewInterior(c)};opacity:${c.opacityPct /
            100};border:${c.borderWidth}px solid ${c.borderColor};transform:scale(${c.xScalePct /
            130}, ${c.yScalePct / 130})"
          >
            <div>
              ${t.enabled && t.lines?.length
                ? html`<div
                    style="color:${t.fontColor};font-family:${t.fontFamily};font-size:${t.fontSizePt}px"
                  >
                    ${t.lines[0]}
                  </div>`
                : ""}
              ${c.showTriggerLabel
                ? html`<div class="label" style="color:${t.fontColor}">
                    TRIGGER_CLASS
                  </div>`
                : ""}
            </div>
          </div>
        </div>
      </bm-section>

      <bm-section heading="Censor">
        <bm-select
          label="Mode"
          hint="How box interiors render; options below adapt to the mode"
          .options=${[
            { value: "box", label: "Solid box" },
            { value: "blur", label: "Blur" },
            { value: "mosaic", label: "Mosaic" },
            { value: "static", label: "TV static" },
          ]}
          .value=${c.mode}
          source=${src("mode")}
          @field-change=${(e: CustomEvent) => setOverride("mode", e.detail)}
          @reset=${() => clearOverride("mode")}
        ></bm-select>
        <bm-slider
          label="Opacity"
          hint="Below 100% the content underneath shows through"
          min="10"
          max="100"
          step="5"
          unit="%"
          .value=${c.opacityPct}
          source=${src("opacityPct")}
          @field-change=${(e: CustomEvent) => setOverride("opacityPct", e.detail)}
          @reset=${() => clearOverride("opacityPct")}
        ></bm-slider>
      </bm-section>

      ${c.mode === "box"
        ? html`<bm-section heading="Solid box">
            <bm-color
              label="Fill color"
              .value=${c.fillColor}
              source=${src("fillColor")}
              @field-change=${(e: CustomEvent) => setOverride("fillColor", e.detail)}
              @reset=${() => clearOverride("fillColor")}
            ></bm-color>
          </bm-section>`
        : ""}
      ${c.mode === "blur"
        ? html`<bm-section heading="Blur">
            <bm-select
              label="Blur type"
              .options=${[
                { value: "gaussian", label: "Gaussian" },
                { value: "box", label: "Box" },
                { value: "average", label: "Average (downscale)" },
              ]}
              .value=${c.blur.kind}
              source=${src("blur")}
              @field-change=${(e: CustomEvent) => this.setBlur({ kind: e.detail })}
              @reset=${() => clearOverride("blur")}
            ></bm-select>
            <bm-slider
              label="Intensity"
              min="1"
              max="100"
              step="1"
              .value=${c.blur.intensity}
              source=${src("blur")}
              @field-change=${(e: CustomEvent) => this.setBlur({ intensity: e.detail })}
              @reset=${() => clearOverride("blur")}
            ></bm-slider>
          </bm-section>`
        : ""}
      ${c.mode === "mosaic"
        ? html`<bm-section heading="Mosaic">
            <bm-slider
              label="Pixel size"
              min="4"
              max="64"
              step="2"
              unit=" pt"
              .value=${c.mosaic.cellSizePt}
              source=${src("mosaic")}
              @field-change=${(e: CustomEvent) => this.setMosaic({ cellSizePt: e.detail })}
              @reset=${() => clearOverride("mosaic")}
            ></bm-slider>
            <bm-select
              label="Pixel sampling"
              hint="How each pixel's value is generated from the source"
              .options=${[
                { value: "average", label: "Average" },
                { value: "gaussian", label: "Gaussian" },
                { value: "nearest", label: "Point sample" },
              ]}
              .value=${c.mosaic.sampling}
              source=${src("mosaic")}
              @field-change=${(e: CustomEvent) => this.setMosaic({ sampling: e.detail })}
              @reset=${() => clearOverride("mosaic")}
            ></bm-select>
            <bm-select
              label="Color mapping"
              hint="How source color/luminance maps into the color range"
              .options=${[
                { value: "none", label: "True colors" },
                { value: "luminance", label: "Luminance → range" },
                { value: "steps", label: "Luminance → 4 bands" },
              ]}
              .value=${c.mosaic.map}
              source=${src("mosaic")}
              @field-change=${(e: CustomEvent) => this.setMosaic({ map: e.detail })}
              @reset=${() => clearOverride("mosaic")}
            ></bm-select>
            ${c.mosaic.map !== "none"
              ? html`<bm-color
                    label="Range low"
                    .value=${c.mosaic.colorLow}
                    source=${src("mosaic")}
                    @field-change=${(e: CustomEvent) => this.setMosaic({ colorLow: e.detail })}
                    @reset=${() => clearOverride("mosaic")}
                  ></bm-color>
                  <bm-color
                    label="Range high"
                    .value=${c.mosaic.colorHigh}
                    source=${src("mosaic")}
                    @field-change=${(e: CustomEvent) => this.setMosaic({ colorHigh: e.detail })}
                    @reset=${() => clearOverride("mosaic")}
                  ></bm-color>`
              : ""}
          </bm-section>`
        : ""}
      ${c.mode === "static"
        ? html`<bm-section heading="TV static">
            <bm-slider
              label="Density"
              hint="Fraction of grains lit"
              min="5"
              max="100"
              step="5"
              unit="%"
              .value=${c.staticNoise.densityPct}
              source=${src("staticNoise")}
              @field-change=${(e: CustomEvent) => this.setStatic({ densityPct: e.detail })}
              @reset=${() => clearOverride("staticNoise")}
            ></bm-slider>
            <bm-slider
              label="Speed"
              hint="Frame changes per second; 0 freezes the pattern"
              min="0"
              max="30"
              step="1"
              unit=" Hz"
              .value=${c.staticNoise.speedHz}
              source=${src("staticNoise")}
              @field-change=${(e: CustomEvent) => this.setStatic({ speedHz: e.detail })}
              @reset=${() => clearOverride("staticNoise")}
            ></bm-slider>
            <bm-slider
              label="Grain size"
              min="0.2"
              max="5"
              step="0.2"
              unit=" mm"
              .value=${c.staticNoise.grainMm}
              source=${src("staticNoise")}
              @field-change=${(e: CustomEvent) => this.setStatic({ grainMm: e.detail })}
              @reset=${() => clearOverride("staticNoise")}
            ></bm-slider>
            <bm-switch
              label="Colored static"
              hint="Off = classic black & white"
              .value=${c.staticNoise.colored}
              source=${src("staticNoise")}
              @field-change=${(e: CustomEvent) => this.setStatic({ colored: e.detail })}
              @reset=${() => clearOverride("staticNoise")}
            ></bm-switch>
            ${c.staticNoise.colored
              ? html`<bm-color
                    label="Range low"
                    .value=${c.staticNoise.colorLow}
                    source=${src("staticNoise")}
                    @field-change=${(e: CustomEvent) => this.setStatic({ colorLow: e.detail })}
                    @reset=${() => clearOverride("staticNoise")}
                  ></bm-color>
                  <bm-color
                    label="Range high"
                    .value=${c.staticNoise.colorHigh}
                    source=${src("staticNoise")}
                    @field-change=${(e: CustomEvent) => this.setStatic({ colorHigh: e.detail })}
                    @reset=${() => clearOverride("staticNoise")}
                  ></bm-color>`
              : ""}
          </bm-section>`
        : ""}

      <bm-section heading="Border & size">
        <bm-color
          label="Border color"
          .value=${c.borderColor}
          source=${src("borderColor")}
          @field-change=${(e: CustomEvent) => setOverride("borderColor", e.detail)}
          @reset=${() => clearOverride("borderColor")}
        ></bm-color>
        <bm-slider
          label="Border width"
          min="0"
          max="12"
          step="1"
          unit=" pt"
          .value=${c.borderWidth}
          source=${src("borderWidth")}
          @field-change=${(e: CustomEvent) => setOverride("borderWidth", e.detail)}
          @reset=${() => clearOverride("borderWidth")}
        ></bm-slider>
        <bm-slider
          label="Width scale"
          hint="Box width as a percentage of the detected region"
          min="50"
          max="300"
          step="5"
          unit="%"
          .value=${c.xScalePct}
          source=${src("xScalePct")}
          @field-change=${(e: CustomEvent) => setOverride("xScalePct", e.detail)}
          @reset=${() => clearOverride("xScalePct")}
        ></bm-slider>
        <bm-slider
          label="Height scale"
          min="50"
          max="300"
          step="5"
          unit="%"
          .value=${c.yScalePct}
          source=${src("yScalePct")}
          @field-change=${(e: CustomEvent) => setOverride("yScalePct", e.detail)}
          @reset=${() => clearOverride("yScalePct")}
        ></bm-slider>
      </bm-section>

      <bm-section heading="Behavior">
        <bm-switch
          label="Censor screenshots and shares"
          hint="Boxes appear in captures made by other apps too"
          .value=${c.censorInCaptures}
          source=${src("censorInCaptures")}
          @field-change=${(e: CustomEvent) => setOverride("censorInCaptures", e.detail)}
          @reset=${() => clearOverride("censorInCaptures")}
        ></bm-switch>
        <bm-switch
          label="Show trigger label"
          hint="Overlays the NudeNet class that caused the box"
          .value=${c.showTriggerLabel}
          source=${src("showTriggerLabel")}
          @field-change=${(e: CustomEvent) => setOverride("showTriggerLabel", e.detail)}
          @reset=${() => clearOverride("showTriggerLabel")}
        ></bm-switch>
      </bm-section>

      <bm-section heading="Text overlay">
        <bm-switch
          label="Enabled"
          hint="Draw a randomly chosen text on each box"
          .value=${t.enabled}
          source=${src("textOverlay")}
          @field-change=${(e: CustomEvent) => this.setText({ enabled: e.detail })}
          @reset=${() => clearOverride("textOverlay")}
        ></bm-switch>
        <bm-text
          label="Font"
          .value=${t.fontFamily}
          source=${src("textOverlay")}
          @field-change=${(e: CustomEvent) => this.setText({ fontFamily: e.detail })}
          @reset=${() => clearOverride("textOverlay")}
        ></bm-text>
        <bm-slider
          label="Font size"
          min="8"
          max="72"
          step="1"
          unit=" pt"
          .value=${t.fontSizePt}
          source=${src("textOverlay")}
          @field-change=${(e: CustomEvent) => this.setText({ fontSizePt: e.detail })}
          @reset=${() => clearOverride("textOverlay")}
        ></bm-slider>
        <bm-color
          label="Font color"
          .value=${t.fontColor}
          source=${src("textOverlay")}
          @field-change=${(e: CustomEvent) => this.setText({ fontColor: e.detail })}
          @reset=${() => clearOverride("textOverlay")}
        ></bm-color>
        <div style="padding:10px 0">
          <p class="muted" style="margin:0 0 6px">
            Text sets assigned to this display — lines pool together, one is
            picked per box. Manage the sets in Layers &amp; package.
          </p>
          ${pkg.textSets.length === 0
            ? html`<p class="muted">No text sets defined yet.</p>`
            : pkg.textSets.map(
                (set) => html`
                  <label style="display:flex;gap:8px;align-items:center;padding:3px 0">
                    <input
                      type="checkbox"
                      .checked=${t.sets.includes(set.name)}
                      @change=${(e: Event) => {
                        const on = (e.target as HTMLInputElement).checked;
                        const sets = t.sets.filter((s) => s !== set.name);
                        if (on) sets.push(set.name);
                        this.setText({ sets });
                      }}
                    />
                    <span>${set.name}</span>
                    <span class="muted">${set.lines.length} line(s)</span>
                  </label>
                `,
              )}
          <p class="muted" style="margin:6px 0 0">
            Pooled lines: ${(t.lines ?? []).length ? (t.lines ?? []).join(" · ") : "none"}
          </p>
        </div>
      </bm-section>
    `;
  }
}
