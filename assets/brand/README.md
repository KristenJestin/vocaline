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
| `screen.svg` | a scene of Vocaline, drawn (no capture), on a transparent ground: a message being written, and the pill at the bottom of the screen with the voice, the phrase already frozen in white and its end still forming in grey; a dotted arrow shows where the dictation lands when you stop |
| `social-preview.svg` | the social preview of the repository and the banner of the README: the mark, the name and the tagline, the header every project of the maintainer shares, and under it `screen.svg` on a plain screen |

`png/` holds the icon at 128, 256, 512 and 1024 pixels, the full icon at 512, the favicon at 16
and 32, `apple-touch-icon.png`, and `social-preview.png`; `favicon.ico` holds the favicon at 16,
32 and 48 pixels.

## The social preview

`tools/brand.sh` renders `png/social-preview.png` from `social-preview.svg`, and `png/screen.png` from `screen.svg`, with `rsvg-convert`
(librsvg), with the fonts of [`fonts/`](fonts/README.md) only: Fredoka for the name, Open Sauce
Sans for the text. `social-preview.svg` embeds `screen.svg`, so both are rendered together. In `screen.svg`, the pill and its caret follow the width of the phrase: changing a word means measuring it again. Edit the SVG, run it again, commit both.

`png/social-preview.png` is uploaded by hand in the repository's settings on GitHub (Settings,
General, Social preview), as GitHub offers no other way.
