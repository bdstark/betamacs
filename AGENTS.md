# Agent Notes

betamacs is a Rust screen-censor app for macOS with a TypeScript settings app
under `webapp/`. The pipeline, setup, and packaging are in [README.md](README.md);
the settings app's design, its store model, and how it is hosted are in
[docs/config-app.md](docs/config-app.md).

## Settings app (`webapp/`)

- Lit and Vite. The shell is `webapp/src/main.ts`; each policy area is a
  module under `webapp/src/modules/`, and the shared field controls are in
  `webapp/src/components/controls.ts`.
- The frame and theme come from `@newtonhaus/ui-kit`, shared with otactl and
  typeserver and pinned to a tag in `webapp/package.json`. `npm install` clones
  it over ssh, so an agent with a GitHub key must be loaded (`ssh-add -l`).
  Node is Homebrew's at `/opt/homebrew/bin`.
- Two builds: `npm run build` for the copy the app ships (`webapp/dist`,
  packed by `scripts/make-app.sh`) and `npm run build:store` for the copy
  typeserver serves at `/betamacs/` (`webapp/dist-store`, which is then copied
  into the typeserver repository and committed there). Rebuild and refresh both
  after any webapp change; the procedure is under **Build the static bundle**
  in `docs/config-app.md`, and the cross-repository order for a design-system
  change is in the kit's `docs/workflow.md` (github.com/bdstark/ui-kit).
- Colours are kit tokens via `webapp/src/styles/kit.css`; do not add literal
  hex values to a module.

## Things not to touch casually

- `author-pubkey.pem` and anything under `scripts/author-key.sh`: the author
  signing identity that devices verify configuration against.
- The `client/HausmeisterBetamacs` Swift package: consumed by hausmeister-mac
  as a path dependency, so a change here ships with the next Hausmeister
  release, not with betamacs.
