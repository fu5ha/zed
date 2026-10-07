# jj diff construction rework

## Original investigation

The slow path is `JjDiffView`, not builtin Git `ProjectDiff`. Opening a live jj diff currently waits 150 ms, enumerates worktree entries and open-buffer text, runs sequential jj/Git queries, reads and hashes all scoped tracked files, then gathers candidate contents before displaying anything. Changed files can be read multiple times. Buffer/diff construction is subsequently serial. Caches are per view, so opening another tab starts cold.

Relevant files:

- `crates/git_ui/src/jj_diff.rs`: view construction, discovery, loading, subscriptions.
- `crates/git/src/jj.rs`: pinned jj read sessions and Git tree/blob reads.
- `crates/project/src/jj_store.rs`: snapshots, save-triggered mutations, cached state and gutter diffs.
- `crates/project/src/git_store.rs`: `snapshot_jj_for_saved_buffer`.
- `crates/git_ui/src/diff_multibuffer.rs` and `crates/project/src/git_store/diff_buffer_list.rs`: builtin Git architecture to follow.

## Findings and agreed direction

- Full-tree hashing originally compensates for disk changes absent from jj's recorded working-copy commit.
- Zed buffer saves already request coalesced jj snapshots (100 ms window). Snapshot completion and publication of the subsequent refreshed read session are separate asynchronous steps.
- Ordinary `jj diff` snapshots first. `--ignore-working-copy` and pinned `--at-op` reads cannot discover unsnapshotted disk changes.
- Filesystem events can maintain differences incrementally, but cannot alone discover edits made before watching starts.
- Agreed solution: privately snapshot when opening a new live diff, then maintain a filesystem/dirty-buffer overlay without snapshotting on filesystem events. Keep existing snapshot-on-save behavior.

## CLI experiment

Tested installed jj `0.41.0-413f539283e5a87f0a2c64dec54a5258a1bee78f` in a disposable colocated repository, with external file creation, modification, and deletion.

Working initialization command:

```sh
jj --no-integrate-operation op log --no-graph -n 1 -T id
```

This snapshots privately and returns the operation ID on stdout. Query it with:

```sh
jj --at-op <id> diff --summary
```

Verified:

- All three external changes were discovered.
- Published operation head and working-copy commit were unchanged.
- Working-copy metadata and operation-head files were byte-for-byte unchanged.
- Private file contents remained stable after later disk edits.
- A subsequent ordinary snapshot did not integrate the private operation.
- Default GC preserved the fresh private operation; `jj --ignore-working-copy util gc --expire now` removed it.

`jj --no-integrate-operation util snapshot` did not print an operation ID in this build. Do not rely on the global flag's help promise that every command prints one.

This experiment establishes basic isolation, not performance at repository scale, concurrent-process behavior, or long-term retention.

## Latest rework plan

### 1. Immediate UI and race-safe initial discovery

- Create the tab/editor immediately; remove the unconditional 150 ms opening delay.
- Begin collecting filesystem events before starting discovery, using existing Zed worktree watches rather than a new watcher.
- Serialize private snapshot creation with our existing jj mutation pipeline, without publishing it as ordinary `JjStore` state.
- Capture the private operation ID and explicitly read that operation to resolve comparison revisions and obtain scoped changed paths.
- Union those paths with dirty, file-backed buffer paths belonging to the repository and opened scopes.
- Queue filesystem events arriving during discovery, and replay them afterward. Use generations/version checks so stale asynchronous results cannot overwrite newer contents.
- Historical change comparisons remain immutable and skip working-copy snapshotting and filesystem overlays.

### 2. Separate comparison loading from presentation

- Follow builtin Git's `DiffBufferList` / `DiffMultibuffer` separation: comparison discovery and loading outside the view; editor/excerpts/navigation inside it.
- Share immutable-content caches and in-flight request deduplication at repository/store level instead of per tab.
- Key baselines by resolved commit/blob identity and comparison lists by resolved revisions and scope.
- Do not blindly reuse current `JjStore::open_diff`: it assumes the current-parent baseline and can turn errors into unchanged diffs. Comparison views need explicit baselines and visible errors.

### 3. Load only candidate files

- Delete full-tree entry enumeration, whole-tree metadata/hash checks, and capture of clean open-buffer text.
- Resolve tree entries only for candidate paths, batching exact-path requests where practical.
- Batch missing immutable blob reads through `git cat-file`.
- Live target contents come from dirty buffers or real project buffers loaded from disk, not separate disk previews followed by reconciliation.
- Historical targets and deletions use synthetic read-only buffers.
- Preserve binary handling, scope/ignore rules, nested-repository ownership, and existing safety checks for unsupported types/conflicts.
- Preserve comparison semantics: current parent versus working copy; historical parent versus selected change; common ancestor versus working copy for "since bookmark"; selected change/bookmark versus working copy for explicit baseline comparisons.

### 4. Concurrent, incremental construction

- Use bounded concurrency for buffer loading and diff calculation.
- Insert completed files incrementally through existing `SplittableEditor` excerpt APIs; do not wait for all files.
- Keep excerpts sorted independently of load completion order. Prefer this over builtin Git's ordered `.buffered(16)`, which introduces head-of-line blocking.
- Prioritize an explicitly requested path and preserve selection/scroll anchors as earlier excerpts arrive.
- Yield between foreground insertions; keep existing excerpts visible during refreshes.

### 5. Targeted live updates

- File create/modify/delete/rename events inspect only affected paths, even when absent from the initial jj changed-path list.
- Compare each affected path against the chosen baseline, with dirty buffers overriding disk contents; add/update/remove excerpts accordingly.
- Buffer edits recalculate that file against its existing baseline.
- Saves retain the existing coalesced snapshot behavior. Reconcile after the new jj read session is published, not merely after the snapshot command exits.
- External jj operations or baseline changes refresh comparison metadata and invalidate affected baselines.
- Scope/ownership changes refresh membership. Coalesce event bursts; do not queue indiscriminate full reloads.
- Never clear filesystem overlays blindly when a snapshot completes: events may have arrived during snapshotting.

### 6. Private-operation lifetime

- Fetch required metadata/content promptly and cache it independently of the private operation.
- Do not assume the operation survives for the tab's lifetime or attempt to publish it merely to retain it.
- Recover explicitly from collected-operation errors, for example by restarting private discovery when required.
- Ensure later filesystem events can resolve baseline content for newly affected paths even if the private operation has been collected; choose a clear fallback before implementation.

## Tradeoffs and acceptance criteria

- Opening writes temporary jj objects/operations, but does not integrate a snapshot or update the checked-out working-copy state.
- Initial discovery still incurs jj's snapshot cost. It avoids Zed's bespoke cold whole-tree hashing; measure rather than promise constant-time opening.
- Filesystem updates require no jj mutation. Dirty-buffer contents are never saved by discovery.
- Live views can combine a pinned baseline with newer disk/buffer contents; historical views remain immutable.
- Out-of-order completion can shift layout, requiring scroll-anchor preservation.
- Shared caches need bounded retention; lost filesystem events require a reconciliation fallback.

Tests should cover initial external changes, events during discovery, unsaved buffers absent from jj's list, immediate save-then-open, revert/rename/delete, concurrent jj generations, dirty-buffer preservation, nested repositories, binary/unsupported files, snapshot failures, and private-operation GC.

Measure tab creation, first excerpt, requested-file visibility, and full completion on a large repository with few changes. Zed should read only candidate working files during construction, insert results before all files finish, and perform no bespoke whole-tree hashing. No implementation has been started.
