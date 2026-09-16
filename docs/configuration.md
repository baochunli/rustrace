# Configuration

Rustrace reads `$XDG_CONFIG_HOME/rustrace/config.toml`, or
`~/.config/rustrace/config.toml` when `XDG_CONFIG_HOME` is unset, on Linux and
macOS. The file is optional.

## Update settings

Update settings are not in `config.toml`. Automatic daily checks are on by
default. Choose Automatic checks: On/Off in the F7 menu; the preference is
persisted immediately and takes effect at the next launch. Changing it makes
no request. The preference and cached release status are stored in
`$XDG_STATE_HOME/rustrace/update-state.json`, or
`~/.local/state/rustrace/update-state.json` when `XDG_STATE_HOME` is unset.
Keep this directory outside assignments. See [Privacy](privacy.md#update-checks)
for what is sent and [Updating Rustrace](installation.md#updating-rustrace)
for terminal commands and restart instructions.

## Primary modifier

```toml
modifier = "auto"
```

The accepted values are `auto`, `command`, and `control`. `auto` uses Command
on macOS when the terminal exposes it through the kitty keyboard protocol and
Control otherwise. Control shortcuts remain aliases in Command mode.

## Theme

Choose one of the three built-ins and optionally override individual colours:

```toml
[theme]
name = "catppuccin"             # catppuccin | catppuccin-latte | terminal
auto_switch = false
light_name = "catppuccin-latte"
dark_name = "catppuccin"

[theme.custom]
error_bg = "#f38ba8"
warning_bg = "#f9e2af"
tab_active_bg = "#89b4fa"
tab_active_fg = "#1e1e2e"
accent = "#89b4fa"
selection_bg = "#313244"
```

`catppuccin` is the dark true-colour theme. `catppuccin-latte` uses dark text,
light surfaces, and light diagnostic tints. `terminal` uses the terminal's
16-colour ANSI palette. With no theme name, Rustrace preserves its earlier
selection rule: `COLORTERM=truecolor` or `24bit` selects `catppuccin`, and any
other value selects `terminal`.

`auto_switch = true` sends one bounded OSC 11 terminal-background query when a
student or replay TUI starts. A light reply selects `light_name`; a dark reply
selects `dark_name`; no reply keeps name. `auto_switch = false` sends no query.

Custom values must use six-digit `#RRGGBB` hexadecimal notation. Application
order is built-in, then [theme.custom]. An unset `error_bg`, `warning_bg`,
`tab_active_bg`, or `tab_active_fg` keeps the value derived by the selected
built-in; changing `red`, `yellow`, `accent`, or `panel_bg` does not implicitly
change those explicit fields.

Every palette field is overridable:

- `accent`
- `panel_bg`
- `sidebar_bg`
- `active_row_bg`
- `selection_bg`
- `surface0`
- `surface1`
- `surface_dim`
- `overlay0`
- `overlay1`
- `text`
- `subtext0`
- `mauve`
- `green`
- `yellow`
- `red`
- `blue`
- `teal`
- `peach`
- `error_bg`
- `warning_bg`
- `tab_active_bg`
- `tab_active_fg`

An invalid value, unreadable file, or unknown key falls back to the default
configuration and produces one startup warning. `rustrace doctor` reports the
effective theme and every palette value with its `custom` or built-in source.
