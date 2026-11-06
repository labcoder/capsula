# GitHub

## P2a: the checked HTTP boundary

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
cannot be mixed independently. Device endpoints accept only their typed form
inputs and have no bearer. REST requests use the fixed product User-Agent,
`application/vnd.github+json`, API version `2026-03-10` and a checked `ghu_` bearer.
Only issue creation carries JSON. Nonempty legacy form fields are refused.
Every constructed GitHub URL is checked again for its exact host and path,
HTTPS/443 and absence of userinfo or fragments. Installation, repository and issue pages fix
`per_page=100`; page numbers are restricted to one or two. Link URLs aren't used.

The transport collects at most 4 KiB for device replies, 64 KiB for `/user`, and
1 MiB for other REST replies. The 1 MiB limit applies even to a page of 100 large
issues: a future preflight must fail without sending if the page exceeds it, and
a post-send lookup must remain Unknown. Never raise this cap or omit malformed
rows to make a lookup appear complete. The future API layer must apply these
caps again to injected `Http` responses and bound both match pages together.

`ResponseMeta` retains only parsed Date, Retry-After and rate-window numbers.
Duplicate, oversized, malformed and overflowing headers become absent, not zero.
HTTP-date Retry-After requires a valid server Date; no local-clock replacement
is made. Issue-create 4xx replies keep their definite status and metadata without
reading diagnostic bodies. Body failures after headers never become Connect.
Per-request deadlines include the response body: 15 seconds for device, user,
installation-page and repository-page calls, 30 seconds for create and issue-page calls.

The typed device/token parsers, create-outcome classification, two-page exact
matching and their outer Tokio deadlines are a separate API-layer slice. It must
refuse refresh credentials until refresh is supported. P3 owns polling, account
generations and repository aggregation, including a total installation traversal
bound; a two-page limit on each list alone does not bound that traversal.

Public contracts pinned for this policy:
[device flow](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-user-access-token-for-a-github-app#using-the-device-flow-to-generate-a-user-access-token),
[repository issues](https://docs.github.com/en/rest/issues/issues?apiVersion=2026-03-10),
and [user installations and repositories](https://docs.github.com/en/rest/apps/installations?apiVersion=2026-03-10).
