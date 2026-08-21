# Profile transaction and sidecar-only host PATH

## Problem

Desktop boot owns two packages in the shared web Profile: `dsh-desktop-bridge` and `dsh-compaction-hierarchical`. The previous install path ran `plugin add` directly against the user's real `$DSH_HOME/profiles/web`, then attempted a full frozen relink after failure. A failed pnpm command could therefore leave manifest, lockfile, and `node_modules` from different points in the mutation.

A separate desktop-launch problem has the opposite boundary: Finder/Dock launches inherit a short PATH, so a plugin can fail to find a host CLI that works from a terminal. Adding host CLI directories to the shared CLI command builder would also change which pnpm manages the Profile.

## Decision

Desktop-owned package installation is one staged Profile transaction:

1. When both installed package links already resolve to the release-owned paths, skip all pnpm work.
2. Acquire the transaction journal with `create_new`; a live pid + process-start token blocks a second desktop mutation.
3. Create a uniquely named shadow DSH_HOME beside the real DSH_HOME. Both retain the same `<home>/profiles/web` depth and filesystem, preserving relative `file:`/`link:` semantics and directory rename.
4. Copy the real Profile configuration and symlinks without `node_modules`; copy the home-level `cordis.patch.yml` into the shadow only for effective-config validation.
5. In the shadow only, run the old lockfile's frozen install when present, add both desktop-owned packages, run another frozen install, verify every previously resolvable dependency keeps its canonical target, verify Profile `cordis.patch.yml` and `pnpm-workspace.yaml` are byte-identical, and require `dsh --profile web --dump-config` to succeed with the copied home layer.
6. Compare the real Profile configuration, top-level `node_modules` identity, and home-level patch with the snapshots taken before staging. Recheck the renamed Profile backup immediately after real → backup and again immediately before deleting it. A detected terminal or other-process edit rolls back the candidate.
7. Write a candidate marker, append a durable phase record, rename real → backup and shadow → real with bounded retries, then remove backup, marker, shadow home, and journal records.

The immutable primary journal records schema, owner identity, creation time, absolute real/shadow/backup paths, target package paths, the original Profile identity fingerprint, and the home-patch fingerprint. Phase advances (`ShadowReady`, `OriginalMoved`, `ShadowPromoted`, `RollingBack`, or `Aborted`) are append-only records: each is written and synced under a temporary name, atomically renamed to a never-before-used final name, then directory-synced. Recovery combines the latest durable phase (`Prepared` when no phase record exists), `hadOriginal`, the candidate marker, and a strict directory-existence matrix. Before deleting a backup it validates the promoted Profile, target canonical paths, and backup fingerprint. A provably promoted candidate is completed; an interrupted pre-promotion commit restores the backup; ambiguous, corrupt, mismatched-marker, or marker-without-journal state fails loud and preserves every path.

PATH policy is split by process ownership:

- Profile `plugin add/install` and validation use runtime tool directories followed by inherited PATH.
- Only the long-lived `dsh web` sidecar additionally receives existing Homebrew and common user CLI directories, after runtime tools and before inherited PATH.

Thus a GUI-launched plugin can resolve a host CLI while the bundled release runtime's pnpm keeps precedence during Profile mutation. The source-development fallback intentionally continues to resolve pnpm from the developer's inherited PATH.

## Scope boundary

This change does not add a native repair/degrade dialog, change macOS title-bar behavior, or continue boot after an install failure. Those user-facing boot-state decisions belong to the follow-up soft-degrade PR. This transaction is the write-safety prerequisite that follow-up can call without mutating the real Profile before user consent.
