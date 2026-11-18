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
retries, changes key state or forwards provider text. A streamed `error` event is classified
from the event as the reader already read it, never from a second parse of its bytes.

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
there are none, ignoring empty argument fragments, and must be a complete JSON object,
with no member name repeated, before ToolCallReady. A start object with members (however it is spaced) is also queued as one
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

## The request body (not yet activated)

`engines::anthropic::encode::body` turns the store's `ModelRequest` into a `MessagesBody`, whose
envelope check (`MessagesBody::from_json`) runs on the exact bytes it sends, and the run's
`Conversation`, the exact request a tool round continues. The body holds: `model`, `max_tokens` 2048 (the envelope's cap, including
thinking), `stream: true`, `output_config.effort`, the tools as `name`, `description` and
`input_schema`, and the instructions as the one system block. That block carries the only
`cache_control` breakpoint, so the tools and the instructions are the cached prefix and stay
identical across turns. The body has no `thinking` field. On Claude Sonnet 5.5 that means
adaptive thinking, which can't be turned off (`disabled` is a 400), so a reply can carry signed
thinking blocks, usually with empty text, before its calls.

Input items become content blocks, and neighbouring blocks of one role merge into one message,
since Messages wants roles to alternate:

| Input item | Block |
|---|---|
| a user message | a user `text` block |
| a developer message (summaries, notes, the screen lines, the compaction prompt) | a user `text` block that starts `Kumochi note:`, since Messages has no developer role and the system block must stay the same |
| an assistant message | an assistant `text` block |
| a stored call | an assistant `tool_use` block whose `input` is the parsed arguments |
| a result | a user `tool_result` block |
| a native item | rebuilt from its known fields on the assistant side: a `text` block with its text, or a `tool_use` block with its id, name and object input; the build passes only this connection's |

Empty messages are skipped. The `Kumochi note:` label tells the model where a line came from;
it carries no authority, and a user who types it is still the user. Nothing is built for:

- a request with no model, a model on another connection, or no instructions;
- an empty conversation, or one that ends with the assistant, which would ask for a prefill;
- arguments that aren't one JSON object;
- a call id or tool name that isn't 1 to 64 ASCII letters, digits, `_` or `-`;
- a native item other than those two blocks, with exactly those fields (a thinking, image or
  document block among them, since stored items never carry thinking);
- a call that isn't answered exactly once by the results that open the next user message, a
  result anywhere else, or an id called twice. Nothing is moved to make a pair;
- an image, which this route doesn't carry yet;
- a serialised body over the 512 KiB bound.

### Tool rounds

A round that called tools continues its run's `Conversation` with `encode::next`. The context
isn't built again: the next request is the last one unchanged, then the round's reply as one
assistant message, then the results as one user message. Each signed block is bound to the
system text, the tools and every message before it, so a rebuild, which refreshes the drawing
digest, would invalidate it.

Only the round's own mapper supplies the reply. The run makes it with
`Mapper::answering(&conversation)` before the stream's first callback, which binds it to that
request's `Prefix`; each built request gets its own, so a rebuild of the same input is another
one. When the stream completes, `Mapper::reply` hands the whole reply over once: every block in
output order, exactly as it came. Every event is read once, by the literal reader below, and
the reader and mapper decide everything from that one read. The transport makes a fresh reader
for each stream, keeping what `Mapper::capture` names: only a bound mapper's keeps each
`content_block_start` event's bytes, and an unbound stream keeps none, with the same checks
either way. The mapper keeps every member of a block's start as written and in its order (a
call's `caller`, a text block's `citations`, a field this route doesn't know) and sets only the
fields documented deltas stream: `text`, `thinking`, `signature`, and `input` when fragments
came. A streamed field the start lacked comes last. `Reply` has no constructor or field outside
the mapper, and the coordinator adds only the matched results. This is run memory only: the
store never gets a raw block.

There's no reply when the mapper can't put a block back as it came: an unknown block, a start
that isn't an object, an empty text block or empty redacted data (Messages refuses both), a
initial input with members that fragments then follow, or content already in `message_start`. Nor
after a failed or partial stream: a member name repeated anywhere in an event or a call's input,
an id or name that isn't a string, an unknown delta, unsigned thinking, a block left open, a
missing index, or EOF before `message_stop`, even after closed blocks.

Tool input stays text from the stream to the wire. On a bound stream the tool runs with the same
bytes the reply sends back: the fragments, or the start's `input` as written when none came.
`Conversation` keeps its messages as JSON text, read back literally only for the pairing and
role checks, and `MessagesBody::from_json`, which only the encoder can call, checks the envelope
and then sends those bytes. The literal reader refuses a member name repeated in any object, and
reads every name as a plain string, so it never takes an object for serde_json's embedded raw
value. That holds in a workspace build too, where gpui turns serde_json's `raw_value` feature
on; the brain doesn't ask for it. The 512 KiB bound applies to the final bytes. A 30-digit integer or a long
decimal goes back as the model wrote it. Whitespace between members and the escapes in streamed
text may differ from the stream, since the provider's prefix check compares parsed JSON. An
unbound stream's start-only input reaches the tool as its parsed value written again, as before,
since nothing from that stream goes back as a reply.

Nothing continues, and the tool flow ends with a local refusal, when the reply answered another
request, or the next request would go on another sign-in tag or model. A `Prefix` means the
request was built, nothing more: whether the run may still send, after a withdrawn source, Stop
or a lost owner, is for the live owner to decide. A source deleted or withdrawn during the run is
never sent again just to keep a signature valid.

Signed thinking lives in the run's memory only. It isn't stored, and a later run replays the
earlier ones' text and call pairs without it. Anthropic allows this: prior turns' thinking may
be left out outside an open tool call, as long as no block is left out between two that are
kept. The next model loses that reasoning, as it loses tool details the answer never stated.
A reply without a call, a call without exactly one result, or an id that was already called
builds nothing.
