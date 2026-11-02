# Scene v1

`capsula-scene` is the shared, synchronous drawing format. It depends on neither
GPUI nor tokio. The brain parses model JSON; the shell decodes the same validated
types from postcard. There is no renderer or scene-state caller yet.

## Records

A drawing call is `{"records":[begin, step, ..., end]}`. Each record is externally
tagged, for example:

```json
{"begin":{"v":1,"scene":"demo","surface":"overlay","alt":"Two connected nodes"}}
```

```json
{"step":{"id":"first","put":[{"diagram":{"id":"d","box":[40,60,880,420]}},{"node":{"id":"a","in":"d","label":"First"}}]}}
```

```json
{"end":{}}
```

The version, scene id, surface and alternative text are required. Steps have a
required id, optional `say`, empty-by-default `put`, `set` and `remove` lists, and
`start` defaulting to `"speech"`. `put` is bounded to 856 operations (600 objects
plus 256 tracks); `set` and `remove` each allow at most 600 entries. Other starts are `"now"` and
`{"after":"earlier-step","ms":200}`. Ordering and reference checks belong to
the scene-state validator, not the typed parser.

`put` accepts `line`, `arrow`, `path`, `shape`, `text`, `callout`, `highlight`,
`diagram`, `node`, `edge`, `table`, `row`, `chart`, `series`, `anim` and `mermaid`.
Addressable objects require model ids: there are no generated `_1` aliases.
Ids contain 1–40 ASCII letters, digits, underscores, dots or hyphens. Host handles
are a separate, opaque `u32` identity.

Points are logical pixels of the inline scene's `size` or the overlay display.
Anchors are `[x,y]` or `{"ref":"object-id"}`. Coordinates must be finite and
within ±20,000. Chart data uses finite f64 values instead of coordinate-bounded
numbers. Composed transforms and domain-specific ranges are admission checks.
Screen/capture anchors and all 3D operations are absent, not ignored.

All relative timing fields, including delays, entrances, edge flow, after-step
offsets and animation key times, are milliseconds in 0–60,000. `Ms` validates on
both routes and `checked_add` returns none if a sum exceeds that cap. `at_word` denotes an estimated offset within a synthetic speech
segment, not real-audio alignment. Optional style values are left to preparation;
for example, an omitted diagram direction can use the inline surface's downwards
default instead of imposing the overlay's direction at parse time.

## Strict decoding and errors

`parse_record` consumes exactly one JSON record of at most 32 KiB. Unknown fields,
unknown operations, duplicate fields, invalid ids, unsupported versions, non-finite
numbers and excess text/list lengths fail. Text counts Unicode scalar values, not
UTF-8 bytes; overlong content is never truncated. Bounded lists reject the excess
element before decoding or storing it.

Path data (`d`) and Mermaid source (`src`) use `Source`, not display `Text`.
Each is capped at 8 KiB of UTF-8 bytes (`MAX_SOURCE_BYTES`); the path figure comes
from S1, while the Mermaid figure is a starting cap. A5 separately enforces the
512-command path limit. Source Debug exposes byte length only.

The mixed-syntax types (`Start`, `Anchor`, `Color`, cells, chart points and key
values) use JSON visitors only when `is_human_readable()` is true. Their binary
branches use declaration-order enum tags and the same validated payload types and
constructors. RGB payloads are restricted to 24 bits in both formats. There are no
untagged enums, flattened fields or omitted binary struct fields.

Postcard enum declaration order is **append only**: never reorder variants or
insert a new variant before an existing one. Golden-byte tests pin every enum's
tags, including the private mixed-syntax representations, because steps will be
persisted and replayed. Version is the literal integer 1, not an enum index.

Decode paths come from `serde_path_to_error`, filtered through known schema field
names and array indices. Unknown model keys never become diagnostic path text.
Only fixed custom-error tokens and serde diagnostic prefixes are inspected to
choose an `ErrorCode`; the parser's original message is discarded. `StepError`
uses `MissingField` for absent required fields and appends only an allowlisted
field name to its path. `UnknownOp` denotes only an unknown record/op tag;
unknown vocabulary values and invalid numeric values are `OutOfRange`.
`StepError`'s `Display` and `Debug` contain only the path and code. `tool_text` may include a
known step id when replying to its model, but never the rejected field value.
`Text` and `WireId` debug formatting exposes lengths only. Do not trace records,
serialize drawing content into logs, or trace a model-facing tool reply.

