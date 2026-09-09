# Themes

Quarkdrive ships with a handful of looks. **Galaxy** — deep violet with a slow
starfield — is the default. The rest are there because a file manager you stare
at all day should be able to match the room you are staring at it in.

Everything is a CSS custom property, so a theme is data rather than code: there
is no second stylesheet and nothing to rebuild.

## Choosing a theme

Click 🎨 in the top bar. Each card shows its three main colours; click to
switch. The choice is remembered in this browser, so sign in from another
machine and you will get Galaxy again until you pick there too.

| Theme    | Light/dark | Character                          |
|----------|------------|------------------------------------|
| Galaxy   | dark       | Violet starfield — the default     |
| Nebula   | dark       | Magenta clouds, warmer             |
| Midnight | dark       | Near-black blue, highest contrast  |
| Aurora   | dark       | Cold teal with a green wash        |
| Ember    | dark       | Warm charcoal, amber accent        |
| Paper    | light      | Plain, high legibility             |
| Linen    | light      | Warm off-white, low glare          |

## Making your own

🎨 → **New theme**. You start from the theme you are currently using, so you
only change what you care about — one colour is enough to be a theme; the rest
falls back to Galaxy.

The editor previews as you type and nothing is stored until you press
**Save & use**. Cancel throws the draft away and puts the old theme back.

**Colours.** The ten properties everything is drawn from: page background,
panels, hover, borders, text, secondary text, accent, text-on-accent, danger
and success. Each is a picker plus a hex box. The translucent accent glow used
for focus rings and hover states is derived from the accent automatically,
since it is the only sensible value for it.

**Background image.** Paste a URL, or upload a picture. Uploaded images are
shrunk to fit a 1920-pixel box and re-encoded before they are stored, which
keeps them comfortably inside the browser's storage limit. Panels turn
translucent and blur whatever is behind them when an image is present, so the
picture reads as a backdrop rather than a wallpaper. Clear it with **Remove
image**.

## Sharing

**Export…** writes a `.quarkdrive-theme.json` file containing the theme's
variables. **Import…** reads one back — it is checked for a `vars` block and
given a fresh id if the name collides, so importing someone else's theme
cannot clobber yours.

A theme file is small and human-readable:

```json
{
  "quarkdriveTheme": 1,
  "name": "Storm",
  "vars": {
    "--bg": "#0d1117",
    "--accent": "#58a6ff",
    "--text": "#e6edf3"
  }
}
```

Anything you leave out is inherited from Galaxy, which is why that file is
three lines long and still a complete theme.

## Notes

- Themes are per browser, not per account. There is deliberately no server
  side of this: the server does not know or care what colour you like, and a
  theme cannot leak anything about you.
- Because a theme is just variables, one written by hand in the JSON above
  works as well as one built in the editor.
- The colours that matter most for legibility are `--text` against `--bg`, and
  `--accent-contrast` against `--accent`. If a custom theme is hard to read,
  those are the pairs to fix.
