# Base generations — design alternatives

## The problem, restated precisely

Today `state_dir()/base` is one fixed directory, mutated in place by
`base::build`. Every box's overlay uses it as `lowerdir`, and overlayfs never
revalidates a lower layer once mounted — so mutating it under a live mount is
the bug this whole area exists to prevent. The current fix is prevention:
`release_boxes()` bails if any box is *running*, and unmounts every *idle* one
first, so `build --refresh|--force` only ever touches an image nothing is
mounted on.

The TODO asks for `build` to stop needing that: give each build its own,
separate directory (a "generation") so a live box's already-open lowerdir is
simply never touched, no matter what `build` does elsewhere. New/re-mounted
boxes pick up the newest generation; a box that is already mounted keeps
serving its own generation's directory, untouched, until nothing references it
and it can be reclaimed.

The mechanical part of this — `state_dir()/bases/<gen>/`, a `current` pointer,
`overlay.id` becoming "which generation this mount is on" instead of an
opaque stamp, `ls`/`remount` treating "on an old-but-live generation" as
normal rather than `stale` — is the same in every alternative below and I'm
confident it's right. What genuinely forks is **how a new generation's
content gets produced** (which is really "what does `--refresh` mean once
in-place mutation of a live lowerdir is off the table") and **when an
unreferenced generation gets deleted**. Both have real, differently-shaped
costs, so I'm stopping here rather than picking for you.

## What's common to all three

- `state_dir()/bases/<id>/` replaces `state_dir()/base`. `<id>` is the same
  nanosecond timestamp `base::stamp()` already generates today — it *is* the
  generation id, so there is no separate `base.id` file to keep in sync with
  it; the directory name is its own identity.
- `state_dir()/bases/current` is a file (or symlink — a symlink is simplest
  and lets `image()`-style callers keep working unchanged, but see the
  uid_base note below for why I'd lean towards a plain text file instead)
  naming the generation new mounts should use.
- A box's `overlay.id` (already written at mount time) becomes the generation
  id it's mounted on, full stop — no separate "does this match the stamp"
  comparison against a single mutable value.
- `agentbox ls`'s `stale` is narrowed to mean "the recorded generation does
  not exist on disk any more" (a corrupted/GC'd-out-from-under-it record, or
  an old-agentbox mount with no record at all — both real bugs) rather than
  "a newer generation exists." A box on an older-but-present generation shows
  something like `mounted (g1712345678, current g1712349999)` — informational,
  not a warning, and `agentbox remount` still does exactly what it does today
  (drop the overlay, mount fresh against whatever `current` is).
- `release_boxes()` and the "refuse while anything is running" bail in
  `base::build` go away entirely: nothing `build` does can affect a directory
  a live overlay has open, so there's nothing left to refuse.

One correctness wrinkle that applies regardless of which alternative you
pick: the image is shifted to `global_uid_base()` once, at the end of each
build (`base.rs:213`). If someone edits `uid_base` in the global config
between builds, two generations can be shifted into different UID ranges.
Today that's latent (there's only one base, built once), but generations
make rebuilding routine, so it stops being a corner case. Whichever
alternative is picked should record the uid_base each generation was shifted
with (e.g. `bases/<id>/uid_base`, a sibling of the image directory, written
by `stamp()`'s replacement) and have `mount()` refuse — not silently
misbehave — if a box's configured `uid_base` doesn't match the generation
it's about to mount on.

## Alternative A — copy-then-upgrade refresh, lazy GC at command boundaries

`--refresh`: `cp -a` (no `--reflink`; the design doc already notes this host's
ext4 gets no reflink, so this is a real full-size copy, "a couple of GB") the
`current` generation into a fresh `bases/<new-id>/`, run today's `pacman -Syu`
inside the copy, then swap `current` to it. `--force` bootstraps a whole new
generation from scratch exactly as today, just into a new directory. Either
way, nothing under the *old* `current` is ever touched.

GC: no explicit command. `base::gc_generations()` — list `bases/*`, subtract
`current` and every id any box's on-disk `overlay.id` names (whether or not
that box happens to be mounted right now, or an unmounted box's own record
still names the last generation it was mounted on and stays untouched until
its own overlay.id changes) — wait, refine: only a *mounted* box pins a
generation, since an idle box's next mount always targets `current` anyway
(exactly like `overlay_stale`'s remount-on-launch does today). So: pin =
mounted boxes' `overlay.id` values, plus `current` itself. Delete everything
else. Call it from the top of `build`, and from the tail of `down`/`reset`/
`rm`/`remount` (the exact places a box stops pinning something), plus `ls` so
`agentbox ls` after the fact reflects reality without needing a separate
command.

**Tradeoffs.** Refresh gets slower and costs real disk headroom (image size
× 2, transiently) it doesn't today — but it stays a `pacman -Syu`, not a full
re-bootstrap, so it's still much cheaper than `--force`. GC has no user-facing
step and never accumulates indefinitely between ordinary commands, at the
cost of touching several call sites and being something you have to trust
rather than something you can watch happen.

## Alternative B — refresh becomes force-into-a-new-generation; explicit `gc`

Drop the distinction between `--refresh` and `--force` at the mechanism
level: both run the full from-scratch bootstrap (pacstrap, keyring, packages,
setup script, ownership shift) into a fresh generation, then swap `current`.
There is no "existing image, apply pacman -Syu to it" fast path any more,
because there's no notion of "the" image to mutate — every build is a clean
room. (The two flags could still both exist for UX/back-compat, `--refresh`
just stops being cheaper than `--force`; or `--refresh` could be retired in
favour of `--force` alone, since they'd do the same thing.)

GC: an explicit `agentbox gc` subcommand, using the same pin logic as
Alternative A (mounted boxes' `overlay.id` ∪ `current`), run by the user
(or a cron/timer they set up) rather than woven into other commands.

**Tradeoffs.** Much simpler code — one "build a generation" path instead of
two, no copy step, no risk of copying a subtly-inconsistent tree forward.
But `--refresh`'s actual purpose today — "pull in security updates without
paying for a full pacstrap" — gets materially slower (always a full network
bootstrap, minutes, same cost as `--force`), which is a real regression for
whoever runs it often. And disk usage from old generations now grows until
someone remembers to run `gc` — plausible to forget, since nothing prompts
for it (though `ls` could still print a hint like "N unreferenced generations,
run `agentbox gc`").

## Alternative C — refresh mutates in place only when nothing is mounted on it yet

Hybrid: if the `current` generation happens to have zero mounted boxes on it
right now (the common case right after a build, or once everyone's relaunched
onto it), `--refresh` mutates it in place with today's cheap `pacman -Syu` —
no copy, no new generation at all. Only when `current` *is* live does it fall
back to Alternative A's copy-then-upgrade, producing a genuine new
generation. `--force` always makes a fresh generation unconditionally (it's
already paying full-bootstrap cost, so there's no cheap in-place case worth
adding for it).

**Tradeoffs.** Cheapest in the common case — most refreshes probably do hit
"nothing's mounted on current yet". But it means `--refresh`'s cost and
on-disk behaviour depend on ambient runtime state at the moment it's run,
which is exactly the kind of surprising cliff this feature is trying to get
away from, and it's the highest-risk option to get wrong: the in-place branch
is only safe if "zero mounted boxes reference this generation" is computed
completely correctly, and a race there (a box mounts between the check and
the `pacman -Syu`) reintroduces the exact bug — a live overlay served a
half-written lower layer — that generations exist to rule out. It's also the
hardest of the three to test with `cargo test` alone, since the interesting
half of the behaviour (which branch fires) depends on real mount state that
the pure unit tests deliberately don't touch; more of the coverage would have
to live in `tests/verify.sh`.

## My leaning, for what it's worth

A. It keeps `--refresh` doing what people reach for it to do (cheap
incremental update) without the runtime-dependent cost cliff of C, and lazy
GC at the handful of places a box stops pinning a generation is a small,
enumerable set of call sites, not a hidden background process.
