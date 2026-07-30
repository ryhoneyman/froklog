# Branch: sqlite-time-model — change log

What this branch changes, what bugs it resolves, and the reasoning behind each design
choice. Baseline: commit `cd41160` (linux-integration).

Ground rules for the branch:
- Every change must build and function in both the Windows and Linux binaries.
- Existing on-disk journal data is intentionally **not** migrated. The server logs a
  notice when it finds an old-format journal it is ignoring.

---

## 1. Storage engine: embedded SQLite replaces the hand-rolled journal format

**Design choice.** Each stream now stores its data in a single SQLite database file
(WAL mode, compiled into the binary — no external install, no service, no DLL).
Batches are still zlib-compressed JSON, now stored as rows instead of records in a
custom binary file. Sessions and the new markers table live in the same database.
The in-memory seek index that the viewer relies on is kept exactly as before, so the
whole viewer replay/seek state machine did not have to change.

**Bugs this resolves by construction (not by patching):**
- *Silent history corruption after a dirty shutdown.* The old format could detect a
  half-written record on startup but never repaired it; new data was appended after
  the garbage, and on the following restart everything past the damage was silently
  misread. With WAL, an interrupted write simply rolls back — this failure mode no
  longer exists.
- *Slow startup on large journals.* The old startup had to read every byte of every
  journal to rebuild its index. The index is now a single small query, so startup
  cost no longer grows with journal size.

**Why SQLite and not a "real" database:** it's a library, not a server — zero
operational burden, one file per stream, and at this system's write rate (about one
batch per second live) it is nowhere near any limit. It also *removes* code: the
custom record format, its header packing, and the dual-format legacy reader are gone.

## 2. Sessions now anchor to permanent batch ids instead of list positions

**Design choice.** A session used to remember "my first batch is the Nth record in
the journal." Once pruning can delete old records, positions shift and every session
boundary after a prune would point at the wrong place. Sessions now remember the
permanent id of their first batch (ids are never reused), and positions are computed
on demand. This is the change that makes retention/pruning safe at all.

## 3. Seeking by game time is now correct even when history is out of order

**Bug resolved.** Jumping the viewer timeline to a game-time position assumed the
journal was sorted by game time. That's only true for a well-behaved live clock —
importing old logs into a stream that already has newer data, or a client clock
stepping backwards, breaks the assumption, and the old binary search would then land
somewhere arbitrary (wrong seek target, wrong session scoping) with no error. The
seek now walks the index in journal order and stops at the first match. The index is
small enough that this costs microseconds; correctness was the only thing at stake.

## 4. Pruning support in the storage layer

**Design choice.** The journal can now delete everything older than a cutoff
timestamp and reclaim the disk space, and the session list cleans itself up to match
— the newest session that started before the cutoff is re-anchored to the oldest
surviving batch, so every remaining batch always belongs to a session. A `markers`
table (raid/group start–end time slices) is part of the new schema; the endpoints
that use it come as a separate change. Endpoint wiring and the optional automatic
retention sweep are also a separate change.

## 5. One import can no longer freeze the whole server

**Bug resolved.** While a batch was being written to disk, the ingest path held a
server-wide registry lock. Because that lock queues fairly, one big history import
(thousands of sub-batches written in one loop) blocked every viewer page, every
WebSocket upgrade, and the admin panel until it finished. The ingest path now grabs
the per-stream handles it needs, releases the shared lock immediately, and does its
disk work without holding anything global.

## 6. Prune endpoint and optional automatic retention

**Design choice.** Two ways to prune, one deliberate exclusion:

- `POST /stream/<id>/prune` with `{"days": N}` (or `{"before": <unix_ts>}`)
  deletes everything older than the cutoff and reclaims the disk space.
  Authenticated with the **stream token** (the owner credential the pushing
  client holds) or the admin token. The shareable **view token cannot prune**:
  a view link is meant to be handed out, and anyone holding it must be able to
  watch but never destroy history. So yes — each member can prune their own
  data, using the same credential their client already has, without needing the
  admin.
- `retention_days` in the server config (default 0 = off) turns on a daily
  sweep that applies the same cutoff to every stream automatically. This also
  closes the "unbounded disk growth" gap: before this branch there was no way
  to cap a stream's size at all short of wiping it.

