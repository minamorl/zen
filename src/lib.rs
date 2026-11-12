//! Value-based text edits in Unicode scalar coordinates.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Edit {
    pub changes: Vec<Change>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gravity {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub id: String,
    pub pos: usize,
    pub gravity: Gravity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Conflict { base: String, a: String, b: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeResult {
    pub segments: Vec<Segment>,
    pub conflicts: usize,
}

/// Input edits may contain adjacent changes and empty insertions. The caller
/// validates ordering and bounds before invoking operations.
pub fn normalize(edit: &Edit) -> Edit {
    let mut result: Vec<Change> = Vec::new();
    for change in &edit.changes {
        if change.start == change.end && change.text.is_empty() {
            continue;
        }
        if let Some(previous) = result.last_mut() {
            if previous.end == change.start {
                previous.end = change.end;
                previous.text.push_str(&change.text);
                continue;
            }
        }
        result.push(change.clone());
    }
    Edit { changes: result }
}

fn scalar_byte(text: &str, pos: usize) -> usize {
    text.char_indices()
        .nth(pos)
        .map_or(text.len(), |(byte, _)| byte)
}

fn slice(text: &str, start: usize, end: usize) -> &str {
    &text[scalar_byte(text, start)..scalar_byte(text, end)]
}

pub fn apply(text: &str, edit: &Edit) -> String {
    let mut output = text.to_owned();
    for change in normalize(edit).changes.iter().rev() {
        let start = scalar_byte(&output, change.start);
        let end = scalar_byte(&output, change.end);
        output.replace_range(start..end, &change.text);
    }
    output
}

pub fn transport_marks(marks: &[Mark], edit: &Edit) -> Vec<Mark> {
    let normalized = normalize(edit);
    let mut result = marks.to_vec();
    for change in normalized.changes.iter().rev() {
        let added = change.text.chars().count();
        for mark in &mut result {
            // Replacement is deletion followed by insertion at start.
            let after_delete = if mark.pos <= change.start {
                mark.pos
            } else if mark.pos <= change.end {
                change.start
            } else {
                mark.pos - (change.end - change.start)
            };
            mark.pos = if after_delete < change.start {
                after_delete
            } else if after_delete > change.start || mark.gravity == Gravity::Right {
                after_delete + added
            } else {
                after_delete
            };
        }
    }
    result
}

#[derive(Clone, Debug)]
enum Atom {
    Base(usize),
    Insert(char),
}

fn apply_atoms(atoms: &mut Vec<Atom>, edit: &Edit) {
    for change in normalize(edit).changes.iter().rev() {
        atoms.splice(
            change.start..change.end,
            change.text.chars().map(Atom::Insert),
        );
    }
}

/// Compose a then b. A base character survives only if neither edit removes it.
pub fn compose(text: &str, a: &Edit, b: &Edit) -> Edit {
    let mut atoms: Vec<Atom> = text
        .chars()
        .enumerate()
        .map(|(index, _)| Atom::Base(index))
        .collect();
    apply_atoms(&mut atoms, a);
    apply_atoms(&mut atoms, b);
    let mut changes = Vec::new();
    let mut cursor = 0;
    let mut inserted = String::new();
    for atom in atoms {
        match atom {
            Atom::Insert(ch) => inserted.push(ch),
            Atom::Base(index) => {
                if cursor != index || !inserted.is_empty() {
                    changes.push(Change {
                        start: cursor,
                        end: index,
                        text: std::mem::take(&mut inserted),
                    });
                }
                cursor = index + 1;
            }
        }
    }
    let length = text.chars().count();
    if cursor != length || !inserted.is_empty() {
        changes.push(Change {
            start: cursor,
            end: length,
            text: inserted,
        });
    }
    normalize(&Edit { changes })
}

fn overlaps(a: &Change, b: &Change) -> bool {
    a.start < b.end && b.start < a.end
}

#[derive(Clone)]
struct Event {
    start: usize,
    end: usize,
    side: usize,
    kind: EventKind,
}

#[derive(Clone)]
enum EventKind {
    Change(Change),
    Conflict { base: String, a: String, b: String },
}

fn append_text(segments: &mut Vec<Segment>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Segment::Text(last)) = segments.last_mut() {
        last.push_str(text);
    } else {
        segments.push(Segment::Text(text.to_owned()));
    }
}

/// Merge edits with positions in the same base. Conflicts retain all three texts.
pub fn merge(base: &str, a: &Edit, b: &Edit) -> MergeResult {
    let a = normalize(a);
    let b = normalize(b);
    let mut changes: Vec<(usize, Change)> = a.changes.into_iter().map(|c| (0, c)).collect();
    for change in b.changes {
        if !changes
            .iter()
            .any(|(side, existing)| *side == 0 && *existing == change)
        {
            changes.push((1, change));
        }
    }
    let mut seen = vec![false; changes.len()];
    let mut events = Vec::new();
    for root in 0..changes.len() {
        if seen[root] {
            continue;
        }
        seen[root] = true;
        let mut group = vec![root];
        let mut head = 0;
        while head < group.len() {
            let current = group[head];
            for candidate in 0..changes.len() {
                if !seen[candidate]
                    && changes[current].0 != changes[candidate].0
                    && overlaps(&changes[current].1, &changes[candidate].1)
                {
                    seen[candidate] = true;
                    group.push(candidate);
                }
            }
            head += 1;
        }
        if group.len() == 1 {
            let (side, change) = &changes[root];
            events.push(Event {
                start: change.start,
                end: change.end,
                side: *side,
                kind: EventKind::Change(change.clone()),
            });
            continue;
        }
        let start = group.iter().map(|&i| changes[i].1.start).min().unwrap_or(0);
        let end = group
            .iter()
            .map(|&i| changes[i].1.end)
            .max()
            .unwrap_or(start);
        let mut sides = [Vec::new(), Vec::new()];
        for i in group {
            let (side, change) = &changes[i];
            sides[*side].push(Change {
                start: change.start - start,
                end: change.end - start,
                text: change.text.clone(),
            });
        }
        let base_text = slice(base, start, end).to_owned();
        let left = apply(
            &base_text,
            &Edit {
                changes: sides[0].clone(),
            },
        );
        let right = apply(
            &base_text,
            &Edit {
                changes: sides[1].clone(),
            },
        );
        events.push(Event {
            start,
            end,
            side: 0,
            kind: EventKind::Conflict {
                base: base_text,
                a: left,
                b: right,
            },
        });
    }
    events.sort_by_key(|event| (event.start, event.end != event.start, event.side));
    let mut result = MergeResult::default();
    let mut cursor = 0;
    for event in events {
        append_text(&mut result.segments, slice(base, cursor, event.start));
        match event.kind {
            EventKind::Change(change) => append_text(&mut result.segments, &change.text),
            EventKind::Conflict { base, a, b } => {
                result.segments.push(Segment::Conflict { base, a, b });
                result.conflicts += 1;
            }
        }
        cursor = event.end;
    }
    append_text(
        &mut result.segments,
        slice(base, cursor, base.chars().count()),
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Random(u64);

    impl Random {
        fn next(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 as usize) % bound
        }

        fn text(&mut self) -> String {
            let alphabet = ['甲', '乙', '丙', 'a', '🌿'];
            let mut output = String::from("日");
            for _ in 0..self.next(8) {
                output.push(alphabet[self.next(alphabet.len())]);
            }
            output
        }

        fn edit(&mut self, length: usize) -> Edit {
            let start = self.next(length + 1);
            let end = start + self.next(length - start + 1);
            let alphabet = ['甲', '🟢', 'x'];
            let text: String = (0..self.next(4))
                .map(|_| alphabet[self.next(alphabet.len())])
                .collect();
            Edit {
                changes: vec![Change { start, end, text }],
            }
        }
    }

    fn mark(pos: usize, gravity: Gravity) -> Mark {
        Mark {
            id: String::from("m"),
            pos,
            gravity,
        }
    }

    #[test]
    fn randomized_identity_composition_and_marks() {
        let mut random = Random(0x59a17);
        for _ in 0..500 {
            let base = random.text();
            let a = random.edit(base.chars().count());
            let after_a = apply(&base, &a);
            let b = random.edit(after_a.chars().count());
            let identity = Edit::default();
            let marks: Vec<Mark> = (0..=base.chars().count())
                .flat_map(|pos| [mark(pos, Gravity::Left), mark(pos, Gravity::Right)])
                .collect();
            assert_eq!(apply(&base, &identity), base, "base={base:?}");
            assert_eq!(transport_marks(&marks, &identity), marks, "base={base:?}");
            assert_eq!(
                compose(&base, &identity, &a),
                normalize(&a),
                "base={base:?}, a={a:?}"
            );
            assert_eq!(
                compose(&base, &a, &identity),
                normalize(&a),
                "base={base:?}, a={a:?}"
            );
            let composed = compose(&base, &a, &b);
            assert_eq!(
                apply(&base, &composed),
                apply(&after_a, &b),
                "base={base:?}, a={a:?}, b={b:?}, composed={composed:?}"
            );
            let sequential = transport_marks(&transport_marks(&marks, &a), &b);
            let combined = transport_marks(&marks, &composed);
            for (index, original) in marks.iter().enumerate() {
                if composed
                    .changes
                    .iter()
                    .all(|c| original.pos < c.start || original.pos > c.end)
                {
                    assert_eq!(
                        sequential[index], combined[index],
                        "base={base:?}, a={a:?}, b={b:?}, composed={composed:?}, mark={original:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn composed_inside_mark_counterexample() {
        let a = Edit {
            changes: vec![Change {
                start: 0,
                end: 0,
                text: "乙".into(),
            }],
        };
        let b = Edit {
            changes: vec![Change {
                start: 1,
                end: 2,
                text: String::new(),
            }],
        };
        let original = vec![mark(1, Gravity::Left)];
        let sequential = transport_marks(&transport_marks(&original, &a), &b);
        let composed = compose("甲", &a, &b);
        let combined = transport_marks(&original, &composed);
        assert_eq!(sequential[0].pos, 1);
        assert_eq!(combined[0].pos, 0);
        assert_eq!(
            composed.changes,
            vec![Change {
                start: 0,
                end: 1,
                text: "乙".into()
            }]
        );
    }

    #[test]
    fn marker_boundaries_and_normalization() {
        let edit = Edit {
            changes: vec![
                Change {
                    start: 1,
                    end: 1,
                    text: "乙".into(),
                },
                Change {
                    start: 1,
                    end: 2,
                    text: "丙".into(),
                },
            ],
        };
        assert_eq!(
            normalize(&edit).changes,
            vec![Change {
                start: 1,
                end: 2,
                text: "乙丙".into()
            }]
        );
        assert_eq!(apply("甲丁", &edit), "甲乙丙");
        let marks = vec![
            mark(0, Gravity::Right),
            mark(1, Gravity::Left),
            mark(1, Gravity::Right),
            mark(2, Gravity::Left),
        ];
        let positions: Vec<usize> = transport_marks(&marks, &edit)
            .iter()
            .map(|m| m.pos)
            .collect();
        assert_eq!(positions, vec![0, 1, 3, 1]);
    }

    #[test]
    fn randomized_nonoverlapping_merge_square_and_identity() {
        let mut random = Random(0x23450);
        for _ in 0..500 {
            let base = random.text();
            let length = base.chars().count();
            let split = random.next(length + 1);
            let a = Edit {
                changes: vec![Change {
                    start: random.next(split + 1),
                    end: split,
                    text: random.text(),
                }],
            };
            let b = Edit {
                changes: vec![Change {
                    start: split,
                    end: split + random.next(length - split + 1),
                    text: random.text(),
                }],
            };
            let a_change = &a.changes[0];
            let b_change = &b.changes[0];
            let after_a = apply(&base, &a);
            let shifted =
                a_change.text.chars().count() as isize - (a_change.end - a_change.start) as isize;
            let b_after_a = Edit {
                changes: vec![Change {
                    start: (b_change.start as isize + shifted) as usize,
                    end: (b_change.end as isize + shifted) as usize,
                    text: b_change.text.clone(),
                }],
            };
            let merged = merge(&base, &a, &b);
            assert_eq!(
                merged.conflicts, 0,
                "base={base:?}, a={a:?}, b={b:?}, merged={merged:?}"
            );
            if normalize(&a) != normalize(&b) {
                assert_eq!(
                    merged,
                    MergeResult {
                        segments: vec![Segment::Text(apply(&after_a, &b_after_a))],
                        conflicts: 0
                    },
                    "base={base:?}, a={a:?}, b={b:?}"
                );
                assert_eq!(
                    merged,
                    MergeResult {
                        segments: vec![Segment::Text(apply(&apply(&base, &b), &a))],
                        conflicts: 0
                    },
                    "base={base:?}, a={a:?}, b={b:?}"
                );
            }
            let empty = Edit::default();
            let expected = MergeResult {
                segments: vec![Segment::Text(apply(&base, &a))],
                conflicts: 0,
            };
            assert_eq!(merge(&base, &empty, &a), expected, "base={base:?}, a={a:?}");
            assert_eq!(merge(&base, &a, &a), expected, "base={base:?}, a={a:?}");
        }
    }

    #[test]
    fn same_point_insertions_and_conflict_partition() {
        let insertion = |text: &str| Edit {
            changes: vec![Change {
                start: 1,
                end: 1,
                text: text.into(),
            }],
        };
        let merged = merge("甲乙", &insertion("a"), &insertion("b"));
        assert_eq!(
            merged,
            MergeResult {
                segments: vec![Segment::Text("甲ab乙".into())],
                conflicts: 0
            }
        );
        let a = Edit {
            changes: vec![
                Change {
                    start: 0,
                    end: 2,
                    text: "A".into(),
                },
                Change {
                    start: 3,
                    end: 5,
                    text: "C".into(),
                },
            ],
        };
        let b = Edit {
            changes: vec![Change {
                start: 1,
                end: 4,
                text: "B".into(),
            }],
        };
        assert_eq!(
            merge("甲乙丙丁戊己", &a, &b),
            MergeResult {
                segments: vec![
                    Segment::Conflict {
                        base: "甲乙丙丁戊".into(),
                        a: "A丙C".into(),
                        b: "甲B戊".into()
                    },
                    Segment::Text("己".into())
                ],
                conflicts: 1,
            }
        );
    }

    #[test]
    fn randomized_conflict_segments() {
        let mut random = Random(0x9347);
        for _ in 0..300 {
            let base = format!("前{}中{}後", random.text(), random.text());
            let length = base.chars().count();
            let a_text = random.text();
            let b_text = random.text();
            let a = Edit {
                changes: vec![Change {
                    start: 1,
                    end: length - 2,
                    text: a_text.clone(),
                }],
            };
            let b = Edit {
                changes: vec![Change {
                    start: 2,
                    end: length - 1,
                    text: b_text.clone(),
                }],
            };
            let expected = MergeResult {
                segments: vec![
                    Segment::Text(slice(&base, 0, 1).into()),
                    Segment::Conflict {
                        base: slice(&base, 1, length - 1).into(),
                        a: format!("{}{}", a_text, slice(&base, length - 2, length - 1)),
                        b: format!("{}{}", slice(&base, 1, 2), b_text),
                    },
                    Segment::Text(slice(&base, length - 1, length).into()),
                ],
                conflicts: 1,
            };
            assert_eq!(
                merge(&base, &a, &b),
                expected,
                "base={base:?}, a={a:?}, b={b:?}"
            );
        }
    }
}
