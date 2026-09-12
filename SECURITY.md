# Security

Brick verifies Ed25519 signatures on update manifests and SHA-256 hashes on downloaded packages. Downloads and extracted archives have size limits. Addon installation is restricted to `AdvanceRaidTools`, `AdvanceRaidTools_Libraries`, and `AdvanceRaidTools_Options`; archive paths outside those directories are rejected.

App updates only run a Brick installer verified against signed release metadata. Starting with 0.5.3, application updates and addon updates use independent Ed25519 keys. The application signing key is kept outside GitHub Actions. App manifests expire after at most 90 days, and Windows updates additionally require a trusted Authenticode signature from an approved Brick publisher. The updater holds the Windows installer and its parent paths against replacement while verifying and launching it. Addon updates never execute downloaded scripts or code.

Versioned releases are prepared as drafts, with all artifacts attached before publication as immutable releases. Release candidates carry GitHub build provenance; final artifacts also have detached application-key signatures. The public verification keys and Windows publisher certificate hashes are in `security/`. Verifying a first download requires obtaining those keys through an independently trusted copy; a key and binary fetched from the same compromised account do not establish independent trust.

The Linux AppImage bundles WebKitGTK. OS package updates do not update that bundled copy; new WebKit security releases require a new Brick release. Dependencies and release tools are pinned and security advisories are checked in CI. Pins must still be updated when upstream fixes are available.

Discord login uses OAuth with PKCE. Windows stores cached sessions with DPAPI protection. Bot tokens and publishing credentials belong on the server or in the release environment, never in the desktop app.

The Discord guild check controls the app's normal workflow. Release downloads are public; the desktop login is not server-side access control for those files.
