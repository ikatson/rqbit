# Live Torrent State Architecture

This document describes the shared state used during live torrent downloading, the invariants that should be maintained, and known issues.

## Key Data Structures

### 1. `ChunkTracker` (in `chunk_tracker.rs`)

Tracks piece/chunk download progress. Lives in `TorrentStateLive`.

| Field | Type | Description |
|-------|------|-------------|
| `have` | `BitVec` | Pieces fully downloaded and verified |
| `queue_pieces` | `BitVec` | Pieces needed but not currently being downloaded |
| `chunk_status` | `BitVec` | Per-chunk completion status |

### 2. `inflight_pieces` (in `TorrentStateLive`)

```rust
HashMap<ValidPieceIndex, InflightPiece>

struct InflightPiece {
    peer: SocketAddr,      // Which peer "owns" this piece
    started: Instant,      // When download started (for steal threshold)
}
```

Global tracking of which peer is responsible for downloading each piece. Used for:
- Preventing multiple peers from downloading the same piece
- Piece stealing (finding slow peers to steal from)

### 3. `inflight_requests` (in `LivePeerState`)

```rust
HashSet<ChunkInfo>  // aka InflightRequest

struct ChunkInfo {
    piece_index: ValidPieceIndex,
    chunk_index: u32,
    offset: u32,
    size: u32,
}
```

Per-peer tracking of which chunks have been requested from this peer. Used for:
- Knowing which chunks to expect from peer
- Detecting unexpected data ("peer sent us a piece we did not ask")
- Cleanup when peer dies

## Intended Invariants

### Piece State Invariant

A piece should be in exactly ONE of these states:

```
have[piece] = true                    → COMPLETED (verified)
inflight_pieces.contains(piece)       → IN_FLIGHT (being downloaded)
queue_pieces[piece] = true            → QUEUED (needed, waiting)
none of the above                     → NOT_NEEDED (deprioritized)
```

These should be **disjoint** - a piece should never be in multiple states simultaneously.

### Chunk-Piece Consistency

If `inflight_requests` contains chunks for piece P, then:
- `inflight_pieces[P].peer` should equal this peer's address
- OR the piece was just stolen (transient state during steal)

## State Transitions

### Normal Download Flow

```
QUEUED → IN_FLIGHT → COMPLETED
```

1. `reserve_next_needed_piece()`:
   - Finds piece in `queue_pieces`
   - Calls `reserve_needed_piece(p)` → clears `queue_pieces[p] = false`
   - Inserts into `inflight_pieces[p] = (self, now)`

2. Chunk requesting:
   - For each chunk in piece, insert into `inflight_requests`
   - Send Request message to peer

3. Data arrival (`on_incoming_piece`):
   - Remove chunk from `inflight_requests`
   - Mark chunk complete in `chunk_status`
   - If all chunks done → verify hash

4. Piece completion:
   - Remove from `inflight_pieces`
   - Set `have[p] = true`

### Piece Stealing Flow

```
IN_FLIGHT (peer A) → IN_FLIGHT (peer B)
```

1. `try_steal_old_slow_piece()`:
   - Finds piece in `inflight_pieces` owned by slow peer
   - Updates `inflight_pieces[p].peer = self`
   - Updates `inflight_pieces[p].started = now`
   - Calls `on_steal(from_peer, to_peer, piece)`

2. `on_steal()`:
   - Sends Cancel messages to victim peer
   - Removes chunks from victim's `inflight_requests` (recent fix)

3. Stealer requests chunks:
   - Inserts into own `inflight_requests`
   - Sends Request messages

**Note:** Stealing does NOT call `reserve_needed_piece()` because the piece shouldn't be in `queue_pieces` (it was already in-flight).

### Peer Death Flow

```
IN_FLIGHT → QUEUED (for pieces owned by dead peer)
```

1. `on_peer_dead()`:
   - Takes `LivePeerState` (consumes it)
   - For each chunk in `inflight_requests`:
     - Calls `mark_piece_broken_if_not_have(piece)`

2. `mark_piece_broken_if_not_have()`:
   - If not `have[p]`: sets `queue_pieces[p] = true`
   - Clears `chunk_status` for piece

**BUG:** `inflight_pieces` is NOT cleaned up! Pieces remain "owned" by dead peer.

### Checksum Failure Flow

```
IN_FLIGHT → QUEUED
```

1. All chunks received, hash verification fails
2. `inflight_pieces.remove(piece)` ← happens first
3. `mark_piece_broken_if_not_have(piece)` → sets `queue_pieces[p] = true`

This flow is correct because `inflight_pieces` is cleaned up before adding to `queue_pieces`.

## Known Bug: Invariant Violation on Peer Death

### Sequence

1. Peer A owns pieces 115-117 in `inflight_pieces`
2. Peer A has chunks in `inflight_requests`
3. Peer A dies
4. `mark_piece_broken_if_not_have(115-117)` → `queue_pieces[115-117] = true`
5. **BUT** `inflight_pieces` still has 115-117 owned by dead peer A!
6. **INVARIANT VIOLATED:** pieces in both `queue_pieces` AND `inflight_pieces`

### Consequence

7. Peer B steals piece 115 from dead peer A
8. `inflight_pieces[115].peer = B`, inserts chunks into B's `inflight_requests`
9. Stealing doesn't call `reserve_needed_piece()` (assumes piece not in `queue_pieces`)
10. `queue_pieces[115]` still true!
11. Next iteration: `reserve_next_needed_piece()` returns 115 again
12. B tries to insert chunks → already in `inflight_requests` → warning

### Root Cause

`on_peer_dead()` cleans up per-peer state (`inflight_requests`) but not global state (`inflight_pieces`). This breaks the invariant that `queue_pieces` and `inflight_pieces` are disjoint.

## Potential Fixes

### Option A: Clean up `inflight_pieces` on peer death (semantically correct)

In `on_peer_dead()`, before marking pieces broken:
```rust
g.inflight_pieces.retain(|_, info| info.peer != dead_peer_addr);
```

This maintains the invariant: pieces transition cleanly from IN_FLIGHT → QUEUED.

### Option B: Call `reserve_needed_piece` when stealing

After stealing, clear from `queue_pieces`:
```rust
g.get_chunks_mut()?.reserve_needed_piece(stolen_idx);
```

This is defensive but treats the symptom, not the cause.

### Option C: Filter `inflight_pieces` in `reserve_next_needed_piece`

Add filter to `natural_order_pieces`:
```rust
.filter(|pid| !g.inflight_pieces.contains_key(pid))
```

Also defensive, doesn't fix the underlying invariant violation.

## Architectural Notes

### State Fragmentation

The piece state is split across multiple structures:
- `ChunkTracker`: `have`, `queue_pieces`, `chunk_status`
- `TorrentStateLive`: `inflight_pieces`
- `LivePeerState`: `inflight_requests`

This fragmentation makes it hard to maintain invariants. State transitions require coordinated updates across multiple structures.

### Potential Refactoring

Consider consolidating piece state into `ChunkTracker`:
- Add `inflight_pieces` to `ChunkTracker`
- Provide atomic state transition methods
- Encapsulate invariant maintenance

This would make illegal states unrepresentable and simplify reasoning about state transitions.

### Lock Ordering

Current comment in code: "locking one inside the other in different order results in deadlocks."

The locking hierarchy appears to be:
1. `peers` lock (via `with_live_mut`)
2. `TorrentStateLive` lock (via `lock_write`)

Care must be taken when modifying state transition logic to maintain this ordering.
