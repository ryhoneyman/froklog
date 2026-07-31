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

## 9. Client robustness — five bugs resolved

- **A single accented character no longer kills the tailer.** EQ logs are
  Windows-1252, not UTF-8; the tailer's strict-UTF-8 line reader panicked on
  the first non-ASCII byte — silently losing data in the tray build, and
  hanging `froklog-replay` forever. Lines are now read as raw bytes and
  decoded tolerantly (a bad byte renders as � in chat text; combat parsing is
  unaffected). Verified by injecting a 0xE9 byte into a replay log: identical
  parse results, no crash.
- **Deleting/recreating the log no longer silences the client until restart.**
  A common EQ habit is deleting the log file; the game recreates it, but the
  tailer kept reading a position past the new end forever. It now detects the
  size regression (a portable check — works the same on Windows, which has no
  inodes) and reopens from the start.
- **Half-written lines are no longer emitted as fragments.** A line the game
  was still flushing at the moment of the read could be delivered as two
  broken halves; the tailer now waits for the newline before emitting.
- **"Replay complete." now means complete.** The replay tool used to wait for
  the file reader, sleep two seconds, and declare success — a big import could
  still have unsent batches in the pipeline. It now waits for the pusher to
  drain everything. Relatedly, events discarded while the server is
  unreachable are now counted and logged instead of vanishing silently.
- **The headless (Linux) client can recover.** If its engine stopped (e.g. the
  tailer exited), the non-tray build slept forever as a zombie; it now
  restarts the engine in a loop, mirroring what the Windows tray build already
  did.
- **A corrupt client config is preserved instead of destroyed.** A TOML typo
  used to silently reset all settings — server URL, stream tokens, layout —
  and the next save overwrote the file. The broken file is now backed up
  (`config.toml.broken`), the error is printed, and the duplicated
  default-construction paths were consolidated so future code can't construct
  a half-wrong default config.

## 10. Time markers — raid/group slices

**Design choice.** Streams get user-defined time markers ("raid_start",
"raid_end", "group_start", …, plus a free-form label), stored alongside the
batches. The owner (stream token, or admin) sets and deletes them —
`POST /stream/<id>/marker` (timestamp optional; defaults to now, which is in
the same true-epoch domain as event timestamps) and
`DELETE /stream/<id>/marker/<mid>`. Anyone with view access can list them via
`GET /stream/<id>/markers`.

Viewing a slice reuses the machinery the session filter already had: the
viewer page accepts `?from_ts=&to_ts=` (log-time epoch seconds) and scopes the
stats to that window — so a raid view is just the stream URL plus the two
marker timestamps. Markers are pruned with the data they annotate by the
retention sweep and manual prune.

Deliberately not in this change (build on the API later): marker buttons in
the viewer UI, and client-side auto-markers from an in-game phrase. Both sit
cleanly on top of these endpoints without storage changes — this was the
"we can always do this after the data is indexed in SQL" part, and the index
made it a ~150-line feature.

## 11. Graceful WebSocket close — imports no longer lose their final seconds

**Bug resolved (found during deployment testing, not in the original review).**
When the pusher finished (end of an import), it sent the last batches and
immediately dropped the socket; the process then exited. Over localhost that
worked; through a TLS reverse proxy the final frames were still sitting in
buffers when the process died — a production replay stored 282 of 287 batches,
silently missing the last ~37 seconds of game time. The pusher now performs a
real WebSocket close handshake and waits (up to 5 s) for the server's close
acknowledgment — which, by TCP ordering, proves the server processed every
frame sent before it. Re-tested through the proxy: exact batch parity with the
local run. Send failures during the final flush are also logged with the event
count instead of being ignored.

## 12. Parser accuracy — found by tracing live gameplay against the viewer

All of these were diagnosed by correlating a live fight's raw log lines with
the events the client shipped and what the viewer rendered.

- **One mob no longer shows as two.** EQ capitalizes a mob's name when it
  opens the sentence ("Orc taskmaster hits YOU") but not mid-sentence ("You
  slash orc taskmaster"). Mob-instance matching was case-sensitive, so every
  fight split into two phantom instances — one holding the player's damage,
  the other the tanking — which also broke the viewer's current-fight
  auto-selection (it flapped between two half-empty mobs). Identity matching
  is now case-insensitive (slay/loot/dead-tracking included), with the
  mid-sentence lowercase form preferred as the display name.
- **Player-side misses were never parsed.** The miss regex's `tries?` matches
  "trie(s)" but not "try" — so every "You try to slash X, but miss!" line
  (~500 in one day's log) was dropped, and accuracy/avoidance stats only
  counted the mob's whiffs, never yours. Fixed to `tr(?:y|ies)`.
- **Mob self-heals were invisible.** "an orc thaumaturgist healed itself for
  11 hit points by Lifespike." never matched (the healer name pattern allowed
  no spaces). It now parses; the mob's encounter window stays open while it
  heal-turtles, but it is deliberately not emitted as a Heal event so the
  viewer's healer lists remain player-only.
- **Incoming damage-shield burns now count as damage taken.** "YOU are burned
  by orc centurion's flames for 6 points of non-melee damage!" (75 hits in one
  day) matched nothing — fighting a fire-shielded mob undercounted your
  incoming damage. Now attributed to the mob's instance as tanking damage.
