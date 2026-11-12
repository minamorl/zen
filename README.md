# zen

zen treats text edits as values that can be applied, composed, used to move marks, and three-way merged through a JSON CLI.

`zen` is a non-interactive CLI. Positions count Unicode scalar values, not UTF-8 bytes. A change uses a half-open range `[start, end)` in the text before that edit; `start == end` inserts at that position. Changes are ordered by `start`, may touch, and must not overlap or contain two insertions at the same position. An empty edit is `{"changes":[]}`. Input edits are normalized before each operation; output edits are in normal form (no empty insertion and no touching changes).

## Laws

- **L1 — Identity.** An empty edit changes neither text nor marks. Composing with an empty edit leaves the original edit unchanged.
- **L2 — Composition.** Applying `a` and then `b` to text gives the same text as applying their composed edit. For marks, this equality holds only when a mark's original position lies outside every closed interval `[start, end]` of the composed edit's normalized changes. Marks inside or at those intervals follow the composed edit's own transport rules; they need not match sequential transport. For example, start with `甲` and a left-gravity mark at position 1. Insert `乙` at 0, then delete `[1, 2)` (the original `甲`): sequential transport moves the mark from 1 to 2 to 1. The composed edit replaces `[0, 1)` with `乙` and moves that mark to 0. Deletion can collapse neighboring gaps before insertion, so no normalized edit can preserve every interior mark's sequential position.
- **L3 — Mark transport.** A mark outside an edit moves with its text. A mark in a deletion moves to its start; at an insertion point, `left` gravity stays before the insertion and `right` moves after it. The default is `right`. More precisely, deleting `[s, e)` leaves positions at or before `s` fixed, moves positions greater than `s` and at or before `e` to `s`, and subtracts `e - s` from later positions. Replacement transports marks as deletion followed by insertion.
- **L4 — Commuting disjoint changes.** When `a` and `b` branch from the same base and change non-overlapping ranges, merging them gives the same text whichever transformed edit is applied first. Insertions at the same position put `a` first. Touching ranges are not overlapping; an insertion strictly inside another change's range is overlapping.
- **L5 — Merge identity and idempotence.** Merging an unchanged side with `x` gives `x`; merging the same change with itself gives `x`. Edit equality uses normalized changes, so replacing text with identical text is still a change.
- **L6 — Conflicts are state.** Overlapping changes produce a conflict segment containing the base, `a`, and `b` content for that region, rather than failing the merge. Outside conflict regions, both sides' changes are included as in L4. Identical changes to the same range and text are applied once without conflict.

## CLI

From this repository, install both `zen` and `zen-git-merge`:

```sh
cargo install --path .
```

Run `zen apply`, `zen compose`, `zen marks`, or `zen merge`. Use `zen --help` or `zen help` for their input and output shapes. Each operation reads exactly one JSON object from standard input and writes exactly one JSON object followed by a newline to standard output. Success exits with code 0. Input errors exit with code 2, write `{"error":{"code":"...","message":"..."}}` to standard error, and leave standard output empty; internal failures exit with code 1.

An edit has the form `{"changes":[{"start":0,"end":1,"text":"乙"}]}`. A mark has `id`, `pos`, and optional `gravity` (`left` or `right`, default `right`). Mark output always includes `gravity` and preserves input order and IDs.

Apply an edit to text:

```sh
printf '%s\n' '{"text":"甲乙","edit":{"changes":[{"start":1,"end":2,"text":"丙"}]}}' | zen apply
# {"text":"甲丙"}
```

Compose `a` with `b`. The positions in `b` refer to the text after `a`; the returned edit refers to the original text:

```sh
printf '%s\n' '{"text":"甲乙","a":{"changes":[{"start":0,"end":0,"text":"新"}]},"b":{"changes":[{"start":2,"end":3,"text":"丙"}]}}' | zen compose
# {"edit":{"changes":[{"start":0,"end":0,"text":"新"},{"start":1,"end":2,"text":"丙"}]}}
```

Transport marks through an edit:

```sh
printf '%s\n' '{"text":"甲乙","marks":[{"id":"left","pos":1,"gravity":"left"},{"id":"right","pos":1}],"edit":{"changes":[{"start":1,"end":1,"text":"新"}]}}' | zen marks
# {"marks":[{"id":"left","pos":1,"gravity":"left"},{"id":"right","pos":2,"gravity":"right"}]}
```

Merge two edits whose positions both refer to `base`:

```sh
printf '%s\n' '{"base":"甲乙","a":{"changes":[{"start":0,"end":1,"text":"丙"}]},"b":{"changes":[{"start":1,"end":2,"text":"丁"}]}}' | zen merge
# {"segments":[{"text":"丙丁"}],"conflicts":0}
```

Overlapping changes appear as `{"conflict":{"base":"甲","a":"乙","b":"丙"}}` in `segments`; `conflicts` counts those segments. Adjacent text segments are combined, and empty text segments are omitted.

## License and support

MIT licensed. Provided **as-is, without support**. See [LICENSE](LICENSE).

## Git merge driver

After installation, configure `zen-git-merge` in each checkout that will run a merge:

```sh
git config --local merge.zen.name 'zen three-way text merge'
git config --local merge.zen.driver 'zen-git-merge %O %A %B %L %P'
```

The repository's `.gitattributes` applies this driver to every path. Git supplies the common ancestor, ours, theirs, marker length, and path. The driver writes the result to Git's `%A` file. A clean merge exits 0; a conflict exits 1 and uses diff3 markers labelled `ours`, `base`, and `theirs`. Invalid UTF-8 exits 2 and leaves `%A` unchanged. Git keeps the driver definition in local `.git/config`, so each checkout needs the commands above before merging.
