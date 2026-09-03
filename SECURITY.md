# Security Notes

Brick has one high-risk job: it writes files onto guild members' computers automatically. The app is built around keeping that power narrow.

- The app does not run Git, shell scripts, Lua, or downloaded executables.
- The addon feed manifest must be signed with the embedded Ed25519 public key.
- The addon zip must match the SHA-256 hash from the signed manifest.
- Downloads must come from the `IsogiE/AdvanceRaidTools` `brick-feed` GitHub release URL namespace.
- Zip entries are rejected if they escape the allowed addon folders.
- Only these folders are managed: `AdvanceRaidTools`, `AdvanceRaidTools_Libraries`, and `AdvanceRaidTools_Options`.
- Existing managed folders are deleted and replaced after verification, matching normal addon-manager behavior.
- App binary releases are produced from the native Rust binary with Cargo Packager. When app self-updating is enabled, it must use signed release metadata and must not execute downloaded scripts.
- Client PCs do not get GitHub, CurseForge, or Wago tokens.
- The addon feed worker lives in the private Brick repo. It packages public ART source in GitHub Actions, then publishes signed public feed assets to `IsogiE/AdvanceRaidTools`.
- The public addon repo should not contain Brick signing keys or feed publishing scripts.

If the addon signing key is rotated, ship a new Brick app release with the new `BRICK_ADDON_PUBLIC_KEY_B64` before publishing packages signed by the new private key.
