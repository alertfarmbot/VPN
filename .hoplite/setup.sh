#!/usr/bin/env bash
# Готовит песочницу к сборке и проверке Ferrogram.
# Состав согласован с владельцем проекта: Rust 1.99.1 с официального сервера
# и перечисленные ниже пакеты Ubuntu. Ничего сверх этого скрипт не ставит.
set -euo pipefail

RUST_VERSION=1.99.1
RUSTUP_VERSION=1.29.3
RUSTUP_SHA256=b1c8cb14e30889e7e2999e6b69e62d9e9b9ffc0f4df8180564d306d9680e6c55

if [ ! -x "$HOME/.cargo/bin/cargo" ]; then
    installer=$(mktemp)
    curl -sSfL "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init" -o "$installer"
    echo "${RUSTUP_SHA256}  ${installer}" | sha256sum -c -
    chmod +x "$installer"
    "$installer" -y --profile minimal --default-toolchain "$RUST_VERSION" --no-modify-path
    rm -f "$installer"
fi

if [ "$(id -u)" = 0 ]; then
    # Чтобы cargo был виден без подключения ~/.cargo/env.
    for tool in cargo rustc rustup; do
        ln -sf "$HOME/.cargo/bin/$tool" "/usr/local/bin/$tool"
    done

    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    # Первая строка нужна самому приложению, вторая — чтобы проверять интерфейс без монитора.
    apt-get install -y --no-install-recommends \
        libxkbcommon-x11-0 libxcursor1 libxi6 libxrandr2 libx11-xcb1 libfontconfig1 \
        xvfb xdotool imagemagick xclip x11-apps x11-utils fonts-noto-color-emoji
else
    echo "setup: нет прав root, системные пакеты не установлены" >&2
fi