## Caps

These constants describe the full admission contract. This first layer enforces
record bytes, validated scalar types and bounded individual lists; aggregate,
reference, motion and residency checks are added by subsequent scene layers.

| Resource | Per object/scene | Global across visible scenes |
|---|---|---|
| Encoded input | 32 KiB/record, 512 KiB/call, JSON depth 16 | — |
| Text and ids | 400 Unicode scalars/text, 40 ASCII characters/id | — |
| Path/Mermaid source | 8 KiB UTF-8/source | — |
| Steps | 64 | — |
| Step lists | 856 puts, 600 changes, 600 removals | — |
| Live objects, including pending removals | 600 | 1,500 |
| Diagram | 48 nodes, 96 edges | — |
| Table | 8 columns × 40 rows | 4,000 cells |
| Chart | 8 series × 500 points, 40 categories | 20,000 marks |
| Path | 512 points, 4,096 flattened segments | 20,000 segments |
| Motion | 256 tracks, 32 keys/track, 60 s/track and after last activation | — |
| Object graph | Depth 16 | — |
| Coordinates | Finite, absolute value ≤20,000 after composition | — |
| Prepared residency | Charged at preparation, independent of admission | 64 MiB |
| Decoded images | Reserved for capture/image support | 64 MiB |
| Visible scenes | — | 4 |

`limits.rs` also reserves architecture §4.4's slice-5 constants: 160 solids,
24,000 triangles per scene/globally, 48 curved segments, and BSP limits of
4,000,000 classifications, depth 64 and twice the input triangle count in
fragments. No 3D vocabulary or geometry is implemented here.

## Incremental drawing input

`Tokenizer::next(&mut rest)` consumes a delta through at most one closed record
and leaves the exact unconsumed suffix in `rest`. Parse its borrowed bytes into
an owned `Record` before awaiting persistence; the next call clears and reuses
the same buffer. There is no callback, record queue or accumulated-text reparse.
Drain each supplied delta until `None`, then provide the next delta. At EOF call
`finish`: a missing record close or envelope tail is `Truncated`.

The outer object must contain exactly one `records` key and an array of record
objects. Escaped spellings of that key work; repeated keys and additional keys
fail. A leading byte-order mark (BOM) and invalid UTF-8 outside strings are rejected.
Whitespace, nested objects/arrays, JSON number/literal syntax, split UTF-8,
escapes and paired UTF-16 surrogate escapes are handled incrementally. Duplicate
fields inside a record remain the typed parser's responsibility.

The tokenizer counts bytes before buffering: at most 512 KiB for the whole
envelope and 32 KiB from each record's opening brace through its closing brace.
Surrounding array whitespace is charged only to the drawing. Container depth is
at most 16, including the envelope object and records array. One 32 KiB buffer is
allocated at construction; scanning and record delivery allocate nothing further.
`Malformed { at }` reports the zero-based drawing byte offset of the first invalid byte.
Every syntax, size, depth or truncation error is terminal. There is no newline
resynchronization, and trailing non-whitespace after the envelope fails. Earlier
records may already have been emitted, so callers retain their committed prefix.
Tokenizer and closed-record Debug formatting never prints buffered bytes.

### Tokenizer measurement method

The ignored `measure_tokenizer` test uses a 512 KiB drawing with 21 typed-valid
records: a begin, a step creating 64 text objects, 18 steps updating their
400-character text, and an end. Trailing whitespace fills the exact drawing cap.
It runs five warm-ups and 64 timed drawings for each of 40-byte and 4 KiB deltas.
Reported MB/s uses decimal megabytes; ns/delta includes draining emitted records.
Fixture construction and initial buffer allocation are outside the timed region.
A separate thread-local counter checks allocation, zeroed allocation and
reallocation, including an instrumentation sanity test; it observes only the
calling test thread. Main had no drawing tokenizer, so the before baseline is
“none,” not an invented speedup.

Build the release tests below normal priority before acquiring the shared
measurement lock. On Rez, after 30 seconds without cargo/rustc, run at normal
priority with the lock held:

```text
nice -n 10 cargo test -p capsula-scene --release --no-run
cargo test -p capsula-scene --release -- --ignored --nocapture --test-threads=1
```
