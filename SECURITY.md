# Security

Brick verifies Ed25519 signatures on update manifests and SHA-256 hashes on downloaded packages. Downloads and extracted archives have size limits. Addon installation is restricted to `AdvanceRaidTools`, `AdvanceRaidTools_Libraries`, and `AdvanceRaidTools_Options`; archive paths outside those directories are rejected.

App updates only run a Brick installer verified against signed release metadata. Addon updates never execute downloaded scripts or code.

Discord login uses OAuth with PKCE. Windows stores cached sessions with DPAPI protection. Bot tokens and publishing credentials belong on the server or in the release environment, never in the desktop app.

The Discord guild check controls the app's normal workflow. Release downloads are public; the desktop login is not server-side access control for those files.
