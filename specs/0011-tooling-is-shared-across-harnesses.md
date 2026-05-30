# 0011-tooling-is-shared-across-harnesses

Status: accepted
Date: 2026-05-30
Area: protocol
Scope: Applies to fleet-control tools, browser tools, memory access, widgets, and harness integrations.

## Decision

agentd should expose one shared tool surface across supported harnesses. Built-in and external agents should be able to access the same fleet-control, memory, widget, and browser capabilities through the integration style appropriate for that harness.

## Reason

agentd coordinates a fleet of heterogeneous agents. If each harness gets a different tool model, agents cannot reliably hand off work, inspect each other, or follow the same operating conventions.

A shared tool surface also keeps product behavior consistent. A capability should feel like an agentd capability, not like a one-off feature of a single harness.

## Consequences

New agent-facing capabilities should be designed once and then made available through all practical harness integration paths.

Harness-specific limitations are acceptable, but they should be treated as integration gaps rather than separate product semantics.

Tools should carry enough session identity and context that agents can avoid accidentally acting on themselves or using the wrong project state.

## Non-Goals

This does not require every harness to use the same transport or implementation strategy.

## Examples

An agent should be able to inspect sessions, update memory, and create task widgets whether it is using the native harness or an MCP-capable child harness.
