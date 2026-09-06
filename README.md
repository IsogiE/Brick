# Brick

[![Release](https://img.shields.io/github/v/release/IsogiE/Brick-Releases?label=release)](https://github.com/IsogiE/Brick-Releases/releases/latest)
[![Build](https://github.com/IsogiE/Brick/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/IsogiE/Brick/actions/workflows/ci.yml)
![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20Linux-blue)
[![License](https://img.shields.io/github/license/IsogiE/Brick)](LICENSE)
[![Issues](https://img.shields.io/github/issues/IsogiE/Brick)](https://github.com/IsogiE/Brick/issues)

A small desktop updater for [Advance Raid Tools](https://github.com/IsogiE/AdvanceRaidTools), written in Rust with egui.

Brick keeps the guild addon installed across your WoW clients, runs in the system tray, and checks for updates automatically. Downloads are verified against signed manifests before installation. Discord sign-in is required for Advance guild members to use the app.

## Download

Get the latest build from [Releases](https://github.com/IsogiE/Brick-Releases/releases/latest).

| Platform | Package |
| --- | --- |
| Windows | `.exe` installer; installs for the current user |
| Linux | AppImage, Debian `.deb`, or Arch package archive with `PKGBUILD` |

Run Brick, sign in with Discord, and select your World of Warcraft folder. You can add more than one installation. Retail, Classic, PTR, and beta clients are supported.

Discord credentials are protected with Windows DPAPI or the Linux desktop keyring (Secret Service, such as GNOME Keyring or KWallet). Linux needs an unlocked keyring to save or restore a login.

Closing or minimizing the window keeps Brick in the tray. **Open at login** controls startup; **Start minimized** keeps that login launch in the tray. Brick's own updates appear in the app when a new release is available.

## Build from source

Use the current stable Rust toolchain. On Windows, install the MSVC C++ build tools and Windows SDK. On Ubuntu/Debian, install the native dependencies first:

```sh
sudo apt install build-essential pkg-config libssl-dev libdbus-1-dev \
  libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libxkbcommon-dev libxkbcommon-x11-dev librsvg2-dev
```

```sh
git clone https://github.com/IsogiE/Brick.git
cd Brick
cargo run --locked
```

The desktop app builds without production credentials. Discord sign-in and signed update feeds need service configuration for a working deployment.

Build an optimized executable with `cargo build --release --locked`. To package it, install Cargo Packager:

```sh
cargo install cargo-packager --locked
```

Then run `cargo packager --release --formats nsis` on Windows, or `NO_STRIP=1 cargo packager --release --formats appimage` on Linux. Linux AppImage packaging also needs `patchelf`. Packages are written to `dist/packages/`.

## Working on the code

| Path | Contents |
| --- | --- |
| `src/ui.rs`, `src/tray.rs` | Native UI and tray behavior |
| `src/addon.rs` | WoW detection, settings, and addon installation |
| `src/app_update.rs`, `src/download.rs` | App updates and bounded downloads |
| `src/discord_auth.rs`, `src/presence.rs` | Discord login and roster client |
| `presence/` | Node.js roster and OAuth callback service |
| `scripts/`, `packaging/` | Release tooling, installer template, and smoke tests |

Run the checks before opening a pull request:

```sh
cargo fmt --check
cargo check --locked
cargo test --locked
node --test scripts/publish-addon-feed.test.mjs presence/*.test.mjs
```

Node.js 22 is used for the service and release scripts. The desktop app itself only needs Rust and its native dependencies. CI checks Windows and Linux.

Bug reports should include the Brick version, operating system, and steps to reproduce. For tray or startup issues, mention whether Brick was opened manually or started at login.

## License

[MIT](LICENSE).
