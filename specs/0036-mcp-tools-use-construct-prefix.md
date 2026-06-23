# 0036-mcp-tools-use-construct-prefix

Status: accepted
Date: 2026-06-23
Area: protocol
Scope: MCP tool naming for construct's injected control server.

## Decision

Construct's MCP server advertises control tools with the `construct_` prefix. It does not advertise or accept `agentd_`-prefixed control tool names.

## Reason

The product-facing binary and MCP server are named construct. Keeping the old `agentd_` prefix in MCP tool names leaks the pre-rename brand into model-visible APIs and confuses agents choosing between available construct tools.

## Consequences

Future MCP control tools should follow the `construct_*` naming pattern unless they belong to an unrelated domain such as browser automation. Renames of this surface are protocol changes and should be reflected in tests that inspect the advertised catalog.

## Non-Goals

This decision does not rename Rust crate/package identifiers or persisted internal schemas that still carry historical `agentd` names.
