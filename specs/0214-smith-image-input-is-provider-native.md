# 0214-smith-image-input-is-provider-native

Status: accepted
Date: 2026-09-09
Area: harness
Scope: Smith accepts explicit local image references as durable multimodal user turns and translates them at each provider boundary.

## Decision

Smith recognizes explicit local image input in submitted user text: a bare or
relative image path, a Construct `[#file:…]` session-attachment reference, or a
local Markdown image link. Clipboard images and remotely uploaded files use the
same path because Construct clients first store their bytes as session
attachments and paste the resulting daemon-host path into the harness.

At submit time Smith reads each image, validates its format and portable size
limits, and snapshots its MIME type and base64 bytes into the canonical user
message alongside the original text. That canonical message is what Smith
persists and replays. Resume and provider retries therefore use the same bytes;
they do not depend on the source path continuing to exist or retaining the same
contents.

Provider adapters translate the canonical image into their native wire shape:
Chat Completions image URL blocks, Anthropic image source blocks, Gemini inline
data parts, Responses API input-image blocks, or Ollama image arrays. Text-only
turns retain their existing message variant and wire shape. A wire-capable
provider may still report that a selected model lacks vision. A provider whose
wire is known not to support image input must reject the turn locally with a
clear error before the new turn is persisted; DeepSeek is currently in that
category.

Portable inputs are PNG, JPEG, GIF, and WebP, at most 5 MiB per image and 20
images per turn. Local paths are never fetched from remote URLs.

## Reason

Clients already had a secure, session-scoped way to carry pasted binary data
to the daemon host, but Smith treated the resulting path as ordinary text. The
model could only discover the image by choosing a filesystem tool, and several
providers never received their native multimodal content at all. Normalizing
once at the Smith boundary keeps clients provider-agnostic while preserving
the exact turn across retries, restarts, and model changes.

## Consequences

- The structured transcript remains textual and shows exactly what the user
  submitted; image bytes live only in Smith's persisted canonical history and
  provider requests.
- Compaction describes the count of images in compacted history rather than
  embedding their base64 in the summarizer prompt. Normal context pruning drops
  an image with its complete user-led turn.
- Switching an image-bearing conversation to a text-only provider fails
  clearly until the user switches back or clears that conversation history.
- Provider implementations added later must explicitly advertise and implement
  image translation before Smith will send multimodal history to them.

## Non-Goals

- PDF, video, audio, remote-URL, or model-generated image support.
- Inferring that incidental prose mentioning an image filename is an
  attachment; references must have an explicit path-like shape.
- A new image preview or attachment chip in Smith's transcript UI.