**Why days-based:** the cutoff compares against event log timestamps, so "keep
90 days" means 90 days of game history regardless of when it was uploaded.
(Until the true-epoch time change lands, log timestamps can be offset from
server time by the streamer's timezone — irrelevant at day granularity.)

## 7. Event timestamps are now true epoch, with an NTP-style server clock probe

**Bug resolved / design choice.** EverQuest stamps log lines with the machine's
local wall-clock and no timezone marker. froklog used to store that reading
as-is, pretending it was UTC ("fake UTC") — which preserved the streamer's
clock on screen but meant stored timestamps were hours off from real time,
differed between streamers in different timezones, and repeated a full hour
every DST fall-back. Timestamps are now converted to **true unix epoch** on the
client, in two layers:

1. **Timezone conversion** (the big correction — hours): the client looks up
   the machine's IANA timezone and applies the offset *for the date being
   converted*, so history imports from last winter get last winter's DST rule.
   The IANA database is compiled into the binary specifically so Windows and
   Linux convert identically — Windows' own timezone APIs apply current-year
   rules to historical dates, which would have made the two platforms disagree
   by an hour on old imports.
2. **Server clock probe** (the small correction — seconds, usually zero): on
   connect the client sends a hello over the existing ingest WebSocket carrying
   its UTC offset and a timestamp; the server echoes it with its own clock, and
   the client computes its skew NTP-style (one round trip, half-RTT accuracy).
   This corrects machines whose clock is simply set wrong, without running an
   actual NTP server — no new port, no new protocol, ~40 lines total.
   Sub-second skew is deliberately ignored: log lines have 1-second resolution.

**Display contract change:** the viewer page used to force UTC rendering (to
show the fake-UTC value = streamer's clock). It now renders in the *viewer's*
local timezone — the streamer still sees their own local times, and remote
viewers see "when that happened in my time." Session labels keep the
*streamer's* calendar dates: the client-reported UTC offset shifts the label
date, so an evening raid doesn't get labeled with the next day's UTC date.

**Compatibility:** an old client against this server just never sends a hello —
everything works, labels fall back to UTC dates. This client against an old
server gets one harmless "invalid EventBatch" log line for the hello and no
skew correction — timezone conversion (the important part) still applies.

**Also fixed here:** the pusher previously dropped the WebSocket's read half
entirely, so server pings were never answered and dropped connections went
unnoticed until the next send — during a quiet stretch that lost the next
event. The probe needed the read half anyway; the pusher now polls it, which
answers pings (keeping proxies happy) and detects closes promptly.

Verified end-to-end against a live server with a synthetic log: stored
timestamps match the independently-computed true epoch exactly (old code was
4 hours off on this machine), skew probe reports 0 ms on a synced clock,
prune-by-cutoff deletes and reclaims, and a pruned journal reloads with correct
retroactive session boundaries after a restart.

## 8. Server hardening — five bugs resolved

- **Stored XSS on viewer pages.** The player name (and other client-supplied
  values) were pasted into the viewer HTML/JavaScript without escaping — and
  stream creation can be unauthenticated, so anyone could register a "player"
  whose name was a script and have it run in other people's browsers. All
  template values are now escaped for the context they land in (HTML for the
  badge, JS-string for the constants).
- **Rate-limiter identity spoofing.** The server believed `X-Forwarded-For`
  from any connection, so anyone reaching the port directly could present a
  fresh fake IP per request and bypass rate limits and bans entirely. New
  `trusted_proxy` config: when set, forwarded headers are honored only from
  that address. (Left empty it behaves as before, for setups that rely on it —
  set it in any internet-facing deployment.)
- **Allocation bomb from one bad timestamp.** A batch containing one corrupt
  timestamp next to a sane one made the batch-splitter allocate one bucket per
  60-second window across the whole span — a single mangled line could request
  tens of millions of allocations. Bucket count is now capped (~two weeks of
  log time per batch); outliers fold into the last bucket.
- **Panic race on stream deletion.** Deleting a stream at the exact moment a
  viewer was connecting could crash that connection's handler on an
  "impossible" lookup. It now returns 404.
- **Silent admin lockout on config typo.** A malformed server config was
  silently replaced with defaults — including a freshly generated random admin
  token — with no message, because the warning was logged before logging was
  initialized. Warnings are now carried past logger startup and printed, and
  an existing (possibly just-mistyped) config file is never overwritten with
  defaults.
- Also: updating a stream's public flag no longer silently loses its
  "recorded replay" status across a server restart.

---

*(subsequent changes appended as they land)*
