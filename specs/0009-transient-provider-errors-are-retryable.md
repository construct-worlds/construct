# 0009-transient-provider-errors-are-retryable

Status: accepted
Date: 2026-05-30
Area: harness
Scope: Applies to provider-backed agent turns, especially model provider streaming failures.

## Decision

Transient provider failures should be classified as retryable and retried automatically when retrying can preserve the user's intent without duplicating completed side effects.

## Reason

Provider APIs can fail due to rate limits, temporary overload, network interruption, server errors, or incomplete streams. Treating all such failures as fatal makes long-running sessions brittle and forces users to manually resubmit work that the system can safely continue.

At the same time, retries must avoid hiding deterministic errors or repeating actions that already happened.

## Consequences

Provider adapters should distinguish transient failures from permanent request, authentication, configuration, and policy failures.

Retry behavior should be visible enough that users can tell progress is still happening and should eventually surface a clear error if retries are exhausted.

Retried turns must preserve conversation intent. They should not duplicate assistant answers or tool side effects that were already accepted as complete. Partial reasoning is presentation rather than a committed assistant answer: when a transient stream failure occurs after reasoning but before any assistant text, the incomplete reasoning is labeled and the turn may be retried automatically.

## Non-Goals

This does not require infinite retries, retrying every error, or silently ignoring provider failures.

## Examples

A temporary provider overload before assistant text is emitted can be retried. A transport interruption after partial reasoning can also be retried, with the interrupted attempt identified in the UI. Once assistant text has streamed, automatic replay stops. An invalid API key or unsupported model name should fail clearly.
