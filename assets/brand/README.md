# The logo of Vocaline

The mark: a voice wave in yellow that flattens into a line, ending on a text caret in cream. The
voice becomes text. The SVGs of this folder are the one source of the drawing; the colours and the
transforms are the maintainer's, never redrawn by a tool.

| Colour | Value |
| --- | --- |
| the wave, on a dark background | `#edd643` |
| the wave, on a light background | `#937800` |
| the caret, cream | `#ece5d8` |
| the background | `#14120f` |

| Source | For |
| --- | --- |
| `mark.svg` | the mark alone, for a dark background (the default) |
| `mark-on-light.svg` | the mark alone, for a light background |
| `icon.svg` | the icon: the mark on a dark rounded square |
| `icon-full.svg` | the full icon, a yellow square with dark strokes |
| `favicon.svg` | the favicon, drawn for 16 to 64 pixels |
| `social-preview.svg` | the social preview of the repository and the banner of the README: the mark, the name, the tagline, and a dictation as the event stream tells it, drawn (no capture) |

`png/` holds the icon at 128, 256, 512 and 1024 pixels, the full icon at 512, the favicon at 16
and 32, `apple-touch-icon.png`, and `social-preview.png`; `favicon.ico` holds the favicon at 16,
32 and 48 pixels.

## The social preview

`tools/brand.sh` renders `png/social-preview.png` from `social-preview.svg` with `rsvg-convert`
(librsvg), with the fonts of [`fonts/`](fonts/README.md) only: Fredoka for the name, Open Sauce
Sans for the text, JetBrains Mono for the events. Edit the SVG, run it again, commit both.

`png/social-preview.png` is uploaded by hand in the repository's settings on GitHub (Settings,
General, Social preview), as GitHub offers no other way.
