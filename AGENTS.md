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
- Discord gate: Advance guild `1166119057993515100`; allowed role IDs are Officer `1167061441023582258` and Raider `1199377026168143872`.

Brick is not a normal addon manager. Its purpose is to install the current in-house guild package automatically after the first WoW path setup.

Addon updates are always enabled while Brick is running and Discord-authorized. On launch, Brick should check the signed addon feed once WoW paths are configured and Discord access is available. If every configured client already has the signed SHA installed and the managed folders exist, it should skip downloading the zip. The periodic watcher is only the follow-up check after launch and should poll every 5 minutes. The legacy `watcherEnabled` setting is ignored on read and written as true for compatibility; do not reintroduce an addon-update pause switch.

Before syncing the addon, Brick requires Discord OAuth login. It uses the public desktop OAuth flow with the HTTPS redirect URL derived from `BRICK_PRESENCE_API_URL` (`https://brick.lusaggo.com/discord/callback` in production), requests `identify guilds.members.read`, reads the current user's member roles in the Advance guild, and unlocks only if the cached or freshly refreshed session has the Raider or Officer role ID. The VPS stores only the one-time authorization code briefly until the matching Brick client polls for it; the desktop app still exchanges the code itself with PKCE and stores the user's Discord session locally. A still-current access token should unlock without a network role recheck while the local session is under 30 days old; when the token expires, refresh the token and recheck the roles. On Windows, store the cached session as DPAPI-protected `discord-auth.dat` and migrate/remove legacy plaintext `discord-auth.json`. Local session expiry, clearing, or failing refresh sends the user back to the login screen.

## Security Rules

- Do not embed GitHub tokens in the client app.
- Do not make the app run scripts, Git commands, Lua, or downloaded executables.
- Keep addon installation limited to the three managed addon folders.
- Keep signed manifest verification and zip SHA-256 verification.
- Keep app binary updates on signed release metadata. Do not run downloaded scripts or Git commands.
- Keep Discord auth as an OAuth user-token check; do not embed bot tokens or Discord client secrets in Brick.
- Existing managed addon folders are deleted and replaced after verification. Do not add backups unless Lucas asks for that again.

## Repo Split

- Brick private source: `/home/lucas/Documents/GitHub/Brick`
- ART public addon source: `/home/lucas/Documents/GitHub/AdvanceRaidTools`
- ART publishes packaged addon builds through the normal BigWigs packager flow to addon platforms.
- Brick's private `Addon feed` workflow packages ART with a pinned BigWigs packager commit in no-upload mode and signs `addon-manifest.json`.
- Fast addon feed updates come from ART's normal `Package addon` workflow after the BigWigs packager step succeeds. ART dispatches Brick's private `Addon feed` workflow with the exact ART commit SHA in `addon_ref`.
- The addon feed workflow keeps a plain five-minute cron only as a fallback. Do not rely on cron for fast guild updates; GitHub scheduled workflows can be delayed or dropped.
- The signed addon feed and Brick app releases publish assets to the public `IsogiE/Brick-Releases` repo. Do not change ART workflow/repo plumbing for Brick unless Lucas explicitly asks.
- The `Release` workflow builds public-test app packages from private Brick source and publishes them to `IsogiE/Brick-Releases`; Windows packaging is the NSIS/current-user `.exe`.
- The `Release` workflow uses GitHub Actions cache entries for Rust dependencies, the cargo target directory, and the `cargo-packager` binary to reduce future cold-start packaging time.
- Public Brick app releases should have an empty release body; keep installer guidance out of the GitHub release text.
- Windows release signing is opt-in with repo variable `BRICK_WINDOWS_SIGNING=artifact-signing` and Azure Artifact Signing secrets. When enabled, the Release workflow signs `target/release/brick.exe` before Windows packaging and signs the final Windows installers before upload.
- Public Brick app releases require repo variable `BRICK_DISCORD_CLIENT_ID` for the Discord login app. The Advance guild and allowed role IDs are built into the client, with build-time env overrides available if they ever change.
- Public Brick app releases require repo variable `BRICK_PRESENCE_API_URL` for the roster/online-status service. The current production endpoint is `https://brick.lusaggo.com`.
- Brick's roster tab talks to the Brick Presence API after Discord login. Clients send short-lived heartbeats with the user's Discord OAuth access token; the VPS verifies the token and stores online state. Full guild roster reads happen server-side with the Discord bot token. Never put a Discord bot token or client secret in the desktop app.
- Brick auto-updates Advance Raid Tools from the signed addon feed. Brick app self-updates use the signed `app-feed` manifest. Clients check the signed app feed on startup and periodically, then show a compact `Update now` prompt when a newer supported package exists. Only the user click downloads the versioned package, verifies its SHA-256, and starts install/restart. Windows uses the NSIS current-user `.exe` from `IsogiE/Brick-Releases`; this is intended for installs under `%LOCALAPPDATA%` so it does not need UAC. Silent NSIS updates force `%LOCALAPPDATA%\Brick` so future updates stay in the current-user install location. Linux AppImage installs download a versioned AppImage, verify its SHA-256, atomically replace the current AppImage, restart Brick, and exit the old process. Deb/pacman style installs may require elevation and are not the preferred self-update path. The addon feed and app feed currently use the same Ed25519 signing key. Brick clients older than the self-update baseline still need one bridge installer.
- Brick starts at login with `--startup` when "Open at login" is enabled (the default). This preference is independent of automatic addon updates and is preserved when adding WoW folders. The user-facing "Start minimized" setting defaults on; when disabled, `--startup` launches the full window instead of hiding it.
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

If "Open at login" has been enabled, Brick may also create:

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
- Repo variable `BRICK_DISCORD_CLIENT_ID`

Public ART repo:

- `BRICK_WORKFLOW_TOKEN`: fine-grained GitHub token that can dispatch the private Brick `Addon feed` workflow.

Do not use a broad personal token for `BRICK_WORKFLOW_TOKEN` unless Lucas explicitly approves the wider blast radius. Prefer a fine-grained token scoped only to `IsogiE/Brick` with Actions write access.

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
node --check scripts/publish-app-feed.mjs
node --check scripts/local-appimage-install.mjs
node --check scripts/local-appimage-remove.mjs
node --check scripts/set-app-version.mjs
npm run feed:latest
```