- **Encounters are deterministic.** The 15-second same-mob window used
  wall-clock time, so a fast import grouped encounters differently than live
  play of the same log. It now uses log-time deltas: the same log always
  produces the same encounter boundaries, live or replayed.

## 13. Viewer: current-fight DPS in the fight header

The fight header showed total Damage and Duration but never the division.
It now shows **DPS over the fight window** — for a live fight the viewer
already auto-selects the active mob, so this is the "current DPS" readout;
for a reviewed past fight it is that encounter's DPS. (The session-wide table
still shows session DPS — that number decaying over idle time is what made
"current DPS" look missing.)

## 14. Viewer: new fights no longer merge into stale list entries

**Bug resolved (user-reported: "I pulled 6 NPCs and they compounded damage
into old entries down the list instead of appearing as a new fight on top").**
The client parser numbers mob instances 1, 2, 3… per process run, and those
ids restart whenever the client restarts. The viewer keyed all state by raw
id, so after a restart the next fight's "mob 5" silently merged into whatever
"mob 5" meant an hour earlier — inheriting its position far down the mob list
and compounding its damage totals. (A related pre-existing symptom: with the
old case-split bug, capitalized phantom instances could never be marked dead
— slay lines are lowercase — so one entry could absorb 30+ minutes of chain
fighting.)

Fix: the viewer namespaces mob ids by client run. The pusher's batch sequence
restarts at 0 exactly when the parser's ids do, so "seq 0 after anything
else" deterministically marks a new run; ids are remapped once at ingestion
(epoch × 1,000,000 + id), which keeps every downstream path — cache replays,
seeks, loot, links — untouched. New runs now append their fights at the top
of the list with fresh totals, including in historical data.

## 15. Encounters: pulls are now the reporting unit

**Feature (user-requested: "I pulled 5 mobs, fighting all as a unit — report
for the entire life of the encounter").** The viewer derives encounters by
chaining mob instances whose activity windows overlap or follow within 15
log-seconds, with a corpse-boundary cut: when every mob in a pull is dead, the
next fresh mob starts a new encounter even inside the gap — so chain-pulling a
camp slices into individual pulls instead of one endless block.

- The mob list groups into pull blocks (newest first): a header row with the
  composition ("orc legionnaire ×2, royal guard"), mob count, and duration,
  with member mobs indented beneath.
- Selecting a pull (click the header) scopes everything to it: combined
  Damage, DPS over the pull's own duration, per-player breakdowns on every
  tab, and merged loot for the whole pull.
- In live mode the viewer auto-follows the current pull, so the headline
  number while fighting five mobs as a unit is the unit's DPS.

**Honest limitation, verified against the game's own Extended Targets window:**
EQ log lines carry no individual identity, so N same-named concurrent mobs
pool their per-individual attribution (a real 5-mob pull of 3 legionnaires +
2 guards captures as 3 instances until deaths separate them — each corpse
closes an instance, so counts converge to truth as the pull dies). The
encounter's combined damage, duration, and DPS are exact regardless. Known
refinement still open: mez-parked mobs idle >15s can split a pull until
crowd-control lines are parsed into a "parked" state (designed, not yet
implemented).

## 16. Encounter refinements — driven by live play-testing with the user

Three rounds of "this is one encounter" feedback against real pulls exposed
and fixed the following, verified against the raw log (a 5-name, 3-minute
pull with max 3-second internal gaps that the viewer had been splitting into
fragments):

- **The real culprit: viewer case-flapping.** Events carry each log line's
  original casing ("Orc slaver" attacking, "orc slaver" attacked); the
  viewer's mob tracker treated a case flip on the same instance id as "id
  reused — reset," wiping the fight window on nearly every event. Fights
  showed 1-second durations and pulls shattered into blocks. Name comparison
  is now case-insensitive (lowercase preferred for display, matching the
  client). One warrior cleave pull went from 3+ fragments to a single
  227-second, 9-instance encounter after the fix.
- **Loot is the pull boundary, not a timer.** Between kills of the same pull
  you tab to the next target without looting; you loot corpses when the pull
  is over. The corpse-boundary cut now fires only when the pull is fully dead
  AND looting occurred — chain-killing never splits, looting-then-repulling
  always does.
- **Crowd control parks mobs** (from the earlier mez design, now
  implemented): "has been mesmerized/enthralled" lines register an unengaged
  add as a pull member the moment CC lands, suspend its idle/instance-gap
  timers (with a 5-minute silent-wear-off ceiling), and show a blue "mez"
  badge; "has been awakened by" (or any fresh combat line) releases it. New
  `cc` event type — client and server must deploy in lockstep (an old server
  rejects batches containing unknown event kinds).
- **Pull headers renamed** to "Pull #N · 9:57 AM" with count and duration;
  the composition list lives in the fight header and the indented member rows.

## 17. Combat heartbeats — stuns and out-of-mana no longer end an encounter

**User-identified gap.** A stunned or mana-dry player looks idle in the log,
and if the mobs are meanwhile beating on a merc/pet/tank (whose combat never
appears in this player's log), the whole log goes silent mid-fight — the
15-second gap would then split the encounter even though "if the mobs are
still hitting, the encounter is not done."

The player-state lines that prove combat is ongoing are now parsed as a
throttled (1/log-second) heartbeat event: "You are stunned!" / "no longer
stunned" / "can't cast spells while stunned", "Insufficient Mana to cast this
spell!", spell interrupts, concentration recovery, and life-force drain
(thousands of each in the real logs). Heartbeats extend an encounter's
effective window when chaining pulls, so a stun-locked stretch bridges the
gap; they never *start* an encounter (a stray out-of-combat interrupt is
harmless). New `hb` event kind — client and server deploy in lockstep, as
with `cc`.

## 18. Named-vs-trash classification

**Feature.** The game itself distinguishes named mobs from trash in its text:
trash names are lowercased mid-sentence ("You slash orc centurion"), named
mobs never are ("You slash Marrowbane"). The viewer keeps a session-level
registry of names ever seen in lowercase form; a name that stays capitalized
is a named. Named mobs render gold in the mob list, and an encounter
containing one becomes a starred header titled by the named
("★ Marrowbane · 10:41 AM") instead of "Pull #N" — which also gives boss
pulls a human name in the list. Self-correcting edge case: trash seen only
via sentence-start lines (e.g. it has only attacked, never been hit)
classifies as named until its first mid-sentence line arrives, then
reclassifies. Verified live: Chokehold, Bloodgurgler, and Marrowbane
encounters starred correctly across a real session.

## 19. Viewer layout polish

- **Resizable columns**: drag handles on the Mobs and Summary panels
  (120-800 px, widths persisted per browser via localStorage, double-click to
  reset). Verified across reloads.
- **Summary panel headers and true columns**: each data tab now renders a
  sticky header row (Player / Damage / % / DPS, labels following the tab),
  and rows share fixed grid tracks with the header so values align in real
  columns instead of drifting with name/class width. (Lesson recorded for
  the curious: separate CSS grids don't share max-content track sizing —
  every track except the flexible name column must be fixed.)

## 20. Encounter-centric header and replay

- The fight header now carries the same identity as the pull list: the named
  with its true casing ("★ Retlon Brenclog") or "Pull #N", with the
  composition as a small subtitle instead of being the title.
- The timeline bar is hidden while watching live; clicking a pull summons it
  scoped to that pull's own time window, with a replay-from-start control
  (↺). The scrubber, position labels, and far-right behavior all operate
  within the pull's bounds while scoped; Go Live or deselecting returns to
  the session view (and hides the bar again when live). Any non-live state
  keeps the bar visible so transport controls are never stranded mid-replay.

## 21. Live header, running fight clock, and scope-consistent cards

- While live, the fight header always shows the current pull — or, between
  fights, the most recent one — instead of going empty.
- An active pull's Duration and DPS tick in real time (a 1-second live ticker
  advances them between combat lines, in the header and the pull list); the
  clock freezes at the recorded span once everything is dead.
- The combat cards (third panel) now respect the selected pull: per-player
  type/spell breakdowns are merged across the pull's members and rates are
  computed over the pull's own duration. Previously the cards showed
  session-wide totals under a pull-scoped header — 42K/284 DPS next to a
  header saying 2K/13 — which read as broken formatting when it was really a
  scope mismatch. Verified: card rows now sum to the header's totals.

## 22. Pull index dropdown and encounter graphs

- The static "Mobs" label above the mob list is now a **pull index dropdown**
  — every encounter listed newest-first ("★ Marrowbane · 11:37 AM" /
  "Pull #37 · 11:36 AM"). Choosing one selects it, scopes stats and the
  replay bar to it, and scrolls the list to its block; it stays in sync with
  clicks and live auto-follow, and only rebuilds when the pull set changes so
  an open dropdown never snaps shut.
- The **graph strip works for pulls**: member-mob timelines are merged per
  player over the pull's window, so cumulative damage and DPS-rate charts
  render for a selected encounter (previously the graph code only understood
  a selected mob and showed an empty strip under pull selection).

## 24. Encounter replay, completed

**User requirements: select a pull and replay it; drill into one mob of the
pull and keep the same replay.** Three fixes deliver both:

- **Play replays the pull.** With a pull selected while live, the Play button
  now starts the pull's replay from its first event (same as the ↺ control).
  Previously it just paused the live feed — the replay engine worked, but the
  natural button didn't drive it.
- **Re-streamed batches no longer corrupt state.** Replays/seeks make the
  server resend journal batches the viewer already caches; those resends were
  appended to the cache as duplicates (double-counting damage on any later
  cache rebuild, e.g. Go Live) and could double-bump the client-run epoch,
  spawning ghost mob entries. Resends are now recognized (seq + log-ts) and
  the cached, already-remapped copy is applied instead. Verified: a 5-second
  replay streams with zero new cache entries.
- **Mob drill-down inside a pull.** Clicking a member of the selected pull
  shows that mob's damage/tank/heal panels while keeping the pull's replay
  context — scoped bar, ↺, Play — so you can watch one mob's numbers through
  the pull's playback. Clicking a non-member mob leaves the pull as before.

## 25. Honest status semantics: client connection vs. Go Live

The top-right badge used to say "● live" whenever the WebSocket was open —
even while replaying, and regardless of whether the streamer was still
playing. It now reports exactly one thing, the **streamer's client
connection**: green "● client connected" / gray "○ client disconnected"
(covering completed recordings and mid-view disconnects via stream_done).
Returning to live is a separate control — a "⏵ Go Live" pill that appears
next to the badge only when the client is connected and the viewer isn't
watching live (it also lives in the timeline bar). Status is status; buttons
are buttons.

## 26. Named classification without a third-party lexicon

**Design question (from the dev): how do you know nameds with no common term
— and (from the user) how do you maintain that lexicon without leaning on a
non-authoritative wiki?** Answer: derive it from the game's own testimony,
in two layers, both computed from data the viewer already holds:

1. **Grammar (perfect recall):** EQ's text engine never lowercases a
   proper-named NPC mid-sentence; trash it lowercases. Validated across a
   192 MB raid log: every recognizable raid boss classified named —
   including "Innoruuk, the Prince of Hate", backticked names (Coercer
   T`vala), hyphens (Cazic-Thule) — with a one-line rule and no term list.
2. **Corpse concurrency (precision):** a named is by definition a unique
   spawn, so two same-name corpses within 90 seconds prove population > 1
   and demote the name for the session. This is what separates capitalized
   farm-elites (Innoruuk`s Chosen: 57 kills, twice in one minute; Cleric of
   Innoruuk: 86 kills) from true nameds — all 17 verified nameds, including
   The Tenderizer farmed 72 times across weeks, never died twice in a
   window. Kill *frequency* was deliberately rejected as a signal: farmed
   nameds are frequent; only simultaneity is disqualifying.

The wiki-lexicon idea was considered and set aside as non-authoritative and
operationally fragile; the log itself is the primary source. If a stubborn
edge case ever survives both layers, the clean escape hatch is a small
owner-curated override list server-side — not built until needed.

## 27. Historical data can't ban a mob from its own fight

**Bug resolved (user-reported live: "fighting The Prophet but current combat
is inaccurate").** The Prophet was missing from its own encounter. Root
cause chain: before the mob-self-heal parser fix landed, "The Prophet healed
himself … by Inner Fire" was journaled as a player Heal event; every session
replay recreated a "player" entry for The Prophet from that history; and the
viewer's player-guard then refused to track the real mob — even in brand-new,
correctly-parsed fights. Old data poisoned new truth.

Fix: mob-position evidence outranks heal-only ghosts. A "player" known ONLY
from casting heals (zero damage dealt, zero received, no /who class) that
appears in a mob position gets reclassified as the mob it is. Real healers
keep their protection (they take hits, receive heals, or have classes).
Verified live: The Prophet's encounter reassembled with full damage/tank
data the moment the page reloaded.

## 28. One Go Live control

Per dev feedback on the shared viewer: the timeline bar's Go Live button was
redundant next to the new pill by the connection badge — two identical
controls read as a glitch. The bar button (and its orphaned CSS/handle) are
removed; the pill is the single Go Live control, visible exactly when going
live is possible and you aren't already there.

## 29. Heal traffic is not playerhood evidence

Bug resolved: Emperor Crush vanished from the parse — classified as a
*player*. His priest adds healed him, and the heal-ghost rule from entry 27
treated received-heals as proof of playerhood, so the ghost survived and
every mob event for him was discarded. Mobs heal each other constantly;
heals in either direction prove nothing. The corrected rule: only damage
dealt from a player position or /who class data makes a name a player.
Verified live: the session rebuilt with 62 encounters including two
★ Emperor Crush pulls that previously didn't exist.

## 30. Owner-curated named NPCs

The named/trash heuristic (capitalization + corpse-count demotion) is
game-derived but not infallible, and the game offers no authoritative list —
so the stream owner's judgment now layers on top. Click the ☆/★ on any mob
row and choose: Always named, Never named, or Automatic. Overrides live in
the stream's SQLite database (`mob_overrides` table, name-keyed), so they
apply to every viewer of the stream and survive reloads.

Design choices: writes accept the VIEW token (curation happens on the viewer
page, which never holds the stream token) as well as stream/admin tokens;
the public tokenless page reads overrides (labels must match everywhere) but
gets no toggle. Deleting the row — not a third stored state — is
"automatic", so the table only ever holds actual decisions.

## 31. Pulls with more than one named

A pull holding two nameds (Ambassador D`Vinn + Emperor Crush pulled
together) was labeled with whichever engaged first, hiding the second.
Encounters now collect every named member; headers, pull rows, and the
playback dropdown show "★ A + B" (two spelled out, "+N" beyond). The
first-engaged named still anchors anything keyed on a single name.

## 32. Pet ownership

Summoned pets draw a random generated name on every summon (Labarer,
Gabann, Xobtik — a fixed syllable space both viewer and parser now share),
so a pet was recognizable as *a* pet but never as *someone's* pet; only
Beastlord warders ("X`s warder") carried their owner in the name.

The log offers a clean recurring signature: Burnout-family pet hastes are
castable only on the caster's own pet, and their landing is the visible
"<Pet> goes berserk." line. "Ruin begins casting Burnout" followed within
10 s by "Labarer goes berserk" proves Labarer is Ruin's — re-proven on
every rebuff, which is exactly what per-summon renaming needs. The parser
records the association (known_pets) and emits a new `pet {name, owner}`
event; warder detection now emits it too. Verified on a real session:
three independent Burnout pairs, all agreeing.

Display is association-first, attribution-unchanged: the viewer's Pet
badge becomes "Pet · Ruin" (tooltip "Pet of Ruin"), and damage rows stay
per-entity — folding pet damage into the owner's row is deliberately left
as a future option rather than assumed.

Second correlation for classes without a Burnout tell (enchanter, necro):
a player's pet-summon cast ("Lesser Summoning: Water", "Sisna's
Animation") followed within 60 s by the first attack of a
never-before-associated generated-name pet. Already-owned pets are never
re-owned by someone else's summon. Bug found en route: RE_CAST's spell
charset had no ':' — colon-named spells ("Lesser Summoning: Water") never
matched, so those casts produced no Cast events at all.

## 33. Player-card deep stats (collapsed by default)

Every damage card gains a "▸ more stats" toggle. Expanded, it shows what
the totals hide:

- **Per type/spell hit ranges** — hits, low / avg / high, and crit rate,
  from per-event aggregation (count, min, max, crit-flag) tracked on both
  the global and per-mob buckets, so the section obeys the same scope as
  the rest of the card (pull-merged stats widen ranges and sum counts).
- **Melee accuracy** — landed swings vs mob-avoided attempts with the
  avoidance breakdown ("Accuracy 78.4% — 156 landed / 43 avoided (dodge
  21, parry 12, miss 10)"). The attacker-side Miss events were already in
  the stream; the viewer just discarded them.
- **Fizzles** — new `fizzle {src, sp}` event (the parser deliberately
  skipped these before). EQ only logs your own fizzles, so the count
  appears on the streamer's card, with a per-spell breakdown.

Tanked cards get the same ranges for incoming hits. Expansion state
sticks per entity across the continuous re-render.

---

*(subsequent changes appended as they land)*
