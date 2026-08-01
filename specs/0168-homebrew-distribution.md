# 0168-homebrew-distribution

Status: accepted
Date: 2026-07-31
Area: convention
Scope: Homebrew is an installation and upgrade channel for macOS users.

## Decision

The Homebrew package installs the single `construct` executable from the
architecture-specific macOS artifact published by the project's GitHub
Release workflow. Homebrew-managed installations are upgraded with
`brew upgrade construct`; `construct upgrade` and automatic in-app upgrade
prompts must not replace a Homebrew-managed executable in place.

## Reason

The release workflow already produces checksummed Apple Silicon and Intel
artifacts. Reusing those artifacts makes the tap install fast and keeps the
Homebrew package identical to the binaries distributed through the project's
installer. Homebrew owns files inside its Cellar, so an application-level
self-update would bypass Homebrew's version tracking and can fail on a
read-only or replaced keg.

## Consequences

The formula must be updated with every release's version and macOS checksums.
The formula installs only the `construct` binary because all adapters and the
daemon are built into that executable. Users who installed through Homebrew
must use Homebrew to upgrade. The release artifacts remain the source of truth
for the tap package; no separate macOS build is required.

## Non-Goals

This decision does not make Homebrew responsible for installing the external
harness CLIs that construct can wrap, such as Codex or Claude Code.
