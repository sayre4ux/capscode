# Boxes

A box is a persistent project environment with all supported tools available. Create one
from the project folder, then enter it as a shell. For example, creating a box named `abc`
from `~/projects/abc` prints a unique name such as `abc-k3f9x2`:

```bash
cd ~/projects/abc
capscode create abc
# Created box abc-k3f9x2
capscode
```

From that project folder, bare `capscode` enters its box. From elsewhere, name it explicitly:

```bash
capscode abc
```

Inside the box, run `claude`, `codex`, `opencode`, `pi`, or `omp` as needed. Press **Ctrl-b d**
to detach; the box keeps running. Use `capscode ls` to find boxes and their routed providers,
`capscode sync abc` to copy work back, `capscode stop abc` to stop it, and `capscode resume abc`
to rebuild and enter it later. Resume preserves the workspace, but stops running tools and
does not keep shell history. Entering a stopped box with `capscode abc` also resumes it.
By default, each box also keeps its helper caches in its own `/cache` Docker volume. The volume
survives stop and resume, is never copied back to the host, and is not shared with other boxes.
The tool home and `/tmp` are still reset on resume. Use `capscode create --no-cache` to skip the
volume when creating a box; that choice persists across resumes until `capscode resume abc --cache`
enables it. Setting `capsule.cache_volume` to `false` disables cache volumes globally, including
for boxes that request `--cache`. Boxes without a cache volume use temporary `/tmp/.cache` storage.
Run `capscode resume abc --fresh-cache` to replace the box cache with an empty volume when cache
volumes are enabled; switching toolchains also clears it, while keeping the current toolchain
preserves it. `capscode ls --json` reports the cache volume name as `cache_volume` or `null`.
If you run CapsCode inside another multiplexer such as tmux or herdr, the outer one takes the
first Ctrl-b: press **Ctrl-b Ctrl-b d** to detach the box, or pick another prefix with
`capscode config set box.prefix C-q` (tmux key syntax; it applies the next time you enter).
`capscode rm abc` copies back work and removes the box. It refuses while work is unsynced
(`-f` copies it back and removes anyway) or when it cannot verify the box;
`--discard-workspace` deletes the box without copying anything back.
Removing a box also deletes its cache volume; that cache contains no workspace data.

You can use a full box name, its unique prefix, or its six-character id. If a prefix matches
more than one box, CapsCode reports the matching names and asks you to use a longer prefix.
The `ls` command takes precedence over a box prefix such as `ls`; use a longer prefix (for
example `ls-`) to select a box whose name starts with `ls`.

By default, a box gets a private workspace copy. `capscode create --mount` makes a mount-mode
box instead: the project directory is bind-mounted read-write, changes appear on the host
immediately with no copy-back review, `sync` has nothing to copy, and `rm` leaves the host
project in place. Creating one always warns and asks first (`--yes` confirms). Boxes ignore the
global `workspace.mode` setting, which applies to one-shot tool runs only.

`rm` refuses to delete a box that holds work you have not synced, or gitignored files (for
example `.env` or `node_modules/`) that sync never copies back; it lists what would be lost.
`capscode rm <box> --discard-workspace` deletes the box and its workspace anyway. Anyone with access to your Docker socket can
execute into a box, so a box is as private as that socket.

At creation and resume, choose which stored provider credentials the gateway routes. Every
routed provider can be reached by the agent for the box's life; see the [threat model](threat-model.md).
