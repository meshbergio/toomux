# toomux brand assets

The toomux logo, mark, colours and social images, as vector files that need no
fonts. Use them to link to toomux, write about it or build something that works
with it.

## Files

| Folder | What's in it |
|---|---|
| `logo/svg/` | The full logo (mark and wordmark side by side): `primary` for light backgrounds, `reversed` for dark ones, `stacked-*` with the mark above the wordmark, plus black, white and `currentcolor` versions. |
| `mark/svg/` | The figure-eight mark on its own, in graphite, mint (`signal`), black, white and `currentcolor`. |
| `wordmark/svg/` | The word "toomux" on its own, in graphite, ivory, black, white and `currentcolor`. |
| `master/` | The source drawings of the mark and the wordmark. |
| `app-icons/` | A square app icon and a maskable icon for Android and web apps. |
| `favicon/` | The favicon as SVG. |
| `social/` | Link-preview images (1200×630) for light and dark, as SVG and PNG. |
| `tokens/` | The colours and logo proportions as CSS custom properties and JSON. |

The README uses `logo/svg/toomux-logo-primary.svg` and, in dark mode,
`logo/svg/toomux-logo-reversed.svg`.

## Colours

| Name | Hex | Use |
|---|---|---|
| Graphite | `#121619` | text and the logo on light backgrounds; dark backgrounds |
| Ivory | `#F6F5F1` | the wordmark on graphite; light backgrounds |
| Mint | `#57E6BE` | the mark on graphite; accents |
| Slate | `#687178` | secondary text |

## Using the logo

- On light backgrounds, use the primary (graphite) logo. On dark backgrounds,
  use the reversed one (mint mark, ivory wordmark).
- In the side-by-side logo the mark is about 11% taller than the wordmark.
  Scale the file as a whole rather than resizing its parts.
- The two breaks in the figure-eight are part of the design: don't close them.
- Don't add glows, gradients, shadows or other effects.
- `currentcolor` versions take the colour of the surrounding text, which suits
  inline use in HTML.

## License

These files are part of the toomux repository and are licensed like the rest
of it, under [MIT](../../LICENSE-MIT) or [Apache 2.0](../../LICENSE-APACHE).
When you use them to refer to toomux, please don't present your project as
toomux or as endorsed by it.
