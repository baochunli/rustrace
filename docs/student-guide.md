# Rustrace student guide

Rustrace is a terminal editor that records how you work on a Rust assignment.
You write code inside it, run Cargo inside it, and at the end it prepares one
ZIP file that you upload to the course LMS yourself. Rustrace has no server: it
never uploads anything, and it cannot tell whether your LMS upload succeeded.

This guide covers the pilot workflow, including the bounded console Test
options described below.

## Recommended terminals

Install Ghostty, kitty, WezTerm, or iTerm2 before the course. These terminals
deliver Command shortcuts through the kitty keyboard protocol and honour
Rustrace's one-way system clipboard mirror. In iTerm2, enable the
`Applications in terminal may access clipboard` preference.

Rustrace mirrors text that you copy or cut inside the editor with an OSC 52
clipboard write. A matching delivered ⌘V then uses the recorded internal
source, so it works like ctrl-V. Terminal settings can still reserve individual
Command shortcuts.

macOS owns ⌘Q as the application Quit command. Ghostty binds ⌘W, ⌘A, ⌘C and
⌘V by default, and applications built on Ghostty inherit those bindings. In
particular, ⌘A is consumed by Ghostty's own Select All unless it is unbound in
the Ghostty configuration with `keybind = super+a=unbind`. Ctrl-A selects all
unless the startup probe observes Ghostty's exact `super+arrow_left` →
`text:\x01` rewrite. That rewrite changes Ctrl-A to line start only while the
keyboard enhancement is active; otherwise Ctrl-A still selects all. Right-click
in the editor and choose `Select all` for an unconditional mouse route.
The keybinds overlay spells the modifier `Control`; mode-bar hints and toasts
use `Ctrl`. Those compact hints use Ctrl-Q, Ctrl-W, Ctrl-C, Ctrl-X and Ctrl-V
because those forms reach the editor. They show Ctrl-A only when it selects
all; otherwise the keybind overlay shows `right-click → Select all`.
Rustrace still accepts the Command forms when a terminal delivers them.

Ghostty users should run `rustrace doctor assignment.rta` before the first
session. The Ghostty section reports which Rustrace setup chords the terminal
owns or rewrites. If it recommends the setup, run:

```console
rustrace doctor --write-ghostty-keys
```

The command writes `~/.config/ghostty/rustrace-keys`, or the matching path
under `$XDG_CONFIG_HOME`. Add the line it prints to your Ghostty config:

```text
config-file = rustrace-keys
```

Then reload through Ghostty's own `super+shift+,=reload_config` binding (⌘⇧,).
The command never edits Ghostty's main config, and it refuses to replace an
existing `rustrace-keys` file without its Rustrace marker. The snippet releases
⌘Up/Down/Left/Right, ⌘Backspace,
Option-Left/Right, ⌘F, ⌘Z, ⌘K, ⌘Home, and ⌘End. ⌘A, ⌘C, ⌘V, ⌘Q, and ⌘W stay
assigned to Ghostty so those global terminal actions keep working in shell
sessions.

Terminal.app provides the basic terminal features that Rustrace needs, but it
does not deliver the Command shortcuts or honour the clipboard mirror. Use the
ctrl shortcuts there. Ctrl-V remains the internal paste fallback, and ⌘V stays
blocked.

## Commands and keys at a glance

| Command | Purpose |
| --- | --- |
| `rustrace --version` | Print the version; add `--verbose` for build, format, and target details |
| `rustrace doctor assignment.rta [--workspace DIR]` | Check your machine before you start |
| `rustrace doctor --write-ghostty-keys` | Write the marker-protected Ghostty key snippet |
| `rustrace environment` | Older, narrower tool probe (rustup, rustc, Cargo, rust-analyzer, rustfmt, Clippy) |
| `rustrace work assignment.rta [--workspace DIR]` | Start or reopen an assignment |
| `rustrace work ... --resume` / `--inspect` / `--abandon` / `--restore-logical` | Explicit recovery controls |
| `rustrace status WORKSPACE` | Show whether an attempt is unfinished, finalized, or needs recovery |
| `rustrace privacy WORKSPACE` | Show exactly what the workspace would contribute to a bundle |
| `rustrace submit WORKSPACE --student-id ID [--allow-incomplete] [--output PATH]` | Finalize locally and write the ZIP |
| `rustrace revise PARENT_WORKSPACE NEW_WORKSPACE assignment.rta` | Start a new linked attempt after finalizing |
| `rustrace cleanup WORKSPACE [--confirm] [--destroy-provenance]` | Remove build output, or all local recorded data after grades |

| Key | In the editor |
| --- | --- |
| Ctrl-Q | Quit; macOS reserves ⌘Q for the application |
| ⌘S or Ctrl-S | Save all buffers, then run Check |
| ⌘F or Ctrl-F | Open the find and replace panel |
| F3 | Repeat the last find while the panel is closed |
| Ctrl-Space | Request completion (needs rust-analyzer) |
| Ctrl-C/X/V | Copy, cut, paste text from inside this workspace; Ghostty reserves ⌘C and ⌘V, and the Ctrl-X form keeps the clipboard hints consistent |
| ⌘Z/Y or Ctrl-Z/Y | Undo, redo |
| Ctrl-A | Select all unless the startup Ghostty probe sees the exact ⌘Left rewrite; right-click → Select all is always available |
| ⌘/ or Ctrl-/ | Add or remove line comments on the current or selected lines |
| ⌘Left / ⌘Right | Move to the start or end of the line |
| ⌘Up / ⌘Down | Move to the start or end of the document |
| Option-Left / Option-Right or Ctrl-Left / Ctrl-Right | Move to the previous or next word boundary |
| Ctrl-Home / Ctrl-End | Move to the start or end of the document |
| Option-Backspace or Ctrl-Backspace | Delete to the previous word boundary |
| ⌘Backspace | Delete to the start of the line |
| Shift with a movement chord | Extend the selection from its current anchor |
| F5, F6, Ctrl-Tab / Ctrl-BackTab | Previous or next open file |
| Ctrl-W | Ask to delete the selected file; Ghostty reserves ⌘W by default |
| Option-Up / Option-Down or alt-Up / alt-Down | Previous or next Cargo or live diagnostic |
| F1 | Open the curated non-obvious keybind reference |
| F7 | Command menu |
| F8 | Reload the language service |
| F9 | Embedded Cargo console |
| F4 | Open the modal packaged test-case picker |
| Mouse | Click or drag source, files, tabs, diagnostics, dividers, menus, and confirmation pills; right-click the editor for Cut, Copy, Paste, and Select all; right-click a file for its menu; wheel scrolls the pane under the pointer |

