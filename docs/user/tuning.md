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
