#!/usr/bin/env bash
# Existing CI prerequisites for native package builds, on disposable runners.
set -euo pipefail
mode="${1:?usage: setup-native-system-deps.sh base|magick}"
[[ "$mode" == base || "$mode" == magick ]] || { echo "unsupported native dependency set" >&2; exit 1; }
sudo apt-get update
sudo apt-get install --no-install-recommends -y build-essential pkg-config
[[ "$mode" == magick ]] || exit 0
sudo apt-get install --no-install-recommends -y libclang-dev libjpeg-dev libltdl-dev libpng-dev libtiff-dev libxml2-dev xz-utils zlib1g-dev
IMAGEMAGICK_VERSION=7.1.2-36
IMAGEMAGICK_SHA256=410cab5ed416329290e77cbd7833a76502671ea25a42d1cfe010f9067b27c2d6
IMAGEMAGICK_PREFIX=/usr/local
archive="${RUNNER_TEMP:?}/ImageMagick-$IMAGEMAGICK_VERSION.tar.xz"
source_dir="${RUNNER_TEMP:?}/ImageMagick-$IMAGEMAGICK_VERSION"
trap 'rm -rf "$source_dir" "$archive"' EXIT
curl --fail --location --retry 3 \
  "https://github.com/ImageMagick/ImageMagick/releases/download/$IMAGEMAGICK_VERSION/ImageMagick-$IMAGEMAGICK_VERSION.tar.xz" \
  --output "$archive"
echo "$IMAGEMAGICK_SHA256  $archive" | sha256sum --check --status
tar -xJf "$archive" --directory "$RUNNER_TEMP"
cd "$source_dir"
./configure \
  --prefix="$IMAGEMAGICK_PREFIX" \
  --disable-static \
  --without-magick-plus-plus \
  --without-perl
make --jobs 2
sudo make install
sudo ldconfig
