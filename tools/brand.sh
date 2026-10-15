#!/usr/bin/env bash
# Renders assets/brand/png/social-preview.png from assets/brand/social-preview.svg, with the fonts
# of assets/brand/fonts only: the machine's fonts would change the drawing from one machine to
# the next. Needs rsvg-convert (librsvg).
set -euo pipefail

brand="$(cd "$(dirname "$0")/../assets/brand" && pwd)"
run="$(mktemp -d)"
trap 'rm -rf "$run"' EXIT

cat >"$run/fonts.conf" <<CONF
<?xml version="1.0"?><!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig><dir>$brand/fonts</dir><cachedir>$run/cache</cachedir></fontconfig>
CONF

FONTCONFIG_FILE="$run/fonts.conf" rsvg-convert "$brand/social-preview.svg" -o "$brand/png/social-preview.png"
echo "wrote $brand/png/social-preview.png"
