# Spec: show rating-scale guidance at the point of rating

**Status:** proposed
**Scope:** small, self-contained UX change to the interactive rating prompt in `run`.

## Problem

After an interactive session, `blindcoder run <cmd>` asks the user to rate the session:

```
  how did it perform?  [-2..2, Enter to skip]:
  how hard was the task?  [0..4]:
```

The two scales are **numeric only** — nothing on screen says what `-2` vs `+2` means, or what
`0` vs `4` difficulty means. A user reaching this prompt for the first time has to stop and look up
(or ask) what the numbers mean before they can answer honestly. The guidance exists in the design but
is not present *where and when it is needed* — at the prompt.

## Goal

Present concise, self-explanatory labels for **both** scales inline at the rating prompt, so a user
can rate correctly without any external reference. Keep it compact (a few lines, not a wall of text).

## Requirements

1. **Performance legend.** Immediately before the performance question, print the `-2..=2` scale with
   a short label per point. Use exactly these labels (they define the canonical meaning):

   | Value | Label |
   |-------|-------|
   | `+2`  | excellent |
   | `+1`  | good |
   | `0`   | neutral / mediocre |
   | `-1`  | poor |
   | `-2`  | terrible / unusable |

2. **Difficulty legend.** After a performance score is entered, immediately before the difficulty
   question, print the `0..=4` scale with a short label per point:

   | Value | Label |
   |-------|-------|
   | `0`   | trivial |
   | `1`   | easy |
   | `2`   | moderate |
   | `3`   | hard |
   | `4`   | very hard |

3. **One line of "why difficulty".** Include a short note that difficulty only *credits* successful
   work — e.g. `(rates the task, not the model; credits a good result on a hard task)`. One line max.

4. **Skip semantics unchanged.** Pressing Enter at the performance question still skips rating
   entirely (and difficulty is then never asked). The legend text must not imply Enter is invalid.

5. **Interactive path only.** These legends appear only in the interactive post-session prompt
   (`prompt_and_rate` in `src/run.rs`). The non-interactive `blindcoder rate --performance … --difficulty …`
   subcommand takes numeric flags and must be **unchanged in behavior**. (You *may* additionally
   enrich that subcommand's `--help`/arg doc-comments with the same labels, but it is optional and
   must not change parsing or defaults.)

6. **No change to stored data or ranges.** Values, ranges (`-2..=2`, `0..=4`), DB writes, and the
   selector are untouched. This is display-only.

## Non-goals

- No change to the rating model, scoring, difficulty-credit math, or storage schema.
- No interactive multiple-choice widget / arrow-key menu — plain printed legend + the existing
  numeric input is sufficient and matches the current CLI style.
- No new config option to toggle the legend (always show it).

## Suggested implementation (guidance, not prescriptive)

- The prompt lives in `prompt_and_rate` in `src/run.rs`, which calls the `prompt_int(prompt, min, max)`
  helper for each question. Print the legend with `println!` just before each `prompt_int` call, then
  keep the existing question line as the actual input prompt.
- Keep the legend compact — a single labelled line per scale reads better than a table in a terminal,
  e.g.:

  ```
  how did it perform?  -2 terrible · -1 poor · 0 neutral · +1 good · +2 excellent
    (Enter to skip)
    > 
  ```
  ```
  how hard was the task?  0 trivial · 1 easy · 2 moderate · 3 hard · 4 very hard
    (rates the task, not the model; credits a good result on a hard task)
    > 
  ```
  The exact formatting is up to you; the *content* (all five/five labels + skip note + difficulty
  note) is the requirement. Match the surrounding indentation style of the existing prompts.
- Consider a small module-level constant or helper for the legend strings so the wording has one home
  (and could be reused by the `rate` `--help` if you choose to do requirement 5's optional part).

## Acceptance criteria

- Running a session to the rating prompt shows the full performance legend before the performance
  question and the full difficulty legend before the difficulty question, including the skip note and
  the difficulty "credits successful work" note.
- Pressing Enter at the performance question still skips cleanly (no difficulty asked, no rating
  recorded) — verify the skip path is untouched.
- `blindcoder rate --performance <n> --difficulty <n>` behaves exactly as before.
- `nix develop -c cargo build --workspace` and `nix develop -c cargo test --workspace` are green
  (no regressions; the `cargo` binary is only on PATH inside the dev shell — see `AGENTS.md`).
- The change is display-only: no diff to storage, ranges, or selector code.

## Verification note for the reviewer

This session will itself be captured (`capture_level = "replay"`) to
`~/.local/state/blindcoder/wire/<sid>.warc`, so the full transcript — not just the resulting diff —
will be reviewed for how the change was designed and carried out.
