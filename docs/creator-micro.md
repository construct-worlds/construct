# Work Louder Creator Micro 2

On macOS, a Creator Micro 2 can be a native Construct fleet controller over
USB-C or Bluetooth. Construct receives controls through the board's vendor HID
channel, so the terminal does not need desktop focus and no Accessibility or
Input Monitoring permission is required.

## Set up a layer

In Work Louder Input, dedicate one layer to Construct and assign these vendor
keycodes:

| Physical control | Input keycode | Construct behavior |
|---|---|---|
| Six agent keys | `KV_OAI_AG00` … `KV_OAI_AG05` | Select live sessions 1–6 |
| Play | `KV_OAI_ACT06` | Enter / submit |
| Approve | `KV_OAI_ACT07` | Answer yes |
| Reject | `KV_OAI_ACT08` | Answer no |
| Stop | `KV_OAI_ACT09` | Interrupt the selected session |
| Wide key (both switches) | `KV_OAI_ACT10`, `KV_OAI_ACT11` | New session (coalesced once) |
| Four-dot key | `KV_OAI_ACT12` | Command palette |
| Encoder left/right/click | `KV_OAI_ENC_CC`, `KV_OAI_ENC_CW`, `KV_OAI_ENC_CLK` | Scroll up/down; switch focus |

The `KV_OAI_*` keycodes produce vendor events instead of ordinary keystrokes.
Use a dedicated layer if the controls already hold macros you want to keep.
Construct does not rewrite the device keymap.

## Enable Construct

Wake the board or connect a USB-C data cable, then confirm macOS can see it:

```sh
construct creator-micro devices
```

Enable the integration and open a new TUI:

```sh
construct creator-micro enable
construct
```

`● micro` in the modeline means the vendor channel is connected. `○ micro`
means the feature is enabled and Construct is waiting for the sleeping or
disconnected board. Bluetooth reconnects automatically.

The six agent keys follow the first six non-archived, top-level user sessions in
the visible Construct list order. Reordering the list also reorders the hardware
slots. Subagents, operators, and the minibuffer do not take a key.

Key colours are:

- dim blue — assigned and idle
- breathing amber — pending or running
- bright green — needs attention
- off — no session assigned

The thread colours are device-wide. Disable other software that drives Creator
Micro agent lights while using it with Construct.

## Disable or diagnose

```sh
construct creator-micro status
construct creator-micro probe
construct creator-micro disable
```

`probe` opens the vendor channel in shared mode and makes a read-only firmware
version request. It distinguishes enumeration, permission/ownership, write, and
response failures without changing the device keymap or lighting.

Configuration lives in `creator-micro.toml` under the config directory printed
by `construct paths`. Disabling takes effect for newly opened TUIs; an existing
TUI releases the device when it exits.
