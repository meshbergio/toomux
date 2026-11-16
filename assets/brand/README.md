# toomux identity: Thread

A lowercase **t** whose stroke fades into a gold thread that runs off the edge of the icon: many sessions, one place, and the thread is never lost.

Approved by Sam on 2026-11-16. Generated from the brand source (toomux-brand-v3, `v5/kit.py`). Regenerate rather than editing these files by hand.

## Files

| Folder | What | Use |
|---|---|---|
| `svg/toomux-icon.svg` | App icon, dark (master) | Anywhere 32 px and up |
| `svg/toomux-icon-light.svg` | App icon on a paper tile | Light contexts that need a light tile |
| `svg/toomux-icon-small*.svg` | Pixel-snapped drawing, hard colour cut | 16 and 24 px only |
| `svg/toomux-wordmark-on-{dark,light}.svg` | Custom wordmark, transparent background | Headers, site, docs |
| `svg/toomux-lockup-on-{dark,light}.svg` | Icon + wordmark, transparent | Main logo; the icon tile stays dark on both |
| `png/app-icon/` | Dark icon, 16 to 1024 px (16 and 24 use the small drawing) | Launchers, docks, app bundles |
| `png/app-icon-light/` | Light icon, same sizes | |
| `png/wordmark/`, `png/lockup/` | 64, 128 and 256 px tall | Where SVG isn't accepted |
| `favicon/` | `favicon.ico` (16/32/48), `favicon.svg`, `apple-touch-icon.png` (180), `icon-192/512.png` | Web |

## Colour

| Name | Hex | |
|---|---|---|
| Ink | `#121014` | Dark background, text on light |
| Paper | `#F3EFE7` | Light background, text on dark |
| Thread (dark) | `#D3A86C` to `#EBCF9C` | Gold that brightens as it runs on |
| Thread (light) | `#A87A3C` to `#BF9255` | Deeper bronze-gold for paper |
| Icon tile | `#1C191F` to `#0E0C10` (vertical) | Plus a 7% white hairline on the edge |

The fade from the stroke colour into gold is computed in OKLCH with the hue held at gold, so on light the ink goes through bronze rather than muddy brown.

## Rules

- Always lowercase: `toomux`.
- Keep clear space around the lockup of at least half the icon's height.
- Don't recolour the thread, stretch the icon, or add effects. Don't put the wordmark's gold t on a mid-tone background.
- Below 32 px, use the small drawing; the slant and the fade blur at that size.
- The thread is clipped by the icon's own outline. It isn't a separate piece, so don't extend it beyond the tile.

## Construction

The icon is drawn on a 100-unit grid: stem 11, bar 9.6, top at 18 with a slanted cut (3.5 units rise), bar centred at 39, a superellipse turn (n 2.15, radius 20) into the baseline at 77, and the gold starting at 55. The tile is a rounded square with continuous corners.
