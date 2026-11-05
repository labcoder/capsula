# Anthropic Messages transport

A2a supplies validated request bytes, closed error recovery and a route-typed TLS connector.
A2b adds one bounded Messages attempt and reader. A3 owns engine selection, request content,
block/tool assembly, thinking replay and cumulative usage; this transport cannot fetch a key.

`MessagesBody` requires a model slug, nonempty user/assistant messages, `stream: true` and
`max_tokens: 2048`. It retains structured content and tools without interpreting arguments.
Only the supported top-level fields are accepted. Optional effort belongs in
`output_config.effort`. Exactly one ephemeral cache breakpoint belongs on the last system
block, or the last tool when there is no system prefix; dynamic message blocks cannot carry
one. Serialized requests are bounded to 16 MiB, and Debug prints only their byte count.

`AnthropicTls` uses OS roots, HTTP/1.1 without early data, and the fixed
`api.anthropic.com:443` host for both the connection and SNI. It implements the
`AnthropicConnect` marker; the Responses connector does not. The Messages attempt will require
that marker rather than accepting an arbitrary connector or URL. Tests inspect an in-memory
ClientHello; they make no provider call.

`ProviderFailure` keeps a bounded private error body and exposes only closed categories,
recovery advice and localized error keys/actions. Authentication and permission failures ask
the user to check settings; rate limits and transient failures advise retry later. Billing
errors map to credit exhaustion, including only the exact documented legacy HTTP 400
`invalid_request_error` message. Unknown text stays a generic request refusal. Context
exhaustion in a stop reason is truncation, not a provider-error category. No error mapping
retries, changes key state or forwards provider text.

The shared provider-neutral SSE framer enforces 1 MiB line and joined-event bounds and a
16 MiB stream bound. It emits joined data bytes until its consumer stops; it does not parse
JSON or recognize terminal events. The Responses reader keeps its own existing semantics.
The Anthropic reader forwards known events in order, ignores bounded pings/future types and
counts only `text_delta` toward its 1 MiB visible-text cap. It completes only on `message_stop`
after `end_turn`, `tool_use` or `stop_sequence`. Token/context exhaustion is truncation,
explicit refusal is closed, and EOF before completion fails. Malformed or failed streams stay
failed on every later feed; a later valid stop cannot rescue them. Completed readers emit no
more events. A3 validates the block/tool and usage payloads it assembles.

## One attempt

The caller supplies its validated body, key, admission future and shared run/live budgets.
After admission, the transport reserves the run budget then the live budget before connecting.
A live-cap refusal can conservatively spend the run reservation; nothing refunds a failed,
cancelled or uncertain attempt. Retrying requires another admission and consumes another unit.

The attempt accepts only `AnthropicConnect`, fixes `POST /v1/messages`, sensitive Bearer bytes
and API version `2023-06-01`, and uses no proxy, redirect or fallback. Request bytes are wiped
on drop. A connection is closed after every attempt; no pooling/drain path is needed. Upload
and connect share a 10-second deadline. The admission lease settles only after the upload
flushes or the socket closes, including early refusal and cancellation.

Headers and body reads have a 60-second byte-idle deadline; pings reset it. A separate
10-minute content-idle shortcut prevents an empty ping stream from living forever, pending
approved live gap measurements. Each downstream-capacity wait has its own 60-second bound;
unread pings cannot extend it. Receiver closure stops the attempt, and dropping the attempt
cancels all its owned I/O without a producer task. Refusal bodies are bounded to 1 MiB and
10 seconds. Closed errors expose no provider text or request/key material.

Request/error fixtures are explicitly synthetic public-reference shapes, not provider
recordings. Bounds, redaction, invalid envelopes and narrow credit recognition are tested
offline. No owner credential, live API behavior or macOS behavior has been verified. The
interleaved synthetic parsing measurement is recorded separately; it is not provider latency
or end-to-end throughput.
