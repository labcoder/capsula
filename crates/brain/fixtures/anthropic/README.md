# Synthetic Anthropic Messages fixtures

These are hand-written synthetic fixtures, not provider recordings. Their event
shapes follow the public [Messages streaming reference](https://platform.claude.com/docs/en/api/messages-streaming)
and [errors reference](https://platform.claude.com/docs/en/api/errors).
All identifiers, text, tool names and token counts are invented.

- `synthetic-text-tool-usage.sse` exercises ordered text, tool-input and usage
  forwarding, including multibyte UTF-8, ping and an unknown event. Tests also
  replay a CRLF version and fragmented bytes without assembling tool arguments.
- `synthetic-error-after-text.sse` places an overload after partial text and
  before a valid ending. Its planted marker checks diagnostic redaction.
- `authentication-error.json` and `billing-error.json` are fixed synthetic
  response bodies replayed with HTTP 401 and 402. They contain no key,
  credentials, identity, or original error text.

Boundary cases are generated in the reader tests rather than stored as large
fixtures. No live API call or credential is needed for these offline tests.
