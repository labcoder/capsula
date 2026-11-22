# Tuning

Tuning is an advanced tool: a `[tuning]` table in `config.toml` overrides some of Kumochi's
built-in limits. Most people never need it. Its keys may change or go away between versions,
and a key a version doesn't know is ignored.

`config.toml` is in `%APPDATA%\Kumochi` (`%APPDATA%\Kumochi Dev` for debug builds). Kumochi
never writes a `[tuning]` table itself, and it keeps yours when it saves a setting.

## Changing a value

1. Quit Kumochi. A setting saved while it runs writes the whole file, so an edit made while
   Kumochi is running can be lost.
2. Edit `config.toml` and save it.
3. Start Kumochi. Values are read once, at start.

Each value has a range. A value outside it is moved to the nearer end, and a value that isn't
a whole number is ignored. Values in one group that don't fit together are all ignored, and that
group uses its defaults. Kumochi still starts, and its log names the key it didn't take, never
its value. Don't put secrets or anything personal in this table. If the file isn't valid TOML,
for example because a key appears twice, Kumochi runs on its defaults and refuses to save
settings until you fix the file; see [settings](settings.md).

## GitHub

These limit what Kumochi asks of your GitHub account while it files an issue or lists your
repositories. They can only be made tighter, which can make filing fail sooner on a slow
connection or with many repositories.

```toml
[tuning.github]
# Recording the outcome on this computer may finish after this limit, so a sent issue isn't lost.
# 20–60 seconds: the longest filing an issue, or saving a review, may take, retries included.
filing_s = 60
# 12–16: requests one filing may make. At least listing_requests_count + 4.
filing_requests_count = 16
# 20–60 seconds: the longest listing your repositories may take. At most filing_s.
listing_s = 60
# 4–12: requests one listing of your repositories may make.
listing_requests_count = 12
```

## Model replies

These are how long Kumochi waits on ChatGPT or an Anthropic API key at each step of a reply.
A shorter wait gives up sooner on a slow connection; a longer one keeps a stuck reply on screen
longer. Checking an API key keeps its built-in waits.

```toml
[tuning.model]
# 3–10 seconds: connecting and sending one request.
send_s = 10
# 15–240 seconds: the longest pause between parts of a reply. At most content_idle_s.
idle_s = 60
# 3–40 seconds: reading the explanation when a service refuses a request.
error_body_s = 10
# 250–4000 milliseconds: waiting for a completed ChatGPT reply to finish closing its response.
drain_ms = 1000
# 10–50 seconds: the maximum idle age at which a connection can be reused.
pool_idle_s = 50
# 15–240 seconds: Anthropic: how long a reply may wait for Kumochi to catch up.
backpressure_s = 60
# 150–2400 seconds: Anthropic: the longest a reply may go without a recognized reply event.
content_idle_s = 600
```

## Runs

These limit how many tool rounds or calls an answer may use, and how long it may take. The
rounds limit applies when Kumochi runs the tool loop for a direct provider. Native Claude Code
uses its own fixed turn limit. The counts below can only be made lower.

```toml
[tuning.runs]
# 2–8: tool rounds managed by Kumochi for a direct provider.
rounds_count = 8
# 2–16: tool calls one answer may make.
calls_count = 16
# 30–240 seconds: the time available to process an answer, separate from discovery.
wall_clock_s = 120
# 8–60 seconds: how long a message to Claude Code waits while Kumochi is still looking for it.
finding_wait_s = 30
```

## ChatGPT sign-in and tokens

These set waits for signing in, refreshing a session, and revoking old tokens. A refresh may
already have reached the provider when it times out; Kumochi doesn't send that same refresh
again. Revocation that doesn't finish within its budget leaves unconfirmed tokens for a later
run.

```toml
[tuning.oauth]
# 75–300 seconds: the local sign-in window, including sending the result to your browser.
sign_in_s = 300
# 8–30 seconds: HTTP setup and one token refresh.
refresh_s = 30
# 3–40 seconds: revocation's HTTP setup and network budget, including both retry waits.
revoke_s = 10
# 125–2000 milliseconds: the wait before a first revocation retry.
first_backoff_ms = 500
# Both waits together must be shorter than revoke_s.
# 250–4000 milliseconds: the wait before a second retry, at least as long as the first.
second_backoff_ms = 1000
```
