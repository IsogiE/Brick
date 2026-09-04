# Brick

Small in-house updater for Advance Raid Tools.

Brick is intentionally not a general addon manager. After first setup it runs at login, checks the single signed guild feed on GitHub, and installs the latest published package into every configured WoW client folder automatically.

Brick is a native Rust desktop app. It does not use Electron, Tauri, WebView2, WebKitGTK, or an embedded browser/webpreview for its UI.

## Targets

- Windows: MSI installer.
- Linux: AppImage for Arch/CachyOS and other desktop distros, plus deb/pacman artifacts for Ubuntu/Debian/Arch-style installs.

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
- `BRICK_ADDON_PUBLIC_KEY_B64`: raw 32-byte Ed25519 public key embedded into Brick for addon and app update feeds.
- `BRICK_ADDON_PRIVATE_KEY_B64`: PKCS#8 Ed25519 private key used by Brick workflows to sign `addon-manifest.json` and `app-manifest.json`.

Optional Windows signing setup:

- Set repo variable `BRICK_WINDOWS_SIGNING` to `artifact-signing`.
- Add secrets `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, and `AZURE_SUBSCRIPTION_ID` for the GitHub OIDC app registration.
- Add secrets `AZURE_ARTIFACT_SIGNING_ENDPOINT`, `AZURE_ARTIFACT_SIGNING_ACCOUNT_NAME`, and `AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE_NAME`.
- Assign that Azure identity the Artifact Signing Certificate Profile Signer role.

When enabled, the release workflow signs `brick.exe` before MSI packaging and signs the final MSI before publishing.

The public AdvanceRaidTools addon repo does not need Brick feed scripts or Brick signing secrets. For near-immediate feed publishing, it does need one dispatch-only secret:

- `BRICK_WORKFLOW_TOKEN`: fine-grained GitHub token scoped to `IsogiE/Brick` with Actions write access. ART uses it after the normal BigWigs packager step succeeds to dispatch Brick's private `Addon feed` workflow for that exact ART commit.

Generate the addon feed key with:

```sh
npm run keys:addon
```

Create app releases by pushing SemVer tags:

```sh
git tag v0.1.0
git push origin v0.1.0
```

You can also run the `Release` workflow manually and provide a SemVer version. In both cases, the private Brick repo builds the app and publishes only the installer/package assets to the public `IsogiE/Brick-Releases` repo.

The release workflow publishes to `IsogiE/Brick-Releases` and builds:

- Windows MSI installer
- Linux AppImage
- Linux deb
- Linux pacman package

The published app packages are separate from the addon feed. After publishing the versioned app packages, the release workflow updates the fixed `app-feed` release with:

- `app-manifest.json`
- `app-manifest.json.sig`

Installed Brick clients with app self-update support check the signed app feed on startup and periodically while running. On Windows, Brick downloads the newest MSI, verifies the manifest signature and MSI SHA-256, starts the installer in passive mode, and exits so Windows Installer can finish the update. On Linux AppImage installs, Brick downloads the newest AppImage, verifies the manifest signature and AppImage SHA-256, atomically replaces the current AppImage, restarts Brick, and exits the old process. Brick clients older than the self-update baseline need one more installer install to receive this capability.

## Addon Feed

The `Addon feed` workflow in this repo checks out public `IsogiE/AdvanceRaidTools` source, runs the pinned BigWigs packager in no-upload mode, signs the package metadata, and publishes the current package to:

```text
https://github.com/IsogiE/Brick-Releases/releases/download/addon-feed/
```

It publishes:

- `AdvanceRaidTools-current.zip`
- `addon-manifest.json`
- `addon-manifest.json.sig`

ART's normal `Package addon` workflow dispatches this workflow with the exact ART commit SHA after Wago/Curse packaging succeeds, so the public Brick feed can update within seconds without shipping a new Brick client. A plain five-minute schedule remains as a fallback for missed dispatches.

Because the workflow packages the current Git commit, untagged commits become alpha-style builds such as `v1.7.10-17-g2f772b5`, while tagged commits become release builds. The workflow ignores non-`v*` tags in its local checkout so release-only feed tags cannot affect addon package versions. The BigWigs packager is pinned to commit `20a3713ec537df54db5c0d8b4822d88ee63c70e8`, and the workflow verifies the `release.sh` SHA-256 before running it. Installed Brick clients poll the signed feed every 30 seconds.

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
