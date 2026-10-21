#!/usr/bin/env bash
# Existing CI prerequisites for native package builds, on disposable runners.
set -euo pipefail
mode="${1:?usage: setup-native-system-deps.sh base|magick}"
[[ "$mode" == base || "$mode" == magick ]] || { echo "unsupported native dependency set" >&2; exit 1; }
sudo apt-get update
sudo apt-get install --no-install-recommends -y build-essential libssl-dev pkg-config
[[ "$mode" == magick ]] || exit 0
sudo apt-get install --no-install-recommends -y libclang-dev libjpeg-dev libltdl-dev libpng-dev libtiff-dev libxml2-dev zlib1g-dev
IMAGEMAGICK_VERSION=7.1.1-47
IMAGEMAGICK_SHA256=818e21a248986f15a6ba0221ab3ccbaed3d3abee4a6feb4609c6f2432a30d7ed
IMAGEMAGICK_PREFIX=/usr/local
archive="${RUNNER_TEMP:?}/ImageMagick-$IMAGEMAGICK_VERSION.tar.gz"
source_dir="${RUNNER_TEMP:?}/ImageMagick-$IMAGEMAGICK_VERSION"
trap 'rm -rf "$source_dir" "$archive"' EXIT
curl --fail --location --retry 3 \
  "https://github.com/ImageMagick/ImageMagick/archive/refs/tags/$IMAGEMAGICK_VERSION.tar.gz" \
  --output "$archive"
echo "$IMAGEMAGICK_SHA256  $archive" | sha256sum --check --status
tar -xzf "$archive" --directory "$RUNNER_TEMP"
cd "$source_dir"
./configure \
  --prefix="$IMAGEMAGICK_PREFIX" \
  --disable-static \
  --without-magick-plus-plus \
  --without-perl
make --jobs 2
sudo make install
sudo ldconfig
