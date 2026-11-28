# Local Chat Completions fixtures

- `synthetic-llama-server-text.sse` is written by hand in the shape llama-server documents.
  It's not a recording.
- `llama-server-b12774-text.sse` was recorded from llama.cpp's `llama-server` b12774 (CPU build)
  serving LFM2.5-1.2B-Instruct Q4_K_M as `local-model` on loopback, with temperature 0 and seed 1.
  It ends with a `timings` object and no `usage` object.
- `ollama-0.40.19-thinking-text.sse` was recorded from an isolated Ollama 0.40.19 instance with its
  own models folder, home and port, serving `openbmb/minicpm5-2b:q4_K_M` as `local-model` on
  loopback, with temperature 0 and seed 1. It streams `reasoning` deltas before the answer and has
  no `usage` object. To fetch the same build and model again: Ollama 0.40.19's
  `ollama-windows-amd64.zip` has SHA-256
  `0114b9f103d783b96f14390cf7f11d0fa360040b3ee9941f370fdee503cca308`, and the model tag is
  `openbmb/minicpm5-2b:q4_K_M` (digest `71a44e98400f`).

Both recordings answer the request the adapter sends (`model`, `messages`, `stream`) for the
single prompt "Reply with exactly: Hello from loopback." Each was the only request sent, with the
model loaded offline. Ids and `created` times are replaced, along with llama-server's
fingerprint build hash and its timing rates. Token counts, event order and every text and reasoning
delta are kept byte for byte.
