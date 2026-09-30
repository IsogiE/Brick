#!/bin/bash
# Build the reviewed upstream runtime when the distribution has not caught up.
set -euo pipefail
version=2.54.0
sha256=846fd19ccedbae1dbfe904f26dbf2d68a800a33a50caf2ad5222c8dcb3f25682
if pkg-config --atleast-version="$version" webkit2gtk-4.1; then
  exit 0
fi
if [[ $(id -u) != 0 || $(dpkg --print-architecture) != amd64 ]]; then
  echo 'The runtime builder requires an amd64 Ubuntu build environment and root.' >&2
  exit 1
fi
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
  build-essential pkg-config cmake ninja-build bison flex gperf gettext ruby python3 python3-jinja2 \
  python3-setuptools curl ca-certificates libgtk-3-dev libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev libicu-dev \
  libgcrypt20-dev libtasn1-6-dev libhyphen-dev libxslt1-dev libwebp-dev \
  libharfbuzz-dev libwoff-dev libenchant-2-dev libsecret-1-dev libmanette-0.2-dev \
  libjpeg-dev libjxl-dev libavif-dev liblcms2-dev libsystemd-dev libepoxy-dev \
  libsoup-3.0-dev libseccomp-dev libsqlite3-dev libegl1-mesa-dev libgles2-mesa-dev \
  libgbm-dev libdrm-dev libwayland-dev wayland-protocols bubblewrap xdg-dbus-proxy \
  libunwind-dev libdw-dev libevent-dev unifdef libxcomposite-dev libxdamage-dev \
  libxt-dev libxrandr-dev libxml2-dev libpng-dev libfontconfig1-dev libfreetype-dev
build_root=$(mktemp -d /var/tmp/brick-webkit.XXXXXXXX)
trap 'rm -rf -- "$build_root"' EXIT
curl --fail --location --retry 3 -o "$build_root/source.tar.xz" \
  "https://webkitgtk.org/releases/webkitgtk-$version.tar.xz"
printf '%s  %s\n' "$sha256" "$build_root/source.tar.xz" | sha256sum -c -
tar -xf "$build_root/source.tar.xz" -C "$build_root"
cmake -S "$build_root/webkitgtk-$version" -B "$build_root/build" -G Ninja \
  -DPORT=GTK -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr \
  -DCMAKE_INSTALL_LIBDIR=lib/x86_64-linux-gnu \
  -DCMAKE_INSTALL_LIBEXECDIR=lib/x86_64-linux-gnu -DUSE_GTK4=OFF \
  -DENABLE_DOCUMENTATION=OFF -DENABLE_INTROSPECTION=OFF \
  -DENABLE_MINIBROWSER=OFF -DENABLE_WEBDRIVER=OFF -DENABLE_GAMEPAD=OFF \
  -DUSE_FLITE=OFF -DENABLE_SPEECH_SYNTHESIS=OFF -DUSE_SYSTEM_SYSPROF_CAPTURE=OFF -DUSE_LIBBACKTRACE=OFF
# Bound compiler parallelism for the disposable build VM's 12 GiB RAM limit.
cmake --build "$build_root/build" --parallel 4
cmake --install "$build_root/build" --strip
# Preserve the source identity and notices alongside distribution notices.
notice=/usr/share/doc/brick-webkit-runtime/copyright
mkdir -p "$(dirname "$notice")"
{
  printf 'WebKitGTK %s\nSource: https://webkitgtk.org/releases/webkitgtk-%s.tar.xz\nSHA-256: %s\n\n' "$version" "$version" "$sha256"
  for file in Source/JavaScriptCore/COPYING.LIB Source/WebCore/LICENSE-APPLE Source/WebCore/LICENSE-LGPL-2 Source/WebCore/LICENSE-LGPL-2.1 Source/ThirdParty/skia/LICENSE; do
    printf '\n--- %s ---\n' "$file"
    cat "$build_root/webkitgtk-$version/$file"
  done
} > "$notice"
ldconfig
pkg-config --atleast-version="$version" webkit2gtk-4.1
pkg-config --modversion webkit2gtk-4.1
