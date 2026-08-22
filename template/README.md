# hew.template

`hew.template` renders small Mustache-style HTML templates. Double-brace
placeholders escape HTML metacharacters; triple-brace placeholders insert
trusted, pre-rendered HTML unchanged.

The checked [render page example](examples/render_page.hew) is:

```hew
import hew.template;

fn main() {
    let context = "name\n<Ada & Co>\nbody\n<strong>Welcome</strong>";
    let page = template.render(
        "<h1>Hello, {{name}}!</h1><main>{{{body}}}</main>",
        context,
    );
    println(page);
}
```

Run it from the ecosystem checkout:

```sh
hew run --pkg-path . template/examples/render_page.hew
```

The program prints:

```html
<h1>Hello, &lt;Ada &amp; Co&gt;!</h1><main><strong>Welcome</strong></main>
```

Context is a newline-delimited sequence of key/value pairs. An unpaired key
has an empty value, duplicate keys use the last value, and an unknown
placeholder renders as empty text. Only LF (`\n`) separates fields; a CR
(`\r`), including the CR in a CRLF pair, remains part of its key or value.

Marker parsing commits to the longest opening marker. `{{{` therefore starts
a raw placeholder and must be closed by `}}}`. If its exact closing marker is
missing, the malformed marker and the rest of the template render literally;
the renderer does not retry at the second `{`. A valid double-brace marker
followed by an extra `}` renders its escaped value and preserves that extra
brace. A malformed raw marker is never reinterpreted as an escaped marker one
byte later: a template that opens `{{{` and never closes it renders literally
rather than silently changing meaning.

A lone `{{` or a lone `}}` has no matching marker and renders literally.

Rendering scans the context and template once, uses average constant-time
context lookup, and appends rendered UTF-8 into a growable buffer. Expected
time is linear in the context, template, and rendered output sizes. Use raw
triple-brace placeholders only for HTML you already trust.
