# 0190-config-edits-apply-without-a-restart

Status: accepted
Date: 2026-08-03
Area: architecture
Scope: When an edit to the daemon's configuration file reaches the running daemon, and how the part that cannot be applied is told to the operator.

## Decision

A configuration edit is applied to the running daemon when it is saved, on the
same terms a service definition is. No restart is involved, and no command is
run.

Each part of the configuration has one propagation class, and the class is a
property of where the daemon reads that field:

| Change | Applies |
|---|---|
| Adding or removing a harness, its description, the template directory, suggestion generation | immediately |
| A model profile, a published model list, a usage probe command | on the next request |
| A harness binary, its arguments, its environment, the daemon environment, the default worktree setting, the orchestrator harness | on new sessions |
| The router's port; switching off a router that is already listening | on restart |

The classes are published as data that both the daemon and its clients read, so
the promise shown beside a field is the promise the daemon keeps. Adding a field
to the configuration means giving it a class.

**Nothing is re-applied to a running conversation.** A session keeps the harness
binary, arguments, and environment it was created with, for its whole life, for
the same reason a service-backed conversation keeps its instruction: a harness
assembles its world when it starts, and there is no point at which a changed
binary path could be delivered to a process already running from the old one.
A session created while an edit is being applied is created from one
configuration or the other, never from half of each.

**Reload is all or nothing.** If the file fails to parse, the running
configuration is left exactly as it was. This is what makes watching the file
safe: an editor caught mid-write produces a file that does not parse, changes
nothing, and is picked up once it is complete.

**A router that is listening does not stop.** The router's port is the one part
of the configuration the daemon holds in a form it cannot exchange while
running. Harness processes are told the port once, when they are spawned, and
have no way to be told it moved — so moving it, or withdrawing it, would
silently disconnect every session already dialing it. A router that is *not*
listening has no such obligation: switching one on applies immediately, because
nothing is depending on its absence.

**The residue is reported, once, where the restart is.** An edit that changes
something in the restart class is saved and read like any other, and the
operator is told which field is waiting and given the restart in one gesture.
The report names the field, not the file: "the router port is waiting" is
actionable, "the configuration changed" is not.

**Re-deriving is not patching.** A reload reads the configuration file, the
plugin registry, and the plugin manifests, and builds the running configuration
from all three, exactly as starting up does. It does not adjust the
configuration it already had. Anything else would let a harness contributed by
a plugin that has since been removed survive every reload, because a merge can
add a harness but cannot know to take one away.

## Reason

The configuration file sits in the same directory as the service definitions
beside it, and those already apply when saved. That a file needs a process
restart while its neighbour does not is not a distinction an operator can be
expected to hold, and nothing announced it: an edit simply had no effect, which
is indistinguishable from an edit that was wrong.

The restart was also worse than it sounds. It is not a private act — it drops
every attached client and re-spawns every adapter, so the cost of adopting a
one-line change was disproportionate to the change, and the guidance to perform
one appeared beside a dozen separate settings.

Publishing the propagation classes as shared data, rather than as prose beside
each setting, is what keeps the answer honest as the code changes. Prose drifts
from behavior silently; a shared table drifts loudly.

The restart class exists so that the promise stays true where it cannot be kept.
A design that applied everything would have to move a bound socket out from
under the processes dialing it; a design that admitted no exception would have
to keep claiming a restart for the fields that never needed one. Naming the
exception, and only the exception, is what lets the rest be believed.

## Consequences

- A harness removed from the configuration stops being offered as soon as the
  daemon notices, and a harness added becomes usable without a restart.
- Adding a field to the configuration means assigning it a propagation class; a
  field with no class is an unanswered question at the point of edit.
- A credential removed from the daemon environment is gone the moment the edit
  is saved. An operation already resolving one may still complete with it.
- Because the configuration is re-derived rather than patched, disabling a
  plugin removes the harnesses it contributed on the next reload.
- Watching the file means edits are noticed within a short interval rather than
  instantly; the schedule above is about what applies, not about the latency of
  noticing.
- An edit that changes nothing semantically — a comment, a reordering — is
  applied and reported as having changed nothing, rather than being announced.
- A restart residue survives a client disconnecting and reconnecting. It is a
  property of the running daemon, not of the session that happened to observe
  it, and it persists until the restart it names.
- Switching a listening router off, then on again, in a single edit is not a
  restart: the router never stopped, and the configuration once again says it
  should be running.

## Non-Goals

- Re-instructing, re-modelling, or re-confining a conversation that is already
  running, or moving one onto a harness binary it did not start with.
- Moving a bound port out from under the sessions that were given it. A port
  the daemon is serving is kept until the daemon is replaced.
- Editing the configuration from a client. This is about reading what the
  operator wrote, not about providing another place to write it.
- Applying a partially written file. A file that does not parse is not an edit.

## Examples

- An operator adds a harness to the configuration and saves. Within a couple of
  seconds it appears in the harness list and a session can be created on it. No
  restart, no signal, no command.
- A model profile is added. The next route resolution finds it; sessions already
  running keep the transport they were spawned with.
- A credential is added to the daemon environment. The provider it unlocks
  becomes available as a route target without the daemon being restarted, which
  previously required exporting it into the shell that launched the daemon.
- The router port is changed. The edit is read, everything else in it applies,
  and the operator is told the port is waiting on a restart — with the restart
  one keystroke away. The daemon keeps serving the port it already bound.
- A plugin is disabled. On the next reload the harnesses it contributed are no
  longer offered.
- The file is saved with a syntax error. The daemon keeps running exactly as
  before, says so, and picks up the corrected file when it is saved.
