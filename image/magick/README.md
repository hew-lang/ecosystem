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

`magick_rust`'s link directives do not survive the trip through this package's
staticlib, so `hew.toml` names the libraries itself with
`pkg-config = ["MagickWand"]` in `[native]`. Every program that imports
`hew.image.magick` links MagickWand without extra flags:

```sh
hew run --pkg-path . image/magick/examples/basic.hew
```

## Example

Save this as `main.hew` in a project depending on
`hew.image.magick = "0.4.0"`:

```hew
import hew.image.magick;

fn main() {
    let image = spawn magick.Image { source: Source.Blank(640, 480, "#3366cc") };
    let _ = image.thumbnail(160, 120);
    let _ = image.sharpen(0.0, 0.5);
    let png = match image.write_blob("PNG") {
        .Ok(value) => value,
        .Err(_) => bytes.new(),
    };
    stop(image);
    stopped(image);

    // A freshly created image carries no ImageMagick format tag until it is
    // decoded from real image data, so format() is checked on the reopened
    // blob rather than on `image` itself.
    let decoded = spawn magick.Image { source: Source.Blob(png) };
    match decoded.format() {
        .Ok(value) => println(value),
        .Err(.Failed(error)) => println(magick.error_message(error)),
        .Err(_) => println("image actor stopped before replying"),
    }
    stop(decoded);
    stopped(decoded);
}
```

The checked example lives at [`examples/basic.hew`](examples/basic.hew); it
writes `magick-example.png` beside you and prints `magick-example.png is PNG`.

Every `Image` method is declared `-> T fails MagickError`, so a call replies
`Result<T, ActorError<MagickError>>`: `.Ok(value)` on success,
`.Err(.Failed(error))` for an image error, and another `.Err` variant when the
actor could not answer (for example `.Dead` after it stopped).

`write_blob` explicitly selects the encoded format. File output remains
available through `write`, where ImageMagick infers the format from the path.
