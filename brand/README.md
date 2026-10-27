# Bothy brand

The Bothy mark is a mountain shelter with its door left ajar: a place that belongs to no one,
where whoever comes next finds what was left for them.

![Construction](construction.svg)

## Files

| File | Use |
|---|---|
| `bothy-mark.svg` | the mark, green |
| `bothy-mark-light.svg` | the mark on dark backgrounds |
| `bothy-mark-black.svg` | one-colour print |
| `bothy-logo-horizontal.svg`, `bothy-logo-horizontal-dark.svg` | mark and wordmark side by side |
| `bothy-logo-vertical.svg` | wordmark under the mark |
| `favicon.svg` | site icon |
| `app-icon.svg`, `app-icon-macos.svg` | sources of the demo app's icons (Android and iOS; macOS) |
| `bothy-google-120.png` | OAuth consent screen logo (120 × 120) |
| `png/` | transparent PNGs of every file above, named `<name>-<width>.png` (marks 256–2048 px, lockups 800–3200 px, app icons 512 and 1024 px, favicon 32, 180 and 512 px) |

Every file is a plain filled path, with no strokes and no live text, so it renders the same in
any browser or design tool.

## Construction

The proportions come from the golden ratio φ ≈ 1.618. For a mark of width `w`:

| Measure | Rule |
|---|---|
| width : height | √φ |
| roof rise : wall height | √φ |
| door width | wall ÷ φ |
| wall left of the door : wall right of it | φ² |
| door top | its right end on the eave line, falling by door ÷ φ² towards it |
| corner radius, every corner | w ÷ φ⁷ |

**Wordmark:** "bothy" in [Outfit](https://github.com/Outfitio/Outfit-Fonts) SemiBold (600),
outlined, with tracking −0.02 em. Its "b" is the mark's height ÷ φ. Between mark and wordmark
the gap is the mark's height ÷ φ², and around a lockup the clear space is the height ÷ φ³.
Outfit is under the SIL Open Font License (`Outfit-OFL.txt`).

## Colours

| | Hex |
|---|---|
| Moss (mark) | `#2f4a39` |
| Ink (text) | `#1b2420` |
| Stone (on dark) | `#e8e3d8` |
| Night (dark background) | `#141a17` |

Keep the mark in one colour. Do not stretch it, rotate it, or put it inside other shapes, except
the app icon tile.
