# Brick

Small in-house updater for Advance.

Brick is intentionally not a general addon manager. After first setup it runs at login, checks the single signed guild feed on GitHub, and installs the latest published package into every configured WoW client folder without asking users to click an update button.

Brick is a native Rust desktop app. It does not use Electron, Tauri, WebView2, WebKitGTK, or an embedded browser/webpreview for its UI.

## Targets

- Windows: NSIS setup exe, installed per-user so it does not require admin rights by default.
- Linux: AppImage for Arch/CachyOS and other desktop distros, plus deb/rpm/pacman artifacts for Ubuntu/Debian/Fedora/Arch-style installs.

## First Setup

1. Run Brick.
2. Select the World of Warcraft folder, or a specific client folder such as `_retail_`, `_ptr_`, or `_xptr_`.
3. Brick enables startup automation and immediately syncs the current package.

Supported client folders:

- `_retail_`
- `_ptr_`
- `_xptr_`
- `_beta_`
- `_classic_`
- `_classic_era_`
- `_classic_ptr_`
- `_classic_beta_`

## Release Setup

The Brick source repo can stay private, but updater binaries and the addon feed need to be reachable without GitHub auth. The addon feed and app release artifacts are published to the public release-only repo `IsogiE/Brick-Releases`.

Push this Brick folder to its private GitHub repo, then add these GitHub secrets to that private Brick repo:

- `BRICK_RELEASE_TOKEN`: fine-grained GitHub token with Contents read/write for `IsogiE/Brick-Releases`.
- `BRICK_ADDON_PUBLIC_KEY_B64`: raw 32-byte Ed25519 public key embedded into Brick.
- `BRICK_ADDON_PRIVATE_KEY_B64`: PKCS#8 Ed25519 private key used by the Brick feed workflow to sign `addon-manifest.json`.

The public AdvanceRaidTools addon repo does not need Brick feed scripts or Brick signing secrets.

Generate the addon feed key with:

```sh
npm run keys:addon
```

Create app releases by pushing SemVer tags:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The release workflow publishes to `IsogiE/Brick-Releases` and builds:

- Windows NSIS installer
- Linux AppImage
- Linux deb
- Linux rpm
- Linux pacman package

## Addon Feed

The `Addon feed` workflow in this repo checks out the latest public `IsogiE/AdvanceRaidTools` source, runs the pinned BigWigs packager in no-upload mode, signs the package metadata, and publishes the current package to:

```text
https://github.com/IsogiE/Brick-Releases/releases/download/addon-feed/
```

It publishes:

- `AdvanceRaidTools-current.zip`
- `addon-manifest.json`
- `addon-manifest.json.sig`

Because the workflow packages the current Git commit, untagged commits become alpha-style builds such as `v1.7.10-17-g2f772b5`, while tagged commits become release builds. The BigWigs packager is pinned to commit `20a3713ec537df54db5c0d8b4822d88ee63c70e8`, and the workflow verifies the `release.sh` SHA-256 before running it.

Brick clients download from that public release, verify the Ed25519 manifest signature, verify the zip SHA-256 from the signed manifest, delete the managed addon folders, and replace them with the verified package. If the feed has not been published yet, clients show a waiting state and retry automatically.

To locally see which ART source version Brick would publish:

```sh
npm run feed:latest
```

## Local Development

```sh
npm run dev
```

For local Linux package testing:

```sh
cargo install cargo-packager --locked
npm run build:local-appimage
npm run install:local-appimage
```

Then search your application launcher for `Brick`.

To remove the local AppImage launcher install:

```sh
npm run remove:local-appimage
```

To remove the local launcher install plus Brick config/cache:

```sh
npm run remove:local-appimage -- --purge
```

The app must be built with `BRICK_ADDON_PUBLIC_KEY_B64` set before it can trust the public addon feed. Local builds without that key still open the UI, but sync will refuse to install packages.