The on-screen keybind panel shows `⌘` when Command is effective and spells
Control chords with the full word `Control` in both modes. It shows `Option` or
`alt` for word navigation. Control forms remain accepted aliases in Command
mode. Rustrace can act only on chords that the terminal delivers. See
[Recommended terminals](#recommended-terminals) for the supported choices. Any
reserved shortcut is outside Rustrace's control; use Home and End for line
navigation when needed.
Every prompt that asks for confirmation accepts Y or Enter to proceed and N or
Esc to cancel.

### Injected macOS editing text

Some macOS terminal bindings send editing text or perform a terminal action
instead of delivering a modifier key. Ghostty 1.3.1 reports this complete
relevant default set from `ghostty +list-keybinds --default`:

```text
super+arrow_left=text:\\x01
super+arrow_right=text:\\x05
super+backspace=text:\\x15
alt+arrow_left=esc:b
alt+arrow_right=esc:f
super+arrow_up=jump_to_prompt:-1
super+arrow_down=jump_to_prompt:1
super+a=select_all
super+q=quit
super+w=close_surface
super+f=start_search
super+z=undo
super+k=clear_screen
super+c=copy_to_clipboard
super+v=paste_from_clipboard
super+home=scroll_to_top
super+end=scroll_to_bottom
```

Thus, Ghostty 1.3.1 defaults send `text:\x01` for ⌘Left, `text:\x05` for
⌘Right, `text:\x15` for ⌘Backspace, `esc:b` for Option-Left, and `esc:f` for
Option-Right. These are normal default bindings, not an opt-in preset. When
Rustrace has activated the kitty keyboard enhancement, the Option forms below
keep their injected-text meanings. The control bytes are ambiguous after
crossterm parsing, so the startup probe must report the corresponding exact
Ghostty rewrite. Only an exact startup-probe rewrite activates each
control-byte translation, without extending the selection:

- `super+arrow_left` → `text:\x01`: Ctrl-A → line start;
- `super+arrow_right` → `text:\x05`: Ctrl-E → line end;
- `super+backspace` → `text:\x15`: Ctrl-U → delete to line start;
- `super+k` → `text:\x0b`: Ctrl-K → delete to line end;
- Esc b → previous word;
- Esc f → next word;
- Esc Delete → delete previous word.

A passed binding, an unavailable probe, a non-Ghostty terminal, or an inactive
keyboard enhancement keeps the legacy meanings: Ctrl-A selects all, while
Ctrl-E, Ctrl-U, and Ctrl-K retain their usual editor behavior. The
escape-prefixed Option translations continue to depend only on the active
keyboard enhancement.

The Command-mode hints keep the ⌘Left, ⌘Right, ⌘Backspace, Option-Left, and
Option-Right forms only for these exact translations or when Ghostty passes the
chord unchanged. A different rewrite uses the corresponding Control fallback.

Ghostty's default ⌘Backspace binding sends `text:\x15`, the Ctrl-U form, so it
deletes to the start of the line when that exact rewrite is observed. A
terminal can opt into the Ctrl-K translation by exposing an exact
`super+k=text:\x0b` rewrite to the startup probe. Option-Backspace has no
Ghostty default binding; it arrives through the kitty protocol as Alt-Backspace,
which Rustrace handles directly. Without the keyboard enhancement these inputs
keep their usual meanings; in particular, Ctrl-A selects all.

⌘Up and ⌘Down have no injected equivalent and still require the kitty-protocol
Command-key route. Ghostty defaults them to `jump_to_prompt:-1` and
`jump_to_prompt:1`, so they never reach Rustrace. Ctrl-Home and Ctrl-End are the
always-available forms of document start and end. Ghostty also consumes ⌘F,
⌘Z, ⌘K, ⌘Home, and ⌘End for its own search, undo, screen clearing, and scroll
actions. The `rustrace doctor --write-ghostty-keys` command writes the following
unbind snippet so Ghostty delivers those navigation and editor chords through
the kitty protocol:

```text
keybind = super+arrow_up=unbind
keybind = super+arrow_down=unbind
keybind = super+arrow_left=unbind
keybind = super+arrow_right=unbind
keybind = super+backspace=unbind
keybind = alt+arrow_left=unbind
keybind = alt+arrow_right=unbind
keybind = super+f=unbind
keybind = super+z=unbind
keybind = super+k=unbind
keybind = super+home=unbind
keybind = super+end=unbind
```

`⌘A`, `⌘C`, `⌘V`, `⌘Q`, and `⌘W` are left to the terminal on purpose because
Ghostty bindings are global. This preserves Ghostty's select-all, copy, paste,
quit, and close-surface behavior across its surfaces rather than changing those
terminal-wide actions solely for Rustrace.

To see exactly what your terminal sends, run this capture command and press the
chords during its 15-second raw-input window. It activates the same flags as
Rustrace, prints the captured bytes, then restores the terminal:

```console
python3 -c 'exec("import os,termios,time,tty\nf=os.open(\"/dev/tty\",os.O_RDWR)\nold=termios.tcgetattr(f)\ndata=b\"\"\ntry:\n tty.setraw(f);os.set_blocking(f,False);os.write(f,b\"\\x1b[>5u\");time.sleep(15)\n while True:\n  try:data+=os.read(f,4096)\n  except BlockingIOError:break\nfinally:\n os.write(f,b\"\\x1b[<1u\");termios.tcsetattr(f,termios.TCSADRAIN,old);os.close(f)\nprint(repr(data))")'
```

## Install

Install Rustrace with:

```sh
curl -fsSL https://raw.githubusercontent.com/baochunli/rustrace/main/scripts/install.sh | sh
```

Install rustup from <https://rustup.rs/> first (version 1.28.1 or newer), plus
native compiler/linker tools: Xcode Command Line Tools on macOS or
`build-essential` on Debian/Ubuntu. The installer requires curl and Git. It
builds the latest release from source with Rust 1.98.1; this takes a few minutes.
If that toolchain is missing, it installs the minimal profile with clippy and
rustfmt. It never installs rustup or changes your default toolchain. Open a
new terminal if prompted, then run `rustrace --version`.

The pilot uses source builds on macOS Apple Silicon and Linux x86-64. Windows
users build inside WSL 2 and keep assignments in the WSL filesystem, not on a
mounted Windows drive. Install the assignment's pinned toolchain separately
when `doctor` names it; the installer only prepares Rust 1.98.1 for Rustrace.

You can instead use Cargo directly:

```sh
cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag vX.Y.Z rustrace --locked
```

Replace `vX.Y.Z` with the latest release tag from
[GitHub Releases](https://github.com/baochunli/rustrace/releases), and install Rust
1.98.1 first. From a checkout of that tag, run `cargo install --path . --locked`
at the repository root. The root package ships only the `rustrace` binary, so
neither command needs `--bin`. The Git form names the package because the
repository also contains executable fixture manifests. See the
[installation guide](installation.md) for PATH, custom roots, and uninstalling.

On macOS, Linux, and WSL 2, no platform-specific first-launch steps are
needed for a locally built binary. Confirm the installation with
`rustrace --version`; terminal shortcut and clipboard preferences are described
above under Recommended terminals.

## Updating Rustrace

Run `rustrace update --check` to check the latest published release explicitly.
It prints whether your installed version is up to date, a newer version is
available, or the check is unavailable. Automatic daily checks are on by default
and finish before your recording session starts. You can work offline;
`rustrace doctor assignment.rta` reports cached version status. See
[Privacy](privacy.md#update-checks) for the request details.
In the F7 menu, Update Rustrace opens a panel that reads only the cache and
installation receipt. It shows `Installed: X.Y.Z`, `Latest known: A.B.C` (or
`unknown`), and `Last checked: <relative time>` (or `never`). Esc or Enter
closes it. The panel never makes a network request or starts an update during a
session. A newer cached release adds `NEW` to the row and an accent `● menu`
footer. An unknown cache shows `Latest release is unknown. Quit, then run:
rustrace update --check` for every copy. A latest release that is not newer
shows `Rustrace is up to date.` for every copy. A newer release shows `Rustrace
vA.B.C is available. Quit, then run: rustrace update` for installer-managed
copies and the exact Cargo install command below with the real tag for other
copies.

Choose Automatic checks: On/Off in the menu to flip the runtime preference.
The preference is persisted immediately and takes effect at the next launch.
There is no `config.toml` update knob. Doctor includes `automatic checks on/off`
in its cached version advisory.

Quit Rustrace before updating. Run `rustrace update` from a terminal; it
checks the latest release and builds from source with Cargo and Rust 1.98.1
when a newer version is available. This takes a few minutes and displays Cargo's
progress. Success prints `Installed X.Y.Z. Restart Rustrace to use it.` Restart
Rustrace to use the new build. Already running sessions retain their original
recorded version and build identity. A failed Cargo build keeps the old binary.
If post-install identity validation fails, the new binary is already installed
at the reported path; follow the printed manual remedy to repair it.

The updater requires the installer's `$XDG_STATE_HOME/rustrace/install.json`
receipt (default `~/.local/state/rustrace/install.json`) matching the executable.
Cargo-managed copies without that receipt, unknown installation methods, and
moved executables receive the exact manual remedy with the latest release tag:
`cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag vX.Y.Z rustrace --locked --force`.
Use the original Cargo install root when running that remedy. After pulling a
new version in a checkout, reinstall with `cargo install --path . --locked --force`.
The `--force` is required because an older installed binary silently lacks newer
shortcuts even though the source tree is current.

rust-analyzer is optional. Without it you still get editing, recording, Cargo
commands, and submission; you lose code completion. Install it into the
assignment's toolchain with `rustup component add rust-analyzer` (add
`--toolchain <version>` if the pinned toolchain is not your default). rustfmt
and Clippy are also optional and only disable the Format and Clippy commands.

Rustrace needs a real terminal at least 80 columns by 24 rows with UTF-8 and
bracketed paste. Use one of the [recommended terminals](#recommended-terminals)
for Command shortcuts and clipboard mirroring. Windows Terminal attached to
WSL 2 and mainstream Linux terminals also provide the baseline features.

### Choose the primary modifier

The optional configuration file is
`$XDG_CONFIG_HOME/rustrace/config.toml`, or
`~/.config/rustrace/config.toml` when `XDG_CONFIG_HOME` is unset, on both Linux
and macOS. The top-level modifier setting is optional:

```toml
modifier = "auto"
```

The accepted values are `"auto"`, `"command"`, and `"control"`. `"auto"`
uses Command on macOS when the terminal reports kitty keyboard-protocol
support, and Control everywhere else. `"command"` requests Command but falls
back to Control with a one-time startup toast when support is unavailable.
`"control"` always uses Control. A missing file means `"auto"`; an invalid
value, unreadable file, or unknown key produces a dismissible startup toast
that names the file and then uses `"auto"`.

### Choose a colour scheme

The same configuration file accepts a theme table. Rustrace includes
`catppuccin`, the light `catppuccin-latte`, and `terminal`, which preserves the
host's 16-colour ANSI palette. With no `[theme]` table, an explicit
`COLORTERM=truecolor` or `COLORTERM=24bit` keeps the existing `catppuccin`
default; other values use `terminal`.

```toml
[theme]
name = "catppuccin"
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

Set `auto_switch = true` to make one bounded OSC 11 query when the student or
replay TUI starts. A light reply selects `light_name`, a dark reply selects
`dark_name`, and no reply keeps name. Set individual `[theme.custom]` values
as six-digit hexadecimal colours. Application order is built-in, then
[theme.custom]; an unset diagnostic tint or active-tab colour keeps the
value derived by that built-in. Invalid names, colours, or unknown keys fall
back safely and produce one dismissible startup warning. Run `rustrace doctor`
to see the effective theme, every palette value, and whether each value came
from the built-in or a custom override. The full field reference is in
[`docs/configuration.md`](configuration.md).

The [recommended terminals](#recommended-terminals) deliver unreserved Command
chords through the required protocol. Terminal-owned chords never reach
Rustrace unless they are unbound; use the Control aliases or the menu.

To confirm the binary, run `rustrace --version`. With `--verbose` it prints
six lines: the version, the build commit, the event format, the package
format, the assignment format, and the target triple.

## Run doctor

Before your first session, run one command against the assignment package
your course gave you:

```console
rustrace doctor assignment.rta
```

Add `--workspace DIR` if you plan to keep the assignment somewhere other than
the default location. Each check prints `OK`, `WARNING`, or `BLOCKER` with a
one-line remedy for anything that is not OK, then a summary line. The exit code
is 0 when everything is OK or only warnings remain, and 1 for any blocker.
`doctor` prints fourteen checks, in this order: terminal size and
capabilities; the effective theme and palette sources; a writable workspace; enough disk space; rustup; the
assignment's pinned toolchain; Cargo; rustfmt; Clippy; rust-analyzer; the
integrity of the starter `.rta` package; whether an existing workspace is
already owned by a live session; the filesystem capabilities Rustrace needs
at the workspace location, meaning safe extraction, atomic save, rename, and
remove, exclusive file creation, and cooperative locking; and cumulative
storage headroom, meaning whether the full 2048 MiB session recording budget
fits on that filesystem. A missing rust-analyzer is a warning, never a blocker.

On macOS under Ghostty, `doctor` also prints an informational key-setup
section. It gets the active bindings from `ghostty +list-keybinds`; a missing
or failed Ghostty CLI makes the section use the known default binding set.
This section does not add a warning or blocker and does not change the exit
code.

The older `rustrace environment` command still exists and probes only the
tool versions.

## Start an assignment

```console
rustrace work assignment.rta
```

Rustrace validates the package, extracts the starter into a workspace directory
named after the package with a `.work` suffix (`assignment.work` next to
`assignment.rta`), and opens the editor. Use `--workspace DIR` to choose a
different directory; keep it on a local filesystem, not a cloud-synced or
network folder.
Assignments work from any directory because each is its own Cargo workspace root.

Version 2 assignment packages include a nonempty `test-cases/` directory.
Each case is a complete pair named `test-cases/NAME.in` and
`test-cases/NAME.expected`. `NAME` is 1 to 64 ASCII bytes and may contain only
letters, digits, `-`, and `_`. A package may contain at most 256 cases, with a
limit of 1 MiB per case file and 10 MiB across all case files. The `.rta` file
itself must be no larger than 32 MiB. Version 1 packages remain accepted
unchanged and do not contain packaged test cases.

For a version 2 package, Rustrace places the cases next to the workspace as
`WORKSPACE_PARENT/test-cases/`; it does not put them inside `assignment.work`.
Rustrace checks every packaged case path before publishing a fresh workspace.
It never replaces a different file, symlink, directory, or other special entry.
If the sibling directory already contains the exact packaged bytes, startup
accepts them. Resume recreates missing packaged files, accepts byte-identical
files, and preserves unrelated files such as `NAME.actual`. `--inspect` and
`doctor` never write the sibling directory.

Before the editor appears, Rustrace prints the toolchain it found and each
tool's version. A missing required tool stops startup with the assignment
preserved; fix the tool and run the same command again. A missing rust-analyzer
prints a warning that completion is unavailable and continues.

The last startup line names your session. Rustrace records your source and
editor history and the commands you run. See the [privacy notice](#privacy-notice)
for the exact recorded and excluded categories.

## Resume an assignment

Run the same command against the same workspace:

```console
rustrace work assignment.rta --workspace assignment.work
```

Rustrace automatically resumes a valid unfinished session without asking for a
choice. Resume validates the whole recorded history, restores your open files,
and continues the same attempt. Undo history starts empty after a restart. If a
file on disk changed while Rustrace was closed, resume restores the recorded
contents and records what it observed; your work is not lost, but the outside
change is not adopted.

Explicit recovery controls remain available: `--resume` selects the same path
as automatic resume, `--inspect` shows the preserved views, `--abandon` starts a
linked fresh workspace, and `--restore-logical` is an older alias for
`--resume`. You cannot resume a finalized attempt; Rustrace tells you to use
`rustrace revise` instead. If the package you pass does not match the one that
created the workspace, startup stops and nothing changes.

## Use the editor

The workspace view has a ` files` sidebar on the left, open-file tabs and the
borderless editor on the right, then a faint horizontal divider and the output
pane. The horizontal divider uses the accent color while the lower pane has
focus. The tab bar's right edge shows only the cursor's line and column; the
assignment title is not displayed in the student shell. The bottom row is
always reserved so opening a mode, error, or console never moves the panes. At
idle it shows the dim hint `F1 keybinds · F7 menu · F9 console`; an active mode
or error replaces that hint with its controls. This row is not a status line:
short results and warnings appear as a toast for 10 seconds or until your next
key press, whichever comes first. Press F1 for the curated non-obvious keybind
reference.

Right-click a file row to open, rename, delete, or create a file. Right-click
the ` files` header or empty space to open a menu with only `new file…` enabled.
Disabled entries are dim and ignore clicks. Opening a file is silent; the active
tab and caret show the change. Deleting a dirty file uses the existing
confirmation. A second right-click, a left-click outside, or Esc closes the
menu. The files panel is mouse-only and has no keyboard focus mode.

Only files matching the assignment's allowed paths can be created or edited.
`Cargo.lock` is controller-managed, so it is hidden from the files list and tabs
and cannot be activated there. It still remains part of dependency provenance,
the workspace hash, and a submission.

The sidebar ` new` label opens the same new-file panel. The ` + ` tab and the
file menu's `new file…` entry also open the centered `new file` panel. Its dim
`src/` prefix is fixed: type only the remainder, such as `foo.rs` or
`util/mod.rs`, to create `src/foo.rs` or `src/util/mod.rs`. Backspace in an empty
field cannot remove the prefix. The file menu's `rename…` entry opens a
`rename file` panel with the current path prefilled and the caret at its end.
Type or erase the single-line field, then press Enter or click `↵ create`/`↵ rename`;
press Esc or click `esc cancel` to close it. Panels receive keyboard input ahead
of the editor and console; closing one returns to the editor.

Click the dim `find` label in the ` files` header, or press ⌘F or ctrl-F, to
open the centered find and replace panel. A one-line editor selection prefills
the find field. Tab switches between the find and replace fields. Matching is
literal and case-sensitive across the whole document. The counter is blank for
an empty find, says `no matches` when none exist, and otherwise shows `n of m`.
Enter in either field or `↵ next` selects the next match after the caret and
wraps at the end. `replace` changes the current match and advances as one
keyboard edit. `replace all` changes every match as one transaction and one
undo step. Esc, `esc close`, or a click outside closes the panel without moving
the editor caret or selection. F3 repeats the last find while the panel is closed.

Ctrl-S saves every open buffer and then starts Check after the save completes;
when Command is effective, ⌘S does the same and ctrl-S remains an alias.
One successful explicit save shows exactly one `File saved` message. The Check
it starts does not add a completion message. A successful command started from
the menu or console also adds no completion message. Command preparation, tool
resolution, execution, evidence saving, and owned-process cleanup do not raise
notices; their state remains visible in the mode bar or active output/console
pane. Command outcomes do not raise notices: nonzero exits, launch failures,
cancellations, deadlines, terminations, and stopped console commands are state
rather than notices. The ERROR pill and output or console pane keep showing
failed command results. Cleanup problems, save failures, and Check-start
failures still raise notices.
The save-triggered Check is recorded exactly like a manual Check from the
command menu. Autosave never starts Check. If another command already owns the
runner, the Check is skipped and a warning appears for up to 10 seconds or
until your next key press. Shift with the arrow
keys selects text; Tab and Shift-Tab indent and outdent the selection. Alt-Up
and Alt-Down jump through the document-ordered combination of live hints and
compiler diagnostics from the latest Check, Test, or Clippy run.

Enter keeps the current line's leading spaces or tabs. After `{`, `(`, or `[`,
it adds one level: four spaces, or one tab on a tab-indented line. Between a
matching pair, Enter also puts the closer on its own line at the original
indentation. Closing `}`, `)`, or `]` on an indentation-only line moves back
one level to the matching opening line. Replacing a selection with Enter still
inserts one newline without automatic indentation.

Typing an opener inserts `()`, `[]`, `{}`, or `""` and leaves the caret inside
when the next character is the line end, whitespace, or another closer. A
quote directly after an identifier character stays single. Inside ordinary
Rust strings, including escaped quotes and multiline LF or CRLF text, `"` stays
single. It also stays single inside `r"..."` and `r#"..."#` raw strings, including
raw strings with additional `#` delimiters. Type an automatically inserted
closer to move over it, or press Backspace inside an empty automatic pair to
remove both. Another edit or caret movement clears that temporary pair
behavior. Ctrl-/ toggles `// ` on the current or selected lines as one undo
step. Each Enter, closer, pair insertion, pair deletion, or comment toggle that
changes bytes is one recorded Keyboard edit and one undo step.

When the caret is on or immediately after a bracket, its match uses the accent
color. Bracket matching does not distinguish brackets inside strings or
comments. Tab and Enter still accept an open completion popup; Esc closes it
first.

Mouse capture is always on; there is no toggle or startup flag. Click to place
the caret, drag or Shift-click to extend a selection, and double-click a word
to select it. The wheel scrolls three lines in the pane under the pointer, and
the editor scrollbar supports track clicks and thumb dragging. While a command
runs, the wheel still scrolls the output and console panes. Left-drag the
horizontal divider to resize the lower pane, also while a command runs. The
editor keeps at least eight rows and the lower pane at least four; the chosen
height is shared by the output and console panes until Rustrace restarts, when
the default height returns.
Left-drag the sidebar's vertical edge to resize it from 18 through 36 columns;
the editor, tabs, scrollbar, output, and console reflow immediately. Its chosen
width also resets when Rustrace restarts. The active divider uses the accent
color. Resizing is not recorded. File rows, tabs, `new`, `menu`, command entries,
diagnostics, and confirmation pills are clickable. Only a left-button down
activates a pill; release and right or middle clicks do not. While the command
menu is open, a left-button down outside it closes the menu without activating
the item underneath. A completion popup is different: a left-button down
outside it closes the popup and then performs the normal click action
underneath. `Up`, release, and drag never use either dismissal route. The
[recommended terminals](#recommended-terminals) let you select terminal text
outside Rustrace by holding Shift while dragging, or Option in iTerm2. With
tmux, configure `set -g mouse on` so mouse reports reach Rustrace.

### Copy, cut, and paste

Use Ctrl-C, Ctrl-X and Ctrl-V for Rustrace's internal clipboard. Ctrl-C copies
and Ctrl-X cuts the selection; Ctrl-V pastes it, including into a different
file. ⌘C, ⌘X and ⌘V work only when the terminal delivers them. macOS
terminals capture ⌘C and ⌘V for the system clipboard.

After a successful copy or cut, Rustrace sends the exact selection to the
system clipboard through a one-way OSC 52 write. If the terminal honours that
write, ⌘V works after copying inside Rustrace. Rustrace accepts the bracketed
paste only when it matches the live internal clipboard, allowing CRLF or lone
CR line endings to match an LF source. It inserts the recorded source bytes and
records the same linked internal-paste event as ctrl-V; it never inserts the
delivered terminal bytes.

Terminal.app ignores the mirror, so ctrl-V is the fallback there and ⌘V stays
blocked. The [recommended terminals](#recommended-terminals) support both the
Command shortcuts and the mirror.

Right-click the source editor to open Cut, Copy and Paste. Cut and Copy are
disabled without a selection; Paste is disabled when Rustrace's internal
clipboard is empty. The menu and the keys use the same internal clipboard
commands and record the same events. Cut and Copy from the menu also mirror the
selection to the system clipboard.

The internal clipboard works because Rustrace knows exactly where the text came
from: the copy records the source file, version, and byte range, and the paste
is recorded as one edit linked to that source. You can paste after editing or
even deleting the source; the copied bytes stay valid until you quit, restart,
finalize, or start a revision.

Text copied elsewhere does not match the live internal clipboard and stays
blocked. A differing bracketed paste, or any bracketed paste after the internal
clipboard expires, produces this warning:

```text
Paste blocked: only text copied or cut inside this recorded workspace is allowed. Use Ctrl-C, Ctrl-X and Ctrl-V. ⌘C, ⌘X and ⌘V work only when the terminal delivers them; macOS terminals capture ⌘C and ⌘V for the system clipboard.
```

The blocked attempt is recorded as a short note giving only the reason and the
input channel; the rejected text is never stored, hashed, measured, or shown to
anyone. External paste is blocked in both find and replace fields, file prompts,
the console, and the test-case picker. Typing the text by hand is recorded like
any other typing. Rustrace writes only text that you copy or cut inside the
editor and never reads your system clipboard.

### Completion

After you pause for 200 ms following an identifier character, `_`, `.`, or
`::`, Rustrace asks rust-analyzer for completion automatically. You can also
request it immediately with Ctrl-Space. If rust-analyzer returns choices, a
popup appears beside the cursor. Up and Down move through it, Tab or Enter
accepts the selected item, and Esc closes it. Typing an identifier character
inserts it normally, hides the old popup, and starts a fresh request after the
next pause. Any other key closes the popup and is then processed normally.
Rustrace does not filter or reuse stale choices on the client.

Each automatic or Ctrl-Space request records the same bounded request event.
Accepting a completion records an acceptance event and the resulting edit;
showing, navigating, or dismissing the popup records nothing.

If rust-analyzer is not installed or has failed, Ctrl-Space reports
`completion unavailable` in a toast and nothing else changes. F8 asks the
service to restart. Editing, recording, Cargo commands, and submission never
depend on the language service. Automatic requests remain silent when the
server is unavailable, fails, times out, or returns no choices.

### Live diagnostics

Only Rust (`.rs`) documents receive live hints. Rustrace does not open non-Rust
documents such as `Cargo.toml` with rust-analyzer, and it drops any live
diagnostic the server sends for them.

When rust-analyzer reports a problem for the exact version you are viewing,
Rustrace underlines the affected source and may put a shortened message after
the code when space permits. The full safe message appears in the output header
while your caret is on the affected line. The row otherwise reads only
`output`; on an affected line it reads `output · message`, with the message
safely shortened to the pane width.
Fixing the source or moving the caret away removes it. Delayed hints for older
text are ignored. If rust-analyzer is missing, stops, or sends an invalid
message, the source stays quiet.

Live hints from rust-analyzer are display only and are never recorded. They do
not appear as output rows or in submission evidence. Cargo diagnostics created
by a controlled Check, Test, Run, or Clippy command remain part of that
command's recorded evidence. Failure-note diagnostics and spanless Cargo
note/help boilerplate stay out of the output pane. Spanned diagnostics remain
visible, and the recorded command evidence is unchanged. The ERROR pill counts
errors and warnings only, never failure-notes. Compiler errors tint the full
source line red and warnings tint it yellow; errors win when both severities
occur on one line.
The tint adapts to the selected colour palette, keeps source colours readable,
and extends through the empty cells after the code. Click a tinted line to keep
the caret at that source position and reveal its accent-marked diagnostic row.
Alt-Up and Alt-Down navigate both kinds and reveal the selected compiler row,
while clicking a diagnostic output row continues to open its Cargo source span.
Successful navigation is silent because the tab and caret show the change;
failures still appear as a toast.

## Run Cargo commands

Press F7 to open the in-place menu above the sidebar's `menu` label. The command
menu contains Check, Run, Clippy, Format, Doc, Update dependencies, Update
Rustrace, Automatic checks: On/Off, Console, Test cases, Keybinds, and Quit.
Left and Right or Up and Down choose, Enter activates, and Esc closes.

| Menu row | Action |
| --- | --- |
| Check | Check saved files |
| Run | Run the program |
| Clippy | Run Clippy |
| Format | Format files |
| Doc | Build documentation |
| Update dependencies | Update Cargo dependencies as recorded DependencyTool edits |
| Update Rustrace | Show cached version information and terminal instructions |
| Automatic checks: On/Off | Persist the automatic check preference for the next launch |
| Console | Open the console |
| Test cases | Open the test case picker |
| Keybinds | Open the keybinds overlay |
| Quit | Follow the unsaved-changes confirmation path |

The Quit entry has no shortcut suffix. Console opens the F9 pane;
Test cases and Keybinds open the same modals as F4 and F1. Quit follows the same
unsaved-changes confirmation path as ⌘Q or ctrl-Q. While a command runs the
editor is read-only and Esc cancels it.
Output and parsed diagnostics appear in the lower pane; use Alt-Up and Alt-Down
to visit the diagnostics in your source. Output rows have no `stdout:` or
`stderr:` text prefix: stdout uses the normal text color and stderr uses red as
a stream cue. The command result, not the row color alone, determines whether
the action succeeded.

Choosing Check, Run, Clippy, Format, Doc, or Update dependencies while the
console is showing switches back to the output pane and returns keyboard focus
to the editor. If a console command already owns the runner, the new action is
skipped with the existing busy toast, but the view still switches. F9 reopens
the console with its prior output, input, and resized height intact.

Compiling commands use your saved files and may download dependencies, but
`--locked` makes them fail instead of changing `Cargo.lock`. Downloaded
dependencies and their build scripts execute on your machine under the retained
Cargo configuration. Run here has closed standard input, so use the console for
programs that read input. Format runs rustfmt on a copy of your files. Manage
dependencies by typing `cargo add` or `cargo remove` in the console. Those
commands and the menu's Update dependencies action run Cargo on a copy and apply
only the resulting `Cargo.toml` and `Cargo.lock` changes as recorded edits. Doc
writes under the workspace `target/doc` directory and never opens a browser.
Every command, including a Check started by Ctrl-S, its arguments, exit status,
output, and accepted source edits are recorded with your history.
Registry/network traffic and credentials are not.

## Cargo console, stdin, and test cases

Press F9 for the focusable console pane, which replaces the diagnostics pane.
Esc cancels any running console command, closes the console, restores the
diagnostics pane, and returns focus to the editor. Its inactive prompt is `> `.
It is not a shell. Type one of these lines and press Enter:

```text
cargo build
cargo check
cargo test [FILTER] [-- OUTPUT_OPTION]
cargo clippy
cargo doc
cargo add NAME
cargo add NAME@VERSION
cargo remove NAME
cargo update
cargo run
cargo run --release
cargo run < input.txt
cargo run > output.txt
cargo run --release < input.txt > output.txt
```

For `cargo test`, brackets mark optional groups; do not type the brackets.
`OUTPUT_OPTION` is exactly one of `--nocapture`, `--no-capture`, or
`--show-output`. These are the allowed forms:

```text
cargo test
cargo test FILTER
cargo test -- --nocapture
cargo test -- --no-capture
cargo test -- --show-output
cargo test FILTER -- --nocapture
cargo test FILTER -- --no-capture
cargo test FILTER -- --show-output
```

Replace `FILTER` with one literal token of 1–256 ASCII bytes from
`[A-Za-z0-9_:-]`, without a leading `-`. For example, `legal_moves`,
`tests::legal_moves`, and `tests::` are valid. A filter matches a substring of
the full Rust test name, including its module path. Even a full-looking name
can match several tests; exact matching with `--exact` is not supported here.
A valid filter can match zero tests, so check the reported test count: a
successful command alone does not mean any tests ran.

The filter must precede the literal `--` separator. An output option needs
that separator and must be last. Multiple filters, multiple output options,
a trailing `--` alone, and other Cargo or test-harness flags are rejected.
For example, `cargo test --nocapture`, `cargo test one two`,
`cargo test -- --exact`, and `cargo test --release` are not allowed.

By default, Rust's test harness captures test prints and shows them for failing
tests. `--no-capture` lets tests print while running; parallel tests can
interleave their output. `--nocapture` is the deprecated alias for the same
behavior and remains accepted. `--show-output` keeps harness capture enabled
and shows successful-test output after all tests finish, grouped by test. See
the official [test-harness output options](https://doc.rust-lang.org/rustc/tests/index.html#output-options).
These options do not disable Rustrace recording: output emitted by the process
is still recorded under the same limits described below. Prints retained only
inside the test harness are not available to Rustrace.

These options apply only to typed console Test commands. Bare `cargo test` and
the menu Test action keep their existing behavior and assignment policy.
Rust unit tests run by `cargo test` are separate from the packaged input/output
cases in the F4 picker, which runs your program with `cargo run`.

Use the course-provided Rustrace update for these forms. Staff must update
grader, verifier, and replay installations before distributing that student
update: older readers reject recordings containing the new Test arguments.
Existing `.rta` packages need no changes, and updated readers still accept
older recordings.

Redirections are allowed only for `cargo run`. Their paths are relative to a
directory named `test-cases` that sits next to your workspace directory, and
each may appear once. Quotes, pipes, wildcards, and other shell characters are
rejected. A line is limited to 4096 bytes, including surrounding spaces.
Leading, trailing, and repeated ASCII spaces are allowed; tabs and newlines
are rejected. If the output file already exists, the console asks before
overwriting: Y or Enter overwrites, N or Esc cancels and leaves the file alone.

When `cargo run` starts without an input redirect, the prompt line becomes the
program's standard input. Type a line and press Enter to send it; the console
shows the line after your program's prompt, like a terminal does. Ctrl-C stops
a running console command and keeps the console open; Esc stops it and closes
the console. Both work while the program is waiting for input or stuck in a
loop, and the mode bar reads `ctrl-c stop  esc stop+close  ↵ send` while a
program runs. There is no separate EOF shortcut; stdin closes through the
cancellation or command-exit path. The lines you type are not recorded as
evidence, although your program's output, which may echo them, is captured
like any other output. Console commands print Cargo's normal output flush-left
and program output verbatim. No console action requests Cargo JSON. The console
recognises Cargo status and diagnostic lines in its combined stdout/stderr view.
Cargo status lines render flush-left; diagnostic blocks are dedented by their
minimum indentation to keep source gutters and carets aligned. Other lines keep
their indentation, and all recorded bytes remain unchanged. The mode bar reads only
`esc close  ↵ run/send` in both modifier modes. The console follows the newest
output and wraps long lines at the pane width. PgUp and PgDn, or the mouse
wheel, scroll back through older output, including while a command runs; the
console header then shows how many lines lie below, and scrolling back to the
bottom follows new output again. Each new command starts at its newest line.
The view stays on the same output line while new output arrives or the pane is
resized. Scrollback holds the last 128 KiB of the live output; older lines are
replaced by `[older console output omitted]`, and a line longer than 4 KiB
shows its newest part after `[line start omitted]`. The console takes about
two fifths of the terminal height by default, and its divider can be dragged
while a command runs. Recorded stdout and stderr share an 8 MiB per-command cap
and a 64 MiB session budget; deployments may set lower budgets,
and a command can use only the remaining session allowance. Reaching the output
limit stops the command. The existing deadline (at most five minutes) and Esc
or Ctrl-C cancellation also apply to filtered tests and every output option. Esc returns
to the workspace view.

F4 or the Test cases menu entry opens a modal test-case picker. It lists complete
`.in`/`.expected` paired cases in bytewise name order and ends with a `Run all`
row. Each case shows `—`, `PASS`, `FAIL line N`, or `ERROR` for its last result
in this session. Up and Down select with wrapping; mouse hover or a single click
also selects. Enter or a double-click runs the selection, R refreshes pairs from
the sibling directory, and Esc closes the picker. A version 1 assignment with no
sibling directory says that no packaged test cases are available; missing
packaged data for a version 2 assignment is reported as unavailable.

The picker closes before a run starts and reopens after the run or queue
finishes. The output pane shows the controlled command and then one test-case
result line, retaining each result line as a Run all queue advances. A later
non-test command takes ownership of the output pane. Run all runs every listed
case serially in that order through the single runner; it never starts cases in
parallel. Each case may run for at most 10 seconds, including the `cargo run`
build, so a program that hangs fails that case with a deadline ERROR and Run
all moves on. During a run the mode bar reads `esc/ctrl-c cancel`: Esc or
Ctrl-C cancels the active run and the rest of the queue; F1, F7, and F9 wait
until that sequence finishes. Menu commands show the same hint while they run. If the picker was opened over
the Console view, closing its final reopened modal restores that view. A
selected case uses the same prepared, policy-checked, limited, and recorded
Cargo action as typing `cargo run < NAME.in` in the console, apart from its
shorter deadline. Every completed
picker run records one `test_case_compared` event immediately after its
controlled command finishes. That event retains the command ID, case name,
expected BLAKE3 digest, optional actual BLAKE3 digest, and typed result.
Mismatch details retain the positive one-based line and expected and actual
line-byte lengths; errors retain one fixed classification such as nonzero exit,
termination, or incomplete capture. No raw test input, expected output, or
actual output bytes are added by the comparison event. Actual program output
remains in the existing bounded controlled-command output records.
The recorded line is at most 1,048,577; expected line lengths are at most 1 MiB
and actual line lengths at most 8 MiB. Pre-launch input or expected-file errors
show ERROR without creating a completed-run comparison record.
The line number and expected line length must together fit the 1 MiB
expected-file limit.

A case is PASS only when the complete captured stdout bytes exactly equal the
`.expected` bytes. A difference reports the first differing LF-delimited line,
the expected and actual byte lengths without the LF, and bounded terminal-safe
previews of both lines. An expected or actual early end is a difference on that
line; CR bytes and invalid UTF-8 are content. Any launch, exit, termination,
capture, or expected-file problem is ERROR, including nonzero exit, cancellation,
deadline or output-limit termination, incomplete capture, and unreadable or
oversized expected output. Input and bounded expected bytes are opened before
launch, so a replacement during execution cannot change that run's comparison.
Comparison never uses the bounded live output tail.

These files are outside the source workspace: their mutable live copies are
never included in workspace hashes, checkpoints, or the submitted source tree.
For a version 2 package, provenance retains one hash of the validated packaged
suite plus the per-run comparison fields above, so an instructor reference can
be checked without trusting the mutable sibling. Editing the live copies
happens outside Rustrace.

Pasting into the console or test-case picker is blocked and recorded the same
way as in the editor. The record shows only what happened inside Rustrace; it
cannot show how files you produced elsewhere were made.

## Use an external debugger

Course policy permits a separate debugger on artifacts built by Rustrace.
Rustrace has no integrated debugger. Keep all source edits inside Rustrace.

Opening an `.rta` extracts the assignment into a workspace such as
`assignment.work`; the package itself is not an executable. Save your source,
then use `cargo build` in the F9 console to build an assignment's ordinary
binary. Wait for the build to finish successfully. With the usual native debug
configuration, the binary is `assignment.work/target/debug/NAME`, where `NAME`
is the assignment's binary target name; use the actual artifact path if the
assignment's Cargo configuration changes that layout. See the official
[`cargo build` documentation](https://doc.rust-lang.org/cargo/commands/cargo-build.html).

`cargo check` does not produce an executable. `cargo test` builds distinct test
harness executables, usually under `target/debug/deps`; do not assume it also
leaves the ordinary program binary you want to debug. To debug a unit test,
use the test executable identified in the completed Rustrace test run's Cargo
output. See the official [`cargo check`](https://doc.rust-lang.org/cargo/commands/cargo-check.html)
and [`cargo test`](https://doc.rust-lang.org/cargo/commands/cargo-test.html) documentation.

Wait for any active Rustrace command to finish before opening the existing
artifact in your separate debugger. Disable any automatic build step in the
debugger, and do not run Rustrace build, check, test, or run commands while
debugging. Stop the debugger and its running program before saving source,
rebuilding, or submitting: saving can start a Check. Make source changes in
Rustrace and finish the next build before starting another debugging session.

Rustrace does not record external debugger commands or output. Running the
program under a debugger can still write files. Rustrace may later observe
assignment-file changes and record the file evidence, but it cannot reconstruct
the external debugging session. See [what is not recorded](privacy.md#what-is-not-recorded).

## Run rustrace submit

Quit the editor with the menu, ⌘Q when the terminal delivers it, or the reliable
ctrl-Q alias, then finalize and package the attempt:

```console
rustrace submit assignment.work --student-id YOUR_ID
```

The student ID must use only ASCII letters, digits, `-`, `_`, and `.`. Use the
identifier your course tells you to use; every later command for this attempt
must repeat the same value. Finalizing appends a closing event to your history
and makes the attempt immutable. Rustrace then writes one ZIP, by default
`YOUR_ID-ASSIGNMENT.zip` in the directory that contains the workspace, or the
path you give with `--output`. The command prints the file's BLAKE3 hash, its
path, and this reminder:

```text
local artifact only; this does not mean a successful LMS hand-in
```

`submit` never overwrites a ZIP. The default name depends only on your ID and
the assignment, so attempts using the same student ID and assignment in the
same folder resolve to the same default file. A linked revision using that ID cannot use it
once you have already submitted. If the default file still exists, `submit`
stops with `bundle destination already exists and was not replaced` and exit
code 1, and writes nothing. Running `submit` again for the same attempt adds
nothing to your history. To rebuild, move or delete the old ZIP first, or pass
a new path with `--output`; the rebuilt ZIP has the same BLAKE3 hash, and
`rustrace status` lists every local export. Once finalized, `rustrace work`
refuses to reopen the workspace and points you to `rustrace revise`.

Run `rustrace privacy assignment.work` first if you want to see exactly what
the bundle contains: identities, attempts, counts of every event kind, and byte
totals for source, commands, output, and evidence. For an unfinished attempt it
is labelled a preview.

## Upload the ZIP to the LMS

Upload the ZIP through the course LMS as you would any other file, then
confirm the hand-in there. Rustrace cannot see the LMS and records no upload
time; the timestamps in `rustrace status` describe only when the local file was
written.

## Revise in a new linked attempt

To keep working after finalizing, start a linked attempt in a new directory:

```console
rustrace revise assignment.work assignment-v2.work assignment.rta
rustrace work assignment.rta --workspace assignment-v2.work --resume
```

`revise` requires a finalized parent, a directory that does not yet exist, and
the same `.rta` package. It copies the parent's final files into the new
workspace and records a link to the parent. The parent stays exactly as it was
and can never accept later work; the recorded history of a finalized attempt is
immutable so that every ZIP built from it verifies the same way. Your new edits
belong to the child attempt.

## Prepare the revised ZIP

Finalize and package the child with your student ID, and give the new ZIP
its own name with `--output`:

```console
rustrace submit assignment-v2.work --student-id YOUR_ID --output YOUR_ID-ASSIGNMENT-v2.zip
```

When reusing the same student ID, the `--output` flag is required here in
practice. Without it, the child would
use the same default filename as the parent's ZIP, `submit` would stop with
`bundle destination already exists and was not replaced` and exit code 1, and
no revised ZIP would be written. Any name you choose is fine; the revised ZIP
carries the complete history from the original starter, so its name does not
affect grading.

Upload the new ZIP to the LMS and confirm the hand-in there. Revise again from
the child for a third attempt, and so on, giving each attempt's ZIP a new name.

## Correct a student ID after finalizing

If you submitted a placeholder or mistyped ID, create a new linked attempt
from the finalized workspace, then submit it with your actual ID. No source
edits are necessary. For example, replace `actual_utorid` below with your UTORid:

```console
rustrace revise lab1.work lab1-corrected.work lab1.rta
rustrace submit lab1-corrected.work --student-id actual_utorid --output actual_utorid-lab1-corrected.zip
rustrace verify actual_utorid-lab1-corrected.zip
```

The new ZIP records the corrected ID and includes the complete recorded history.
The original workspace, receipt, and ZIP retain their original ID. You cannot
change the ID by resubmitting an already finalized workspace or by renaming its
ZIP; renaming changes only the filename. Upload the corrected ZIP to the LMS.

Older versions reject this operation with `revision student identifier differs
from its parent receipt`. Install a version containing the student-ID correction
fix first. If you already encountered that error, keep the failed revision and
create a fresh revision from the original finalized workspace using a new,
nonexistent directory, as above. The failed revision is in incomplete recovery
state; retrying it or using `--allow-incomplete` does not produce a clean corrected
submission. If you made source edits in that failed revision, preserve them and
reapply them in the new attempt before submitting.

## Keep required local history

Each ZIP you build contains the complete history from the original starter
through the current attempt, including every earlier attempt's events, deleted
code, and command output. That is why the newest ZIP is enough for grading and
your TA never needs an older one.

Building the child's ZIP reads the parent's recorded data from the parent's
`.rustrace` directory. Until the term ends, keep every attempt's workspace in
place and do not move, rename, or delete an earlier workspace. `rustrace status`
lists the ordered ancestry for any workspace, and `rustrace cleanup` with
`--destroy-provenance` names the later attempts that would lose the ability to
build a clean ZIP. Rustrace looks for linked attempts only among sibling
directories, so keep all attempts for one assignment in the same folder.

## Recover from interruption

If your machine crashes or the terminal closes, nothing recorded is lost.
Start with:

```console
rustrace status assignment.work
```

It reports one of four states: `UNFINISHED` (reopen with `rustrace work`),
`FINALIZATION PREPARED` (rerun `rustrace submit` to complete it),
`INCOMPLETE RECOVERY` (see below), or `FINALIZED IMMUTABLE SNAPSHOT` with the
receipt details and local export records.

`rustrace work assignment.rta --workspace assignment.work --inspect` prints,
without changing anything, the last saved contents of each file, the last
recorded logical contents including unsaved edits, and what is on disk now.
`--resume` continues the attempt; if it cannot validate the history it stops,
says why, and keeps your code and history intact. `--abandon` is a last resort:
it leaves the original workspace untouched, extracts a fresh starter (not your
current files) into a sibling directory named like
`assignment.work.recovery-<number>-<number>`, and starts a new session linked
to the preserved original. If you abandon by mistake, resuming the original
takes it back as long as the recovery copy has recorded no work (no edits,
saves, file changes, or commands); the unused copy is then closed. Once the
copy has recorded work, the original stays closed and resume names the copy
to continue in. Startup that fails because a required tool is
missing also preserves everything; fix the tool and run the same work command
again. `--resume` remains an explicit equivalent.

If the terminal window closes, or Rustrace is asked to stop, while a command
runs, Rustrace stops that command and its programs, records the stop, and
exits; resume as usual. If Rustrace itself is killed outright (for example
with `kill -9` or a crash), your program can keep running. Every command's
programs hold the workspace lock, so resume then stops with a message naming
`.rustrace/writer.lock`: stop the leftover program (`lsof` on that path lists
it, or quit it in Activity Monitor) and resume again. Rustrace then records the
interrupted command as stopped, without the output it lost, and continues.

If finalization itself was interrupted, `submit` refuses to build a normal
bundle and tells you to rerun with `--allow-incomplete`. That produces a ZIP
clearly marked `INCOMPLETE RECOVERY EXPORT` which does not pass clean
verification. Upload it only if your course staff ask for it, and tell them what
happened.

## Privacy notice

The following is the student privacy notice from Rustrace's privacy document,
reproduced as written.

Rustrace records source and editor history, commands and their output,
diagnostics, tool metadata, and bounded disk-change evidence generated or
observed while you work in the assignment TUI. Source history includes code that
you later edit or delete. A revised submission includes every recorded attempt
back to the original starter, including prior deleted code, tool output, and
required recovery evidence. Blocked clipboard attempts retain only a bounded
reason and input-channel label, never the rejected content. When you copy or cut
inside Rustrace, it sends that already-recorded selection to your terminal as a
one-way system clipboard write. Rustrace never reads the system clipboard or
records other operating-system clipboard contents. It does not record webcam or
screen images, activity in other applications, or keystroke-timing biometrics.
Pointer activity, click coordinates, buttons, click counts, and scrolling are
not recorded. A mouse selection records only the same resulting document byte
range that keyboard selection records.

Cargo commands may contact crates.io or other endpoints selected by retained
Cargo configuration. Downloaded dependencies and their build scripts execute on
the student's machine under the retained Cargo configuration and may have local
side effects. Rustrace does not record registry requests, responses,
credentials, downloaded cache contents, or general network activity. It does
record the command/output evidence and the exact accepted `Cargo.toml` and
`Cargo.lock` edits produced by console `cargo add` and `cargo remove` commands
and the Update dependencies action.

The data is used for grading only. There is no research use of the recorded data.
Only the course's TAs and the instructor may review the data.
All recorded data is permanently deleted after the term's grades are released.

There is no current research collection. Any future research use would require
a separate, new consent process.

Rustrace has no server and does not upload a bundle. The student prepares a
local bundle and manually uploads it through the course LMS. The access and
deletion statements above are course policy; the Rustrace client cannot enforce
or verify copies, backups, or deletion on staff machines or in the LMS. The
bundle is not encrypted and can contain sensitive recorded source and activity;
institutional and local controls must protect its storage, transfer, access,
retention, and deletion.

The full document, including the list of every recorded event kind, is
[privacy.md](privacy.md).

## Clean up after grades are released

While the term is running, the safe command is:

```console
rustrace cleanup assignment.work --confirm
```

Without `--confirm` it only prints what it would do. It removes the workspace's
`target` build directory and stale temporary files from an interrupted
`submit`, and it preserves your source and all recorded data.

After the course releases grades, and only if you no longer need any attempt
of the assignment, you may remove the recorded data too:

```console
rustrace cleanup assignment.work --destroy-provenance --confirm
```

It first lists the linked attempts that would be affected and refuses when a
linked attempt cannot be identified. Then delete the workspace directories and
the ZIP files yourself. This removes only your local copies; the copies you
uploaded are deleted by course staff under the course policy quoted above.
