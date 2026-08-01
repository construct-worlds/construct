# 0168-construct-doctor-reports-and-never-repairs

Status: accepted
Date: 2026-07-31
Area: cli
Scope: What `construct doctor` is allowed to do to the machine it inspects, what its severities and exit code mean, and where its check logic is allowed to live.

## Decision

`construct doctor` is a **read-only diagnostic**. It observes and reports; it
never repairs, migrates, creates, or starts anything.

Five rules define it:

1. **It never mutates the machine.** No daemon autostart, no directory
   creation, no config rewrite, no login refresh, no probe files left
   behind. Writability is established with an access check, not by writing.
2. **It works with the daemon down.** That is its primary use case, not a
   degraded one. A daemon that is absent is a *finding*, never a
   precondition. Checks that genuinely need a live daemon are still
   emitted, marked informational and explicitly labelled as skipped —
   never omitted.
3. **Severity is a claim about the user's machine, not about the report.**
   `error` means *construct cannot work here*, and it is the only thing
   that sets a non-zero exit code. `warn` means *something is degraded or
   surprising and you probably want to know*. `info` is context. A machine
   with no daemon running, an expired third-party login, or a missing
   optional harness is **not** an error — those are warnings. Making the
   ordinary state of a CLI-only user exit non-zero would destroy the exit
   code's meaning.
4. **Every non-`ok` finding carries the exact command that addresses it.**
   A diagnostic that names a problem without naming its remedy shifts the
   work back onto the user. There is no `--fix`: the user runs the command,
   because the commands are things a user should understand having run.
5. **Checks reuse the daemon's real probes.** Doctor asks the same
   question, the same way, that the daemon will ask when it actually
   spawns a session. A doctor that probed independently would eventually
   disagree with the daemon, and a diagnostic that disagrees with the
   system it diagnoses is worse than no diagnostic.

The report is a flat list of sections, each holding findings. Every finding
has a **stable machine-readable id** that is always present, whether or not
the check could run. `--json` emits the same structure that the text
rendering is derived from; consumers key off ids, never off wording,
ordering, or column layout.

Because rule 5 requires the daemon's own probe code and rule 2 forbids
reaching it over IPC, the check logic lives **inside the daemon crate**
behind a narrow public surface, and the client half injects the facts only
it can observe (whether a daemon answered, what it said). That public
surface exposes only plain-data types the module owns — never internal
types that a consumer could receive but not read.

## Reason

Construct installs with one command and then depends on a fair amount of
ambient machine state: four directories, a config file, a daemon, a
socket, several third-party harness binaries found through `PATH`, and
several third-party OAuth logins Construct reads but does not own. When any
of that is wrong the user meets the failure at the point of use, one
symptom at a time, with no way to see the whole picture. This is also the
artifact people paste into bug reports.

The rules exist because each protects against a specific failure:

- **Never mutate**: doctor is run *precisely* when something is already
  broken. A diagnostic that starts a daemon to check whether a daemon is
  running answers its own question and destroys the evidence.
- **Works with the daemon down**: putting the checks behind IPC would make
  them unreachable in exactly the situation that motivates the command.
- **Error is narrow**: an exit code is only useful if it is quiet in the
  normal case. Most healthy machines have a stopped daemon and at least one
  uninstalled optional harness.
- **Fix hints, not `--fix`**: the remedies span other vendors' tools
  (`codex login`, `claude`), the user's own filesystem permissions, and
  their editor. Automating those means Construct silently acting inside
  systems it does not own.
- **Reuse the daemon's probes**: the single most confusing real failure is
  "`claude` works in my terminal but Construct says it's missing" — a
  `PATH` difference between the user's shell and the daemon's environment.
  Only a doctor that asks exactly what the daemon asks can name that.

## Consequences

- New checks are added by reusing an existing daemon probe, not by writing
  a parallel one. If a probe is not reachable from the doctor module, the
  fix is to lift the probe into a shared function that both the daemon and
  doctor call — not to reimplement it.
- Promoting a check to `error` severity is a deliberate decision about the
  exit code, and must be justified as "construct cannot work here". The
  present error-capable set is small: unusable state directories and an
  unparseable config, plus a socket that accepts connections but does not
  answer.
- Finding ids are a compatibility surface. Renaming one breaks scripts and
  bug-report tooling; adding one does not.
- The report degrades rather than aborts. A check that cannot run produces
  a finding saying so; an unparseable config falls back to built-in
  defaults so the remaining sections still report something useful, with a
  note that the user's config was not applied.
- Doctor makes no network calls of its own. Update-availability is read
  from an existing on-disk cache, never refreshed. The command must also be
  excluded from any interactive upgrade prompting, which both hits the
  network and can block on input.
- Some reused probes are not perfectly local (a harness probe may consult a
  keychain or a user-configured host). Rule 5 wins: fidelity to the
  daemon's behavior is worth more than a strict no-syscall guarantee, and
  the deviation is documented rather than avoided.
- The report's layout is plain ASCII, and color is strictly additive on top
  of it. Severity coloring is an accelerant for reading a long report on a
  terminal, never the carrier of a distinction — every severity is stated in
  words in the same line it colors, so the report survives being redirected
  to a file, piped through a stripper, read with `NO_COLOR` set, or pasted
  into an issue. Removing every escape code must yield exactly the plain
  render, which also means column math runs on unstyled text.
- Color follows the platform conventions rather than the TUI's. It is on
  only for a terminal that wants it, overridable both by `NO_COLOR` and by
  an explicit flag, and always off for the machine-readable output. The
  palette stays within the basic ANSI colors so the user's own terminal
  theme decides the hues.

## Non-Goals

- Repairing anything, now or later, under any flag.
- Checking the health of *sessions* or *harness conversations*. Doctor
  diagnoses the installation, not the work.
- Reachability of remote services, model endpoints, or tunnels. Those are
  network-dependent and belong to whatever surface owns them.
- Being a supported machine-readable API beyond finding ids and severities.

## Examples

- No daemon running, everything else fine: `daemon.socket` is a warning
  whose fix is the daemon-start command; daemon-dependent checks are
  informational and say they were skipped; the command exits **0**.
- A socket file exists but nothing is listening: still a warning, worded as
  a stale socket, because the remedy is the same.
- A socket accepts a connection but the daemon does not answer: **error**,
  exit **1** — something is holding the address and Construct cannot use it.
- A config file that does not parse: **error**, exit **1**, with the
  parser's own diagnostic carried as continuation lines and an editor
  command as the fix; the rest of the report still renders against
  built-in defaults.
- A harness login that has expired: **warning**, with the third-party
  renewal command as the fix. Construct does not run it.
- The daemon reports a harness as missing while a local probe finds it:
  **warning** naming the disagreement, because the daemon's environment is
  what actually governs session spawn.
