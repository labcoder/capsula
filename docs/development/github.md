# GitHub

## Configuration and reachability

Settings → Accounts contains the GitHub connection workflow. The public
`GITHUB_CLIENT_ID` in `crates/protocol/src/constants.rs` names the registered
Kumochi Dev app in debug builds. The app enables device flow and expiring
user-to-server tokens. Release builds leave the client ID unset: Connect is
refused without constructing HTTP or starting GitHub credential work. An App ID,
another application's token or an environment variable is not a replacement.
Repository access still requires installing the app for those repositories.

With a configured client, the runtime executes device sign-in, verification,
credential settlement, repository refresh, durable default selection and sign-out.
Draft production, issue consent reception, IssueEdit and filing remain dormant;
this account workflow grants no permission to send an issue.

Settings retains the session of a Connect it successfully queued, including before
the first view arrives. Every panel dismissal cancels that interactive attempt
through the existing CancelConnect control, including verification and an admitted
save. A full/closed cancellation queue keeps the send-failure state and ends that
link so disconnect applies the same fence and queued compensation; the visible
Cancel button instead stays retryable. Saved-account startup checks and automatic
renewal have no panel-owned provenance and are not canceled by dismissal.
Deactivation only updates focus: switching to the browser leaves Settings open.
The brain's durable Connected transition is completion. A later CancelConnect is
ignored and EOF preserves that credential, even if its view has not reached the
shell; reopening Settings shows it and Sign out removes it.

## Shell snapshots

The shell retains session-tagged GitHub snapshots in Settings. Replacement and
disconnect clear them; stale-session snapshots do not change them. Each accepted
scope transition replaces a strong origin identity before observers run, so a
batched loss and return cannot revive an earlier intent. Identical snapshots preserve
that identity. Settings callbacks have a separate identity, retired by every changed
view or error; an incomplete-list transition therefore retires repository choices
without changing the consent scope. Current repository membership must still be
checked at activation, without treating a truncated list as proof that an unlisted
repository is available.

Live GitHub controls route only to the current account owner. Missing client
configuration refuses Connect with AccountMisconfigured; other unavailable
operations retain the tagged unavailable error. Errors reach the current session's
GitHub state without changing generic Accounts errors. An error followed by its
authoritative view retains the error; a later superseding view may clear it.
Offline controls leave no reply owed to a replacement session. Full or closed
control output ends the live connection instead of retrying the refusal.

The Settings section uses this same model for commands, repository selection and
the fixed verification-page Link. Browser activation rechecks the current session,
origin and code deadline. Fixture windows never open a browser. Verified numeric
account authority and Store admission remain prerequisites for a future issue
consent receiver; a login in a snapshot is not that authority.

## The checked HTTP boundary

GitHub uses the account HTTP client: manual OS trust roots, HTTPS, no proxies, no
redirects and no automatic retries. Its raw reqwest client stays inside
`connections/http_client.rs`. `connections/github` owns checked, transient endpoint
inputs, not a connection session, credential lookup or persistent state.

Two client rules cover every account call, not only GitHub's. Retries are pinned
off with `retry::never()`, which is inert while the build has no HTTP/2 or HTTP/3
and keeps a create from being resent if that changes. A failure while reading a
body is Timeout or Transport, never Connect, because account classification reads
Connect as "nothing was sent, so no tokens were issued". Bearer headers are built
in a wiped buffer and marked sensitive.

```mermaid
flowchart LR
    Caller --> Request[Request: endpoint + legacy fields]
    Request --> Policy[Closed GitHub endpoint policy]
    Policy --> Client[Checked HTTP client]
    Client --> Reply[Bounded body + parsed numeric metadata]
```

Each endpoint owns its credentials and payload, so methods and authentication
cannot be mixed independently. Device and refresh endpoints accept only their typed
form inputs and have no bearer or client secret. REST requests use the fixed product User-Agent,
`application/vnd.github+json`, API version `2026-03-10` and a checked `ghu_` bearer.
Only issue creation carries JSON. Nonempty legacy form fields are refused.
Every constructed GitHub URL is checked again for its exact host and path,
HTTPS/443 and absence of userinfo or fragments. Installation, repository and issue pages fix
`per_page=100`; page numbers are restricted to one or two. Link URLs aren't used.

