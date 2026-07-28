# 0150-web-ui-shares-the-tui-chord-keymap

Status: accepted
Date: 2026-07-26
Area: webui
Scope: The web UI binds the TUI's emacs `C-x` chords with the same spelling and the same meaning, so one set of keystrokes drives both clients.

## Decision

The web UI implements the TUI's `C-x` prefix keymap verbatim. A chord that does something in the TUI does the same thing in the browser, spelled the same way. Rules that must hold:

- **Same spelling, same action.** `C-x 3` splits right in both clients; `C-x r` renames in both. The keymap is a shared contract, not a per-client convenience. When either client's binding table changes, the other must change with it or the divergence is a bug.
- **Only the chord-prefixed bindings are shared *globally*.** The TUI's bare-key bindings depend on a modal list that never holds a text caret. A browser has focusable text everywhere, so claiming bare letters across the page would break typing. The prefix family is the portable subset at global scope.
- **The bare keys live in the session list, scoped to it.** A focused list row is exactly as modal as the TUI's list — no caret, nothing to type into — so the TUI's bare bindings carry their meanings there and only there: next/previous selection, drill into the view, toggle pin, expand and collapse a group, reorder. Scoping is the whole mechanism: the same keystroke typed into a composer must never move the list.
- **Every keyboard-reachable surface is keyboard-escapable.** A chord that opens a dialog must have a keystroke that closes it. Cancel unwinds one layer at a time, innermost first, rather than dismissing the whole stack. Pending tool approvals are excluded: they are a decision an agent is blocked on, and a keystroke that discarded one would leave the session waiting with nothing recorded.
- **Focus cycling reaches every focusable region, including the session list.** The TUI's cycle is list plus every visible window, in positional order. A cycle that visits only panes makes the list unreachable by keyboard the moment a split exists.
- **A chord that toggles in the TUI toggles here.** Bindings whose TUI action opens *and* closes a surface must do both, or the keymap can enter a view it cannot leave.
- **`Ctrl` carries the prefix, never the platform meta key.** The browser's own accelerators (⌘W, ⌘T, ⌘L and their Ctrl equivalents on other platforms) stay with the browser. A client that swallowed them would feel broken in a way a terminal never does.
- **Chords escape a focused child PTY.** A pending chord and its prefix key are claimed before terminal emulation sees them, for the same reason the TUI forwards them: the purpose of these keys is to leave a busy pane, so a child that captured them would trap the user. Every other control key still reaches the child untouched.
- **A half-typed chord is visible.** The prefix is echoed on screen until it completes, times out, or is cancelled. A terminal has a minibuffer to show this; a browser has nowhere, and an invisible prefix makes a mistyped second key look like the whole keymap is dead.
- **A bound chord whose action has no web surface answers rather than doing nothing.** It reports that the binding exists and why it cannot act here. Silence is indistinguishable from a broken keymap and invites the user to retry.
- **An unbound key after a valid prefix is consumed, not passed on.** This matches the TUI and stops a stray second key from reaching a child process.

## Reason

Users move between the TUI and the web UI on the same fleet, often within one task. Muscle memory does not switch with the client. Before this, the web UI had no keyboard layer at all — the only global key handling was a few independent `Escape` listeners — so every action needed the mouse, and the split panes the two clients now share (see [0118-split-layout-is-shared-daemon-state](0118-split-layout-is-shared-daemon-state.md)) were reachable by keyboard in one client and not the other.

Sharing the spelling rather than inventing browser-native equivalents is the deliberate choice. A second, differently-spelled keymap doubles what a user has to remember for no gain, and the earlier argument for diverging — that `C-x` is not a browser idiom — turned out to matter less than cross-client consistency: `C-x` is unclaimed by browsers, so taking it costs nothing the user would otherwise use.

## Consequences

- The two keymap tables must be maintained together. Adding a chord to one client without the other reintroduces exactly the divergence this decision exists to prevent.
- Actions the web UI cannot perform still occupy their chord. They are reserved, not free for reassignment, so the spelling stays stable when the surface eventually exists.
- Claiming `C-x` globally means a full-screen editor running inside a session's PTY cannot receive it. This is accepted and matches the TUI, which makes the same trade for the same reason.
- **`Ctrl+X` is the system cut shortcut on Windows and Linux, and the prefix takes it over there.** This is the real cost of sharing the spelling, and it is accepted for the same reason emacs accepts it: the prefix is worth more than the one editing shortcut it displaces, and cut remains available from the context menu everywhere and on `⌘X` on macOS. If this proves too disruptive, the fix is a keymap-profile setting shared with the TUI — not a web-only divergence in what `C-x` means.
- Pane focus, zoom, and the chord echo are per-client. Chords that move focus or zoom must not write to the shared layout, per [0118-split-layout-is-shared-daemon-state](0118-split-layout-is-shared-daemon-state.md).
- Chords that mutate the shared layout are still subject to the clamp-on-render rule: a viewport too narrow to render panes must not write layout changes, so split and resize chords do nothing there and say so.

## Non-Goals

- Adopting the TUI's vim profile, or offering a keymap-profile switch in the web UI.
- Binding the TUI's bare keys at *global* scope, or adopting `M-x`. Bare keys are adopted only inside the session list, where nothing can be typed into.
- Making the chord set user-configurable. If that is wanted later, it should be one setting shared by both clients rather than a web-only preference.

## Examples

- `C-x 3` then `C-x o` splits the view and moves focus to the new pane, in either client.
- `C-x C-s` while the Program surface is open saves it; the same chord elsewhere reports that Program must be open first.
- `C-x` followed by a key with no binding shows that the chord is unbound and sends nothing to the focused session.
- On a viewport too narrow for panes, `C-x 3` reports that the window is too narrow instead of silently writing a split that other clients would then have to render.
- A chord opens the new-session dialog; cancel closes it. The same cancel key, pressed with a half-typed chord instead, abandons the chord — one key, whichever layer is on top.
- The chord that opens Program, pressed again, returns to the surface it was invoked from.
- With two panes open, repeatedly cycling focus visits both panes and the session list, then returns to the first pane.
- With a list row focused, the next/previous keys move the selection; the same keys typed into the composer insert text and leave the selection alone.
