# Anthropic Messages transport

A2a supplies validated request bytes, closed error recovery and a route-typed TLS connector.
A2b adds one bounded Messages attempt and reader. A3 owns engine selection, request content,
request encoding and activation. A3a adds bounded block/tool assembly and literal usage,
without activating an engine or promising thinking replay; this transport cannot fetch a key.

`MessagesBody` requires a model slug, nonempty user/assistant messages, `stream: true` and
`max_tokens: 2048`. It retains structured content and tools without interpreting arguments.
Only the supported top-level fields are accepted. Optional effort belongs in
`output_config.effort`. Tools are client tools only: absent or `custom` type; server-tool
discriminators are refused. Both request and streamed-error JSON are length-counted without
allocation, then serialized into a fixed-size zeroizing buffer without growth.
Exactly one ephemeral cache breakpoint belongs on the last system
block, or the last tool when there is no system prefix; dynamic message blocks cannot carry
one. Serialized requests are bounded to 16 MiB, and Debug prints only their byte count.

`AnthropicTls` uses OS roots, HTTP/1.1 without early data, and the fixed
`api.anthropic.com:443` host for both the connection and SNI. It implements the
sealed `AnthropicConnect` marker; the Responses connector does not. The Messages attempt requires
that marker rather than accepting an arbitrary connector or URL. Tests inspect an in-memory
ClientHello; they make no provider call.

`ProviderFailure` keeps a bounded private error body and exposes only closed categories,
recovery advice and localized error keys/actions. Authentication and permission failures ask
the user to check settings; rate limits and transient failures advise retry later. Billing
errors map to credit exhaustion, including only the exact documented legacy HTTP 400
`invalid_request_error` message. Unknown text stays a generic request refusal. Context
exhaustion in a stop reason is `Exhausted`, with an answer-limit catalog key and no Retry
action. Premature EOF stays `Truncated`, with connection-retry advice. No error mapping
retries, changes key state or forwards provider text.

The shared provider-neutral SSE framer enforces 1 MiB line and joined-event bounds and a
16 MiB stream bound. It emits joined data bytes until its consumer stops; it does not parse
JSON or recognize terminal events. The Responses reader keeps its own existing semantics.
The Anthropic reader forwards known events in order, ignores bounded pings/future types and
counts only `text_delta` toward its 1 MiB visible-text cap. It completes only on `message_stop`
after `end_turn`, `tool_use` or `stop_sequence`.
Nonterminal `message_delta` events with an absent or null stop reason are forwarded without
changing the last recorded reason; `message_stop` still requires a recorded supported reason.
Token/context exhaustion and explicit refusal forward their final delta before failing, and EOF before completion
fails. Malformed or failed streams stay failed on every later feed; a later valid stop cannot
rescue them. Completed readers emit no more events. A3a validates the block/tool and usage
payloads it assembles.

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
unread pings cannot extend it. If content-idle expires during a capacity wait, the failure
remains Transport rather than BackpressureTimeout. Receiver closure stops the attempt, and dropping the attempt
cancels all its owned I/O without a producer task. Refusal bodies are bounded to 1 MiB and
10 seconds. Closed errors expose no provider text or request/key material.

Request/error fixtures are explicitly synthetic public-reference shapes, not provider
recordings. Bounds, redaction, invalid envelopes and narrow credit recognition are tested
offline. No owner credential, live API behavior or macOS behavior has been verified. The
interleaved synthetic parsing measurement is recorded in the pull request body; it is not
provider latency or end-to-end throughput.

## Bounded mapper (not yet activated)

`engines::anthropic::Mapper` consumes the reader's events for one round. It requires a
message start, unique block indices and tool ids, matching delta kinds, and one stop per
block. A validation failure is sticky, but the reader's Exhausted, Refused or Provider
ending takes precedence. Empty initial text is skipped; other text becomes TextDelta.
Exact `draw` becomes Draw and every
other bounded tool name becomes Unknown. The coordinator alone decides offers, charges
calls and dispatches. Tool arguments use concatenated fragments, or the start object when
there are none, ignoring empty argument fragments, and must be a complete JSON object
before ToolCallReady. A nonempty start object used this way is also queued as one
ToolArgsDelta before ToolCallReady, so a streamed `draw` sees the bytes its Ready names.

Thinking and redacted thinking become private Opaque items indexed in provider order;
an absent initial signature waits for signature_delta, and thinking stopped without a
signature fails closed. Unknown/fallback blocks are skipped without copying their payload.
The mapper caps blocks at 64, retained plus queued content at 1 MiB, and each Opaque
item at 256 KiB. Each queued event carries 64 bytes of overhead, including zero-length
events, with 64 bytes reserved at message start for final Usage. Events have no
per-read event-count cap, so a dense 16-KiB read can deliver all
its deltas. Taking events transfers ownership before the next read.

Usage fields retain literal nonnegative u32 counts; absent fields and null input/cache
fields preserve earlier counts, while output_tokens stays strict. Cumulative counts cannot
decrease. On success or a final exhaustion/refusal delta,
the mapper stages one Usage with `first_request: Some(whole)` before the caller emits
Finished, including when an exhausted/refused round has truncated tool arguments.
Premature EOF emits no Usage because its snapshot is only a lower bound. A stalled
consumer during delivery can still replace the semantic failure with a delivery timeout.
There is no usage persistence, retry loop, key access, runtime router or replay contract
in A3a. Those integration decisions belong to A3b.
