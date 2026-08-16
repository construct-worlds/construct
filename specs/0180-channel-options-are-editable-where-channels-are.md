# 0180-channel-options-are-editable-where-channels-are

Status: accepted
Date: 2026-08-02
Area: ux
Scope: An option that exists in a channel definition is offered by the clients that edit channels, not only by the configuration file.

## Decision

Adding an option to a channel definition is not finished until the clients that
edit channels can set it. A definition field reachable only by hand-editing
configuration is an incomplete feature, not a smaller one.

Three rules keep that honest as options accumulate:

- **The accepted values are published once.** The set a client offers and the
  set the daemon accepts come from the same declaration. A client cannot offer
  a value that would be rejected, and an option gains its editor and its
  validator in the same change.
- **An omitted option preserves the stored value.** A client that does not know
  about an option must be able to save an unrelated field without resetting it.
  Absent means "unchanged", never "default".
- **An option is reported back.** A client that is about to preserve a value
  has to be able to show it. Write-only credentials are the sole exception, and
  they are reported as present rather than returned.

An option that applies to one channel kind is offered only for that kind, and
submitting it for another kind is refused rather than stored unread.

## Reason

Two consecutive changes each added a Slack option to the definition and to the
documentation, and neither added it to a client. The result was a user who
could see in the docs that a bot can answer untagged messages, find the channel
editor in front of them, and still have to go find a TOML file — while the
editor sat one field away, silently preserving a value it would not show.

The three rules address how that happens rather than the two instances of it.
Duplicating the accepted values in each client is what lets an editor drift out
of step with the validator. Defaulting on absence is what makes adding an
option to one client silently reset it from another. Not reporting the value is
what leaves a client unable to render a field even once someone wants to.

## Consequences

- Adding a channel option means touching the wire type, the daemon's
  validation, and every client that edits channels — in one change. That is
  more work per option than a configuration-only field, and it is the point.
- The option's propagation class must be declared, per
  [0173](0173-service-definitions-apply-without-restart.md). For an outbound
  channel that holds its configuration in a live connection, saving an option
  replaces that connection.
- A client written against an older protocol keeps working: it sends no option,
  and preserves every value it cannot show.
- Options whose safe use depends on context — anything that widens who can put
  text in front of a session — carry that caveat in the client, not only in the
  documentation. The user making the decision is the one looking at the
  field.

## Non-Goals

- This does not say every service definition field must be editable from every
  client. It is about channel options specifically, and about the client that
  already edits that channel.
- It does not require a CLI subcommand for each option. "The clients that edit
  channels" means the surfaces that already present a channel editor.

## Examples

- A Slack channel's editor lists its behavior options beside its allowlists,
  with what each needs from the Slack app stated in the same view.
- Saving that editor from a client that offers only the allowlists leaves the
  behavior options exactly as they were.
- Submitting a Slack-only option for an HTTP channel is refused, rather than
  stored and reported back as though it were in effect.
