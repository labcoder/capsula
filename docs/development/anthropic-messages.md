# Anthropic Messages transport

A2a supplies validated request bytes, closed error recovery and a route-typed TLS connector.
It does not send requests; the A2b attempt and A3 engine integration land separately.

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
exhaustion is truncation. No error mapping retries, changes key state or forwards provider text.

The shared provider-neutral SSE framer enforces 1 MiB line and joined-event bounds and a
16 MiB stream bound. It emits joined data bytes until its consumer stops; it does not parse
JSON or recognize terminal events. The Responses reader keeps its own existing semantics.
Anthropic event semantics and the interleaved Rez parsing measurement belong to A2b.

Request/error fixtures are explicitly synthetic public-reference shapes, not provider
recordings. Bounds, redaction, invalid envelopes and narrow credit recognition are tested
offline. No owner credential, live API behavior or macOS behavior has been verified.