The transport collects at most 4 KiB for device replies, 64 KiB for `/user`, and
1 MiB for other REST replies. The 1 MiB limit applies even to a page of 100 large
issues: a future preflight must fail without sending if the page exceeds it, and
a post-send lookup must remain Unknown. Never raise this cap or omit malformed
rows to make a lookup appear complete. The API layer applies these caps again to
injected `Http` responses; the issue layer also bounds both match pages together.

`ResponseMeta` retains only parsed Date, Retry-After and rate-window numbers.
Duplicate, oversized, malformed and overflowing headers become absent, not zero.
HTTP-date Retry-After requires a valid server Date; no local-clock replacement
is made. Issue-create 4xx replies keep their definite status and metadata without
reading diagnostic bodies. Body failures after headers never become Connect.
Per-request deadlines include the response body: 15 seconds for device, user,
installation-page and repository-page calls, 30 seconds for create and issue-page calls.

Create-outcome classification and two-page exact matching belong to the
`github/issues.rs` layer. The account layer owns polling, account
generations and repository aggregation, including a total installation traversal
bound; a two-page limit on each list alone does not bound that traversal.

Public contracts pinned for this policy:
[device flow](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-user-access-token-for-a-github-app#using-the-device-flow-to-generate-a-user-access-token),
[repository issues](https://docs.github.com/en/rest/issues/issues?apiVersion=2026-03-10),
and [user installations and repositories](https://docs.github.com/en/rest/apps/installations?apiVersion=2026-03-10).

## Typed device and account reads

`connections/github/api.rs` provides six stateless calls: request a device code,
poll once for a token, rotate a device-flow refresh token, read the user, and read
one installation or repository page.
Each injected `Http` future has an outer Tokio deadline equal to its transport
deadline, 15 seconds for these calls. Dropping the
API future drops the request future; no task or retry survives its caller.
The account layer owns polling intervals, attempt generations, persistence and sign-out.

Parsers reuse each endpoint's transport limit, so scripted clients get it too:
4 KiB for device/token responses, 64 KiB for the user, and 1 MiB for installation
and repository pages. A non-success status is refused before the size check, so a
known 4xx with a huge body stays definite. Trailing data after the document fails.
Known duplicate fields, invalid identifiers, missing or mistyped required values,
and pages over 100 rows fail closed. Token replies reject unknown fields; other
account reads ignore unknown fields. These calls do
not follow Link headers or aggregate across pages or installations.

Device replies must name the fixed verification page, contain a checked user
code, and have positive lifetimes and intervals no greater than 900 seconds.
Only a missing interval defaults to five seconds; null and malformed values fail.
Token replies require a complete expiring pair: a checked `ghu_` bearer, a checked
`ghr_` refresh token, `bearer` token type, no nonempty scope, and positive returned
lifetimes bounded locally to one day for access and 366 days for refresh. These are
upper bounds, not exact service-contract integers. Each returned duration is added
to request start, rounded down to Unix seconds, with checked arithmetic. The two
expiries may be equal or refresh may expire first; persisted pairs retain positive
expiries and bounded, overflow-safe spans without imposing their order.
Non-expiring, partial, null, over-bound, overflowing, mixed-error and unsupported
replies are refused. Device errors may use 200 or 400; only 200 can issue credentials.
Refresh rejects a reused refresh credential and makes no automatic retry, including
after a lost response.

[GitHub's refresh contract](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/refreshing-user-access-tokens)
explicitly exempts device-flow tokens from the client-secret requirement. The
refresh form therefore contains only client ID, refresh grant type and refresh
token. Both credentials stay in Secret/zeroizing storage, never config, logs or
command lines.

`ApiError` retains only closed kinds, numeric status and parsed metadata. Its
shared definite-reason classification treats pre-dispatch/connect refusals as
Network and known 4xx responses as definite; throttled 403 and 429 are RateLimited.
An unusable but present Retry-After still makes 403 RateLimited, without inventing
retry advice. API refusals cap usable Retry-After advice to one hour (3,600 seconds).
Timeout, transport loss, redirects, malformed success and unexpected non-4xx
statuses remain uncertain. Hand-written Debug never prints credentials, user
codes, logins, repository names or provider text. The account owner is the caller;
the APIs themselves still own no session, task, timer or persistence.

## Single-attempt issue creation and exact matching

`connections/github/issues.rs` borrows checked `IssueTitle` and `IssueBody` through
`IssueContent`. Only that content can construct the private-field `IssuePayload`
used by the public create endpoint. Serialization writes just title and body into
a zeroizing buffer; arbitrary bytes and additional issue fields have no constructor.
`create_issue` makes one HTTP call under a 30-second outer deadline, with no retry.
Created requires a 201, a positive issue number and the exact canonical URL built
from the requested repository and number. Malformed or oversized 201 replies,
foreign URLs, unexpected non-4xx statuses and post-dispatch transport failures
are Uncertain. Pre-dispatch refusals and Connect are definite Network failures.
Known 4xx responses use the shared closed reason table and retain only numeric
status and parsed metadata, with usable retry advice capped at one hour.
`rate_reset_s` stays uncapped as absolute timestamp data; this layer derives no wait from it.

`list_matches` makes at most two GETs under one shared 30-second deadline. It
compares remote title/body exactly, without applying local authoring normalization
or bounds. A null body compares as empty; any `pull_request` field, even null,
excludes that row. Missing, mistyped, duplicate or malformed known fields, invalid
timestamps, duplicate issue numbers and pages over 100 rows fail the whole lookup.
Returned candidates contain only checked numbers, locally built URLs and creation
seconds, plus unchanged page-one metadata. A full second page is incomplete even
when every row was filtered out; no Link or third page is followed.

Parser-entry caps enforce 1 MiB even for injected clients. A large remote page
therefore fails the lookup that runs before a create, so nothing is sent, or
leaves the lookup after a send Unknown; callers must require completeness.
Dropping either API future drops its active request future. No task survives
cancellation. The action coordinator owns consent, durable admission, pre-send
exclusions, generation checks and placement; this layer never resends or
substitutes the local clock for missing server evidence.

## Device sign-in machine

The sans-I/O device machine owns one attempt and one connection generation.
It emits work rather than starting requests, timers, browser launches or
credential jobs. The runtime adapter must supply completions with their original
generation stamp, which identifies both an attempt and its connection. A changed
stamp, expired deadline or terminal state
makes a late completion inert. Sign out advances the shared generation before
emitting cancellation and serialized removal, so token consumers can fence
their final dispatch. The adapter must preserve queued saves when their result
receiver closes, and drain removal behind them. Cancel or expiry during a save also
queues removal, because that write may already have passed its fence check.

The first poll waits the code reply's interval. Each pending reply schedules
another wait; slow_down increases the interval by five seconds, takes the larger
of that and the reply's interval, and caps it at 900 seconds. The attempt ends
at its expiry even when a poll or identity read is outstanding. The API accepts
device error bodies on 200 and RFC 8628's 400; only 200 can yield a bearer.
The optional reply interval must be an integer from 1 to 900; null, duplicates
and out-of-range values refuse the whole reply, including a token reply.
Unknown codes and malformed replies end the attempt. No retry wait is derived
from REST rate metadata.

The approving identity is read before saving. During interactive sign-in, only a
successful save produces Connected. Startup checks a loaded bearer with the same
user endpoint; a 401 drops it and shows NeedsSignIn. Cleanup outcomes remain signed
out, including
a failed durable removal. A clean SignedOut view has no cleanup warning. Connect
is also permitted after a refused physical delete, because durable removal has
already fenced the old credential; Running and NotDurable still block it.
Repository defaults are displayed and accepted only by exact membership in a
complete fetched list, preserving GitHub's spelling. A persisted identity may
remain visible after a genuine startup API failure, but `user()`, bearer access
and repository authority stay unavailable until fresh verification succeeds.
A loaded identity cannot silently change numeric account ID; a renamed login
with the same ID is valid. Expired access or current-generation 401 revokes use.

Cancel during a restored identity check is not sign-out: it advances the fence,
aborts and joins that request, and retains the saved credential without another
request or renewal timer. The trailing wire variant `NotVerified { login }` has
tag 6 and exposes only the saved login. It grants no bearer, repository, default
selection or issue authority. Settings offers GET-only Check again through
RefreshRepos, plus Sign out. Only a successful explicit recheck restores use;
late success or failure from the canceled check cannot do so. A genuine
non-401 startup failure uses the same saved-but-unverified presentation.

An access token schedules rotation one minute before expiry, or halfway through
the remaining lifetime when less than two minutes remain. Every positive remaining
lifetime has a positive delay. An access token already unusable when restored-account
verification or a successful fresh-pair save completes ends in NeedsSignIn. A fresh
pair that expires during its identity check still gets its one credential save first.
An expired stored pair found at startup still gets one automatic recovery; an
expired fresh replacement cannot chain that recovery. The machine first advances
its fence, revokes consumers and saves `refresh_sent=true` on the existing
credential FIFO. Only that save's acknowledgment admits one refresh request. The
returned pair is verified against the same numeric account and saved atomically
with `refresh_sent=false` before Connected returns. Failed or lost rotation cannot
resend the old refresh token. Restart with a spent marker or expired refresh
credential shows NeedsSignIn instead. Explicit cancellation retires the rotation;
an ordinary shell replacement may let it finish and save.

Repository traversal shares one 60-second deadline and twelve-request limit across
all installations, with at most two 100-row pages per resource and 200 unique
repositories overall. Totals and page lengths must be consistent; duplicate
installation IDs, incomplete pages, errors and bound exhaustion grant no partial
authority. Names coalesce case-insensitively across installations while retaining
GitHub's spelling. A failed refresh clears repository authority.

The default is non-secret `[github] default_account`/`default_repo` metadata in
config.toml. Its numeric account ID is stored as decimal text, so the whole u64
range survives TOML. The existing ordered settings writer stages and replaces the
file. A successful write restores the machine's account-bound default metadata
from committed Config even if renewal or expiry retired the selecting operation.
Operation acknowledgment remains generation-fenced; display and use still require
the current numeric account and complete repository membership. A failed write
keeps the old default, and another numeric account cannot inherit it. Selecting a
default neither edits an explicitly bound draft nor files one.

The verification page is the protocol's VERIFICATION_URI constant. The API
checks the reply against it; neither the view nor a brain effect carries a
provider-supplied URL. The machine and protocol are separate from the runtime
credential adapter and the connection UI. The connection UI's Open GitHub action
and browser-opening behavior belong to the shell layer, not this machine.

```mermaid
stateDiagram-v2
    [*] --> Loading
    Loading --> SignedOut: no usable record
    Loading --> Verifying: saved account
    SignedOut --> Requesting: Connect when clean or Refused
    NeedsSignIn --> Requesting: Connect
    Requesting --> Awaiting: checked challenge
    Awaiting --> Verifying: token
    Verifying --> Saving: approving identity
    Verifying --> Connected: saved account verified
    Verifying --> NotVerified: cancel saved check or non-401 failure
    NotVerified --> Verifying: explicit GET-only Check again
    NotVerified --> SignedOut: Sign out
    Verifying --> NeedsSignIn: saved bearer refused
    Saving --> Connected: saved
    Connected --> MarkingRefresh: access renewal due
    Loading --> MarkingRefresh: access expired, refresh usable
    MarkingRefresh --> Refreshing: spent marker saved
    Refreshing --> Verifying: rotated pair
    Refreshing --> NeedsSignIn: failed or lost result
    Connected --> NeedsSignIn: current-generation 401
    Requesting --> SignedOut: Cancel or failure
    Awaiting --> SignedOut: Cancel, expiry or failure
    Verifying --> SignedOut: expiry or failed sign-in
    Saving --> SignedOut: failure or Cancel, then cleanup
    Connected --> SignedOut: Sign out, then cleanup
```

## Credential authority and lane

Accounts owns the device-machine adapter on the existing credential lane. A checked,
configured client identifier enables startup, effect dispatch and snapshots.
Debug builds use the Kumochi Dev registration; release builds keep Connect closed
until the release app is registered.
Ordinary account startup and its existing vault sweeps are unchanged; there is no
second account cache, credential worker or issue-consent receiver.

The owner loads only after the account worker opens its
vault. Unavailable admission, a closed reply or an unavailable vault leaves the
machine Loading, with no successful SignedOut snapshot. A genuine empty load and
the exact cleanup outcome remain distinct. Loading is queried from the machine,
not copied into another loaded flag. Device, poll, identity, refresh and repository
effects each retain their original stamp. A retiring construction cannot be
overwritten by a new operation. One independent stamped wake owns the current
attempt deadline or token-renewal time. Credential-only unit fixtures can hold
network work, but the configured production path dispatches it normally.

One network operation owns its client-construction or request handle,
original stamp and origin session. Its credentials stay outside the blocking
factory closure. The event loop joins the stored handle directly and retains
operation identity until disposition. An already joined completion may be retired
by cancellation; its later event is inert. A canceled construction is
joined; a canceled request is aborted and joined. An old event cannot replace a
new operation, and a retiring operation cannot be overwritten. The shared fence
is checked inside the request task before the API is first polled and before
each repository-page dispatch.

A real successful response supplies the current numeric identity without resaving
an already durable account. A real 401 means NeedsSignIn. Other genuine API/factory
failures retain only the persisted display identity and closed error; that is
not fresh verification or fetched repository authority. An owned task ending without
an API result uses a separate terminal input, never a fabricated transport failure
or Connected fallback. Late successful results cannot revive it. Current authority
may reach a replacement, but errors require the
matching nonempty load-origin session. Loaded verification survives ordinary EOF.

Save and Remove enter the existing FIFO before any view/error is delivered.
Interactive cancellation clears suspended work, not admitted credential jobs or
their reply settlement. Shell loss cancels an attempt rather than signing out an
already connected account. A replacement gets the current non-Loading view, not
an old error. Shutdown admits compensation before dropping the job sender, settles
credential replies and waits for the existing worker to release its vault lock,
then joins any remaining canceled verification before returning. A running client
factory is not made abortable by its handle; the API's 15-second request/body
deadline does not bound construction or shutdown. No new wall-clock shutdown
guarantee is made. An admitted refresh and its save may finish until the existing
shutdown refresh deadline. After it, either the committed successor or the old
spent marker survives; no old refresh is replayed. Canceled network handles remain
owned until joined.
The adapter borrows the sender and creates no second credential worker or account
cache. Numeric identity remains the machine's connected identity; fetched membership,
Store admission and separately reviewed activation remain prerequisites to filing.

Successful saved-account verification at startup and successful renewal both
start a fresh complete repository traversal. Each identity request has its existing
15-second request/body limit; each traversal shares 60 seconds and at most twelve
requests, with at most two 100-row pages per resource and 200 unique repositories.
These bounds apply to background refresh as well as the explicit Refresh command;
they do not bound client construction or promise one combined startup deadline.

GitHub records use the same credential thread, vault session lock and write-once
generation names as other accounts. The record contains the checked `ghu_`/`ghr_` pair,
both absolute expiries, the single-use refresh marker, a checked login and a
positive numeric user ID. JSON is limited to 2,048
bytes; missing, unknown or duplicate fields fail closed. Old non-expiring records
are not silently upgraded into valid credentials. Jobs and replies have content-free
Debug output. A save checks the device machine's shared generation immediately
before writing. Closing its reply receiver never stops the credential thread or
discards a later removal; an admitted save finishes before that removal runs.

Authority schema v3 adds the `github` head. Upgrades rebuild only `heads`, with
foreign keys disabled before the transaction and checked before commit, then
enabled again. Existing generations and indexes are retained. Older binaries
refuse the whole v3 authority, including other accounts; they do not delete it.
Live testing must isolate application data and coordinate the credential service
instead of migrating an existing Dev authority silently.

Remove first commits durable retirement, then deletes the retired generation's
names. Load cleans retired GitHub rows without retiring a valid active record.
Explicit cleanup queries every removed GitHub row, including deletions attempted
in this process, so a refused delete can be retried immediately. Physical absence
is reported as clean, but the authority marks a row Swept only after another
process lifetime confirms absence. A failed retirement commit is NotDurable;
remaining names are Refused.

A whole malformed or oversized record is retired and cleaned. A failed or
missing chunk read instead refuses the load and leaves the active generation
unchanged, because a transient credential-store glitch is not sign-out. A vault
that could not open likewise returns a typed refusal. The runtime orchestration
and user-facing connection controls remain separate from this credential layer.
