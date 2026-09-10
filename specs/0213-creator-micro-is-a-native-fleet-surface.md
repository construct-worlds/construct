# 0213-creator-micro-is-a-native-fleet-surface

Status: accepted
Date: 2026-09-09
Area: tui
Scope: Work Louder Creator Micro 2 devices control and display one live Construct TUI through their vendor HID channel.

## Decision

Construct supports the Creator Micro 2 as an opt-in native fleet surface on
macOS. It opens the vendor HID report pipe non-exclusively over USB or Bluetooth
and never synthesizes desktop keyboard events. The six agent keys correspond to
the first six live top-level user sessions in the TUI's durable list order.

Each assigned key displays that session's state: dim blue is idle, breathing
amber is running, and bright green needs attention. An unassigned key is off.
Pressing an agent key selects that session in the active pane and gives its view
keyboard focus. Action keys and the encoder dispatch the same semantic actions
as Construct's keyboard, mouse, palette, and MIDI inputs.

The integration is disabled until the user opts in. Once enabled, a sleeping or
disconnected wireless device is retried without blocking the TUI and has a
visible disconnected state. Feedback is periodically reasserted so reconnects
and dropped wireless updates self-heal.

## Reason

The device exposes direct key notifications and per-key lighting that can make
fleet state glanceable without keeping a terminal focused. Using the vendor
channel preserves normal keyboard behavior and avoids Accessibility permission.
Opt-in ownership matters because the device's six thread colours are global:
two host integrations writing them concurrently would visibly fight.

List order is already persistent, user-controlled, and visible in Construct.
Reusing it makes the hardware mapping useful immediately without requiring
session-title conventions or a second mapping database.

## Consequences

- Native control currently depends on macOS HID support.
- A TUI must be open; the daemon alone does not own the physical surface.
- The active device layer must map its controls to the firmware's vendor agent,
  action, and encoder keycodes. Those keycodes emit host events instead of
  ordinary keystrokes.
- Archived sessions, subagents, operators, and the minibuffer do not consume one
  of the six fleet keys.
- Construct does not rewrite device firmware or the saved keymap. Keymap setup
  remains an explicit user operation in Work Louder Input and can preserve
  unrelated profiles and layers.
- Disabling the feature prevents subsequent TUIs from opening or lighting the
  device; an already-open TUI releases it when it exits.

## Non-Goals

- Global desktop shortcuts when Construct is closed.
- Firmware flashing, factory reset, or automatic device-keymap replacement.
- Treating the Creator Micro as MIDI.

## Examples

- Reordering a live session from list position 4 to position 1 moves its state
  and selection gesture from agent key 4 to agent key 1.
- A sleeping Bluetooth device shows a hollow `micro` indicator; waking it
  reconnects and restores all six current states.
