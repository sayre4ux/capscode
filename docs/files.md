# How your files are handled

By default CapsCode uses a **git snapshot**, not a live mount:

1. Your repo's `HEAD` is cloned into a private container volume; uncommitted changes are
   carried in as a WIP commit.
2. **Untracked files are excluded by default** (keeps `.env`, tokens, and local junk out of
   the capsule and out of the tool's context). Add specific ones with `--include`.
3. Remotes and credential helpers are stripped; the capsule gets a temporary git identity.
4. On an interactive exit, CapsCode summarizes changed files and offers to save them under a
   clearly labeled auto-generated branch, apply them to the current folder, review the diff,
   continue in the capsule shell, or discard them.
5. Automated and batch runs safely save changes to an auto-generated CapsCode branch. The host
   working tree is never changed automatically. If the capsule made no changes, CapsCode exits
   without showing a handoff menu.

`--history full` is the default. `--history shallow` gives the capsule a depth-one clone;
`--history redact-authors` creates a synthetic history with no original commits. In Git mode,
`capscode sync <box>` saves work on a `capscode/<id>` branch. If changes touch hook or runner files,
CapsCode keeps the change set on that branch instead of applying it to the host worktree.

Choosing **Apply changes to this folder** first preserves the generated recovery branch and then
applies the capsule patch. The current branch and Git index are left unchanged.

`--mount` (or `workspace.mode = mount` for one-shot runs; boxes need `create --mount`) is a
convenience mode that bind-mounts the project
read-write instead (faster, full git metadata visible to the tool, but the agent can modify
host files directly, with no copy-back review standing between it and your files — use
deliberately). `capscode sync <box>` reports itself a no-op in mount mode, since there is nothing
to copy.

**Non-git directories work too.** CapsCode snapshots the folder host-side (the capsule sees
no `.git` at all), quiesces the capsule before capturing changes, and then offers to apply
clean, non-conflicting files back to your folder — with per-file conflict detection, backups of
anything overwritten, and a durable staging copy for whatever it declines to touch.
Non-interactive runs never write to your folder.

A file conflicts when your folder changed it since the box and your folder last agreed on it.
Until a file is first synced, that agreement is the snapshot taken when the session started.
After a sync writes it, or finds it identical on both sides, the agreement moves to that
content, so the next sync does not report CapsCode's own writes as your edits. The record lives
in the session's `agreed.jsonl`. Each write is noted there before the file is replaced. If a
sync is interrupted midway, the next one reports each file it was writing as a conflict until
your folder holds the box's version. If the record is damaged, `sync` and `rm` stop and say how
to continue rather than guess.

`capscode sync <box>` and `capscode rm <box>` can copy a non-git workspace back too. Sync stops
a running box to capture a consistent snapshot; after sync, enter it again with `capscode <box>`
or run `capscode resume <box>` to rebuild it around the workspace. A box removed with `rm` does
not take uncopied work with it. Each handoff round retires the previous one into
`previous-handoff.<n>` rather than overwriting it — staging holds what was *not* applied and the
backup holds host files that *were*, so either may be the only copy of something. Nothing is
pruned automatically.

Non-Git copy-back also classifies what it imports. Nothing under `.git/` is ever written to
your host, and files your host would execute out of repo content — `.husky/`, `.githooks/`,
`.envrc`, `.pre-commit-config.yaml`, `lefthook.*` — are reported and skipped rather than
applied, so a capsule cannot plant something your next `git commit` runs.
