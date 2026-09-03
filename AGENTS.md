# Brick Agent Notes

Read this before changing Brick. This repo is the private source app. The public addon repo is `/home/lucas/Documents/GitHub/AdvanceRaidTools`.

## Current Shape

- Product name: `Brick`
- Binary/package name: `brick`
- App identifier: `dev.isogi.brick`
- UI stack: native Rust with `eframe`/`egui`; do not reintroduce a webview UI.
- User-facing UI should stay single-screen and consumer-simple: no Logs tab, no watcher/feed/debug wording, no manual update-manager framing.
- Default window size is intentionally comfortable at `980x720`, with `720x560` as the minimum.
- Public app release repo: `IsogiE/Brick-Releases`
- Public addon feed release: `IsogiE/Brick-Releases` tag `addon-feed`
- Managed addon folders: `AdvanceRaidTools`, `AdvanceRaidTools_Libraries`, `AdvanceRaidTools_Options`
- Local staging folder inside each WoW AddOns folder: `.brick-staging`

Brick is not a normal addon manager. Its purpose is to install the current in-house guild package automatically after the first WoW path setup.

On launch, Brick should check the signed addon feed when automation is enabled. If every configured client already has the signed SHA installed and the managed folders exist, it should skip downloading the zip. The periodic watcher is only the follow-up check after launch.

## Security Rules

- Do not embed GitHub tokens in the client app.
- Do not make the app run scripts, Git commands, Lua, or downloaded executables.
- Keep addon installation limited to the three managed addon folders.
- Keep signed manifest verification and zip SHA-256 verification.
- Keep app binary updates on signed release metadata. Do not run downloaded scripts or Git commands.
- Existing managed addon folders are deleted and replaced after verification. Do not add backups unless Lucas asks for that again.

## Repo Split

- Brick private source: `/home/lucas/Documents/GitHub/Brick`
- ART public addon source: `/home/lucas/Documents/GitHub/AdvanceRaidTools`
- ART publishes packaged addon builds through the normal BigWigs packager flow to addon platforms.
- Brick's private `Addon feed` workflow checks out the latest public ART source, packages it with a pinned BigWigs packager commit in no-upload mode, and signs `addon-manifest.json`.
- The signed addon feed and Brick app releases publish assets to the public `IsogiE/Brick-Releases` repo. Do not change ART workflow/repo plumbing for Brick unless Lucas explicitly asks.
- The `Release` workflow builds public-test app packages from private Brick source and publishes them to `IsogiE/Brick-Releases`; Windows packaging uses WiX/MSI.
- The `Release` workflow uses GitHub Actions cache entries for Rust dependencies, the cargo target directory, and the `cargo-packager` binary to reduce future cold-start packaging time.
- Brick currently auto-updates Advance Raid Tools, not the Brick app binary itself. Add signed app-update metadata before claiming installed Brick clients self-update.
- The feed workflow prunes non-`v*` tags from its local ART checkout before packaging. This prevents feed/release-only tags from changing BigWigs package versions.
- Current BigWigs packager pin: `20a3713ec537df54db5c0d8b4822d88ee63c70e8`; `release.sh` SHA-256: `49bcf94d977478a6f18ead7f0285f193853bf1f9a23ba7d84d08b979d802a734`.

## Local Linux Testing

From this repo:

```sh
cargo check
cargo install cargo-packager --locked
npm run build:local-appimage
npm run install:local-appimage
```

Then search the desktop/application launcher for `Brick`.

The local AppImage installer copies the newest `*.AppImage` with `brick` in the filename to a temporary file, then atomically renames it to:

```text
~/.local/bin/Brick.AppImage
```

It also writes:

```text
~/.local/share/applications/dev.isogi.brick.desktop
~/.local/share/icons/hicolor/256x256/apps/dev.isogi.brick.png
```

If startup automation has been enabled, Brick may also create:

```text
~/.config/autostart/Brick.desktop
```

Remove the local launcher install:

```sh
npm run remove:local-appimage
```

Remove launcher install plus local Brick app data:

```sh
npm run remove:local-appimage -- --purge
```

Use `NO_STRIP=1` for AppImage builds on Arch/CachyOS. `npm run build:local-appimage` already does this. Release CI builds AppImages on Ubuntu 22.04 to keep the Linux baseline friendly for Ubuntu and other common distros.

## Release Secrets

Private Brick repo:

- `BRICK_RELEASE_TOKEN`
- `BRICK_ADDON_PUBLIC_KEY_B64`
- `BRICK_ADDON_PRIVATE_KEY_B64`

Public ART repo:

- No Brick updater secrets are required.
- Brick uses ART as a public source checkout only.

Generate the addon feed signing key with:

```sh
npm run keys:addon
```

## Useful Checks

```sh
cargo fmt --check
cargo check
node --check scripts/generate-addon-signing-key.mjs
node --check scripts/publish-addon-feed.mjs
node --check scripts/local-appimage-install.mjs
node --check scripts/local-appimage-remove.mjs
node --check scripts/set-app-version.mjs
npm run feed:latest
```
