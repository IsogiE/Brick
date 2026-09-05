# Brick

Small in-house updater for Advance Raid Tools.

Brick is intentionally not a general addon manager. After first setup it runs at login, checks the single signed guild feed on GitHub, and installs the latest published package into every configured WoW client folder automatically.

Brick is a native Rust desktop app. It does not use Electron, Tauri, WebView2, WebKitGTK, or an embedded browser/webpreview for its UI.

## Targets

- Windows: NSIS current-user `.exe` installer.
- Linux: AppImage for Arch/CachyOS and other desktop distros, plus deb/pacman artifacts for Ubuntu/Debian/Arch-style installs.

## First Setup

1. Run Brick.
2. Sign in with Discord. Brick allows members with the Advance Raider or Officer role.
3. Select the World of Warcraft folder, or a specific client folder such as `_retail_`, `_ptr_`, or `_xptr_`.
4. Brick immediately syncs the current package and opens at login by default.

Addon updates are always enabled while Brick is running and signed in. Brick checks on launch and every minute afterward. The "Open at login" setting only controls whether Brick starts with your computer; turning it off does not pause addon updates. "Start minimized" controls whether that login launch opens the window or stays in the tray.

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

Add this GitHub repo variable to the private Brick repo:

- `BRICK_DISCORD_CLIENT_ID`: Discord application/client ID for the Brick login app.
- `BRICK_PRESENCE_API_URL`: HTTPS URL for the Brick Presence API, currently `https://brick.lusaggo.com`.

Configure that Discord application as a public OAuth2 client and add this redirect URL:

```text
https://brick.lusaggo.com/discord/callback
```

Brick uses Discord OAuth scopes `identify` and `guilds.members.read` to read only the signed-in user's guild member object. Browser login redirects to the Brick Presence API, which holds the one-time authorization code briefly until the matching Brick client polls for it. The desktop app still exchanges the code itself with PKCE and stores the user's Discord session locally. The Advance guild and allowed role IDs are built in:

- Guild: `1166119057993515100`
- Officer: `1167061441023582258`
- Raider: `1199377026168143872`

Optional Windows signing setup:

- Set repo variable `BRICK_WINDOWS_SIGNING` to `artifact-signing`.
- Add secrets `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, and `AZURE_SUBSCRIPTION_ID` for the GitHub OIDC app registration.
- Add secrets `AZURE_ARTIFACT_SIGNING_ENDPOINT`, `AZURE_ARTIFACT_SIGNING_ACCOUNT_NAME`, and `AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE_NAME`.
- Assign that Azure identity the Artifact Signing Certificate Profile Signer role.

When enabled, the release workflow signs `brick.exe` before Windows packaging and signs the final Windows installers before publishing.

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

- Windows NSIS current-user installer
- Linux AppImage
- Linux deb
- Linux pacman package

The published app packages are separate from the addon feed. After publishing the versioned app packages, the release workflow updates the fixed `app-feed` release with:

- `app-manifest.json`
- `app-manifest.json.sig`

Installed Brick clients with app self-update support check the signed app feed on startup and periodically while running. When a newer supported package exists, Brick shows a compact `Update now` prompt in the header. Clicking it downloads the newest package, verifies the manifest signature and package SHA-256, then starts install/restart. On Windows, Brick uses the NSIS current-user `.exe`; this does not require UAC when Brick is installed under the user's profile. Silent NSIS updates force `%LOCALAPPDATA%\Brick` so future updates stay in the current-user install location. On Linux AppImage installs, Brick atomically replaces the current AppImage, restarts Brick, and exits the old process. Deb and pacman installs are system-package style artifacts and may require elevation, so they are not the preferred self-update path. Brick clients older than the self-update baseline need one more installer install to receive this capability.

App-update checks clean completed installer downloads from the temporary `Brick/updates` folder. Only installers for the running Brick version or older are removed; newer pending downloads are preserved. Locked files are retried on a later check. Successful addon installs also remove leftover `.brick-staging` transaction directories from interrupted runs.

Brick enables login startup by default after setup. Login launches pass `--startup`; Brick starts hidden/minimized by default and exposes a Settings toggle to let users open the full window at login instead.

Brick keeps Discord login cached for up to 30 days. If the cached access token is still current and the local session is still inside that window, Brick opens without another Discord prompt. When the access token expires, Brick uses the saved refresh token to get a fresh token, rechecks the user's Advance roles, and only asks for browser login again if Discord rejects the saved session, the local 30-day session window has elapsed, or the user no longer has an allowed role. On Windows, the cached Discord session is stored as a DPAPI-protected `discord-auth.dat` file instead of plaintext JSON; old `discord-auth.json` files are migrated and removed on the next successful read.

## Presence API

Brick's roster tab uses a small HTTPS service in `presence/`.

- Clients send a heartbeat after Discord login and periodically while open.
- The server verifies each heartbeat with Discord using the user's OAuth access token.
- The server uses the Discord bot token to list guild members and groups them as Officer first, then Raider.
- The desktop app never contains the Discord bot token.

For `lusaggo.com`, DNS should point the Brick API subdomain at the VPS:

```text
Type: A
Name: brick
Content: 2.28.118.132
Proxy: DNS only
TTL: Auto
```

Deploy notes are in `presence/README.md`.

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

Because the workflow packages the current Git commit, untagged commits become alpha-style builds such as `v1.7.10-17-g2f772b5`, while tagged commits become release builds. The workflow ignores non-`v*` tags in its local checkout so release-only feed tags cannot affect addon package versions. The BigWigs packager is pinned to commit `20a3713ec537df54db5c0d8b4822d88ee63c70e8`, and the workflow verifies the `release.sh` SHA-256 before running it. Installed Brick clients poll the signed feed every minute.

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

The app must be built with `BRICK_ADDON_PUBLIC_KEY_B64` set before it can trust the public addon feed. Local builds without that key still open the UI, but sync will refuse to install packages. Local builds without `BRICK_DISCORD_CLIENT_ID` show the Discord login configuration screen.

Windows desktop smoke testing and private candidate builds: [instructions](docs/windows-smoke.md).
