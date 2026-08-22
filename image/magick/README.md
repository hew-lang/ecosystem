# hew.image.magick

Owned ImageMagick 7 images for Hew. Transformations and I/O report typed
errors, and native image handles are released on every scope exit.

## System dependency

This package wraps ImageMagick 7 through `magick_rust`, so the MagickWand
libraries must be installed before anything here will link:

```sh
brew install imagemagick pkg-config          # macOS
sudo apt-get install libmagickwand-dev pkg-config   # Debian/Ubuntu
```

`pkg-config --modversion MagickWand` should report a 7.x version.

## Linking

`magick_rust` emits its link directives when it builds, but those directives do
not survive the trip through this package's staticlib into the final link line,
and a `hew.toml` `[native]` section has no way to name a system library that
the current compiler acts on. Until it does, the MagickWand libraries are named
on the command line:

```sh
hew run --pkg-path "$PWD" \
  $(pkg-config --libs MagickWand | sed 's/\([^ ][^ ]*\)/--link-lib \1/g') \
  image/magick/examples/basic.hew
```

From the ecosystem checkout, `make magick-example` runs exactly that. Any
program importing `hew.image.magick` needs the same flags. Without them the
link fails with `undefined symbol: MagickWandGenesis` and a few dozen siblings.

## Example

Save this as `main.hew` in a project depending on
`hew.image.magick = "0.3.0"`:

```hew
import hew.image.magick;

fn main() {
    let image = spawn magick.Image(source: Source.Blank(640, 480, "#3366cc"));
    let _ = await image.thumbnail(160, 120);
    let _ = await image.sharpen(0.0, 0.5);
    let png = match await image.write_blob("PNG") {
        .Ok(result) => match result {
            .Ok(value) => value,
            .Err(_) => bytes.new(),
        },
        .Err(_) => bytes.new(),
    };
    let _ = image.close();

    // A freshly created image carries no ImageMagick format tag until it is
    // decoded from real image data, so format() is checked on the reopened
    // blob rather than on `image` itself.
    let decoded = spawn magick.Image(source: Source.Blob(png));
    match await decoded.format() {
        .Ok(result) => match result {
            .Ok(value) => println(value),
            .Err(error) => println(magick.error_message(error)),
        },
        .Err(_) => println("image actor stopped before replying"),
    }
    let _ = decoded.close();
}
```

The checked example lives at [`examples/basic.hew`](examples/basic.hew); it
writes `magick-example.png` beside you and prints `magick-example.png is PNG`.

`write_blob` explicitly selects the encoded format. File output remains
available through `write`, where ImageMagick infers the format from the path.
