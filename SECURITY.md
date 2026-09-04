# Security Notes

Brick has one high-risk job: it writes files onto guild members' computers automatically. The app is built around keeping that power narrow.

- The app does not run Git, shell scripts, Lua, or arbitrary downloaded executables. The only downloaded executable allowed is a Brick app installer from signed app-update metadata.
- Discord login is a client-side guild-role gate for the normal Brick UX. It is not a replacement for server-side access control if release downloads need to become private.
- The addon feed manifest must be signed with the embedded Ed25519 public key.
- The addon zip must match the SHA-256 hash from the signed manifest.
- Downloads must come from the `IsogiE/Brick-Releases` `addon-feed` GitHub release URL namespace.
- Zip entries are rejected if they escape the allowed addon folders.
- Only these folders are managed: `AdvanceRaidTools`, `AdvanceRaidTools_Libraries`, and `AdvanceRaidTools_Options`.
- Existing managed folders are deleted and replaced after verification, matching normal addon-manager behavior.
- App binary releases are produced from the native Rust binary with Cargo Packager. App self-updates use release metadata signed by the existing Brick Ed25519 feed key, verify the downloaded NSIS installer or AppImage SHA-256, and must not execute downloaded scripts.
- Brick uses Discord OAuth user tokens for login and must not embed a bot token or Discord client secret. Browser login redirects through the Brick Presence API, which stores only the short-lived one-time authorization code until the matching Brick client polls for it. Cached Discord sessions are stored under the user's Brick config directory, expire locally after 30 days, and are DPAPI-protected on Windows in `discord-auth.dat` instead of plaintext JSON.
- Client PCs do not get GitHub, CurseForge, or Wago tokens.
- The addon feed worker lives in the private Brick repo. It packages public ART source in GitHub Actions, then publishes signed public feed assets to `IsogiE/Brick-Releases`.
- The public addon repo should not contain Brick signing keys or feed publishing scripts.

If the addon signing key is rotated, ship a new Brick app release with the new `BRICK_ADDON_PUBLIC_KEY_B64` before publishing packages signed by the new private key.
