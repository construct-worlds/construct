# 0213-creator-micro-is-a-native-fleet-surface

Status: accepted
Date: 2026-09-09
Area: tui
Scope: Work Louder Creator Micro 2 devices control and display one live Construct TUI through their vendor HID channel.

## Decision

Construct supports the Creator Micro 2 as an opt-in native fleet surface on
macOS. It opens the vendor HID report pipe non-exclusively over USB or Bluetooth
and never synthesizes desktop keyboard events. The six agent keys track the six
most recently active live top-level user sessions. A session keeps its physical
slot while it remains in that set; a newcomer replaces the least-recent member
in the vacated slot instead of reshuffling every surviving assignment.

Each assigned key displays that session's state: dim blue is idle, breathing
amber is running, and bright green needs attention. An unassigned key is off.
Pressing an agent key selects that session in the active pane and gives its view
keyboard focus. The four middle-row action keys focus split panes 1–4 in their
visible ordinal order. The three bottom-row switches dispatch yes, no, and
enter. The encoder dispatches the same scroll and focus actions as Construct's
keyboard, mouse, and MIDI inputs.

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

Recency keeps the limited hardware surface pointed at sessions that are doing
work or asking for attention. Preserving the physical slot of every session
that remains in the top six avoids turning a rank change into a muscle-memory
change. Split-pane ordinals are already visible and shared across Construct
clients, so the middle row can target them without a second numbering scheme.

## Consequences

- Native control currently depends on macOS HID support.
- A TUI must be open; the daemon alone does not own the physical surface.
- The active device layer must map its controls to the firmware's vendor agent,
  action, and encoder keycodes. Those keycodes emit host events instead of
  ordinary keystrokes.
- Archived sessions, subagents, operators, and the minibuffer do not consume one
  of the six fleet keys.
- Sessions with no recorded event, message, or PTY activity do not consume a
  fleet key.
- The yes and no switches must be independently pressable. A keycap spanning
  both physical switches can actuate contradictory answers in one gesture.
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

- When a seventh session becomes more recent than key 4's session, it inherits
  key 4; the sessions on the other five keys do not move.
- Middle-row key 3 focuses the split pane wearing ordinal badge 3 and reports an
  unassigned key when fewer than three panes exist.
- A sleeping Bluetooth device shows a hollow `micro` indicator; waking it
  reconnects and restores all six current states.
