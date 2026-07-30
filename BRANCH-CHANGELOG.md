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

---

*(subsequent changes appended as they land)*
