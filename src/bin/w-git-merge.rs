use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::process::ExitCode;

use w::{Change, Edit, Segment};

fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn line_edit(base: &str, side: &str) -> Edit {
    let old = lines(base);
    let new = lines(side);
    let mut lengths = vec![vec![0usize; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            lengths[i][j] = if old[i] == new[j] {
                lengths[i + 1][j + 1] + 1
            } else {
                lengths[i + 1][j].max(lengths[i][j + 1])
            };
        }
    }

    let mut offsets = Vec::with_capacity(old.len() + 1);
    offsets.push(0);
    for line in &old {
        offsets.push(offsets.last().copied().unwrap_or(0) + line.chars().count());
    }

    let mut changes = Vec::new();
    let (mut i, mut j, mut start_i, mut start_j) = (0, 0, 0, 0);
    while i < old.len() && j < new.len() {
        if old[i] == new[j] {
            if i != start_i || j != start_j {
                changes.push(Change {
                    start: offsets[start_i],
                    end: offsets[i],
                    text: new[start_j..j].concat(),
                });
            }
            i += 1;
            j += 1;
            start_i = i;
            start_j = j;
        } else if lengths[i + 1][j] >= lengths[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    if i != old.len() || j != new.len() || i != start_i || j != start_j {
        changes.push(Change {
            start: offsets[start_i],
            end: offsets[old.len()],
            text: new[start_j..].concat(),
        });
    }
    Edit { changes }
}

fn marker(out: &mut String, symbol: char, length: usize, label: Option<&str>) {
    out.extend(std::iter::repeat_n(symbol, length));
    if let Some(label) = label {
        out.push(' ');
        out.push_str(label);
    }
    out.push('\n');
}

fn content(out: &mut String, value: &str) {
    out.push_str(value);
    if !value.is_empty() && !value.ends_with('\n') {
        out.push('\n');
    }
}

fn conflict(out: &mut String, base: &str, a: &str, b: &str, length: usize) {
    marker(out, '<', length, Some("ours"));
    content(out, a);
    marker(out, '|', length, Some("base"));
    content(out, base);
    marker(out, '=', length, None);
    content(out, b);
    marker(out, '>', length, Some("theirs"));
}

fn line_atoms(base: &[&str], side: &[&str]) -> (Vec<String>, Vec<String>) {
    let mut lengths = vec![vec![0usize; side.len() + 1]; base.len() + 1];
    for i in (0..base.len()).rev() {
        for j in (0..side.len()).rev() {
            lengths[i][j] = if base[i] == side[j] {
                lengths[i + 1][j + 1] + 1
            } else {
                lengths[i + 1][j].max(lengths[i][j + 1])
            };
        }
    }
    let mut atoms = vec![String::new(); base.len()];
    let mut insertions = vec![String::new(); base.len() + 1];
    let (mut i, mut j, mut start_i, mut start_j) = (0, 0, 0, 0);
    loop {
        if i == base.len() || j == side.len() {
            i = base.len();
            j = side.len();
        }
        if i == base.len() || j == side.len() || base[i] == side[j] {
            let old_count = i - start_i;
            let new_count = j - start_j;
            if old_count == 0 {
                insertions[start_i].push_str(&side[start_j..j].concat());
            } else {
                for k in 0..old_count.min(new_count) {
                    atoms[start_i + k].push_str(side[start_j + k]);
                }
                if new_count > old_count {
                    atoms[i - 1].push_str(&side[start_j + old_count..j].concat());
                }
            }
            if i == base.len() || j == side.len() {
                break;
            }
            atoms[i].push_str(base[i]);
            i += 1;
            j += 1;
            start_i = i;
            start_j = j;
        } else if lengths[i + 1][j] >= lengths[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    (atoms, insertions)
}

fn refine_conflict(base: &str, a: &str, b: &str) -> Vec<Segment> {
    let base_lines = lines(base);
    let a_lines = lines(a);
    let b_lines = lines(b);
    let (a_atoms, a_insertions) = line_atoms(&base_lines, &a_lines);
    let (b_atoms, b_insertions) = line_atoms(&base_lines, &b_lines);
    let mut result = Vec::new();
    for i in 0..=base_lines.len() {
        let insertion = if a_insertions[i] == b_insertions[i] {
            a_insertions[i].clone()
        } else {
            format!("{}{}", a_insertions[i], b_insertions[i])
        };
        if !insertion.is_empty() {
            result.push(Segment::Text(insertion));
        }
        if i == base_lines.len() {
            break;
        }
        let base_line = base_lines[i];
        let a_line = &a_atoms[i];
        let b_line = &b_atoms[i];
        if a_line == b_line {
            result.push(Segment::Text(a_line.clone()));
        } else if a_line == base_line {
            result.push(Segment::Text(b_line.clone()));
        } else if b_line == base_line {
            result.push(Segment::Text(a_line.clone()));
        } else {
            result.push(Segment::Conflict {
                base: base_line.to_owned(),
                a: a_line.clone(),
                b: b_line.clone(),
            });
        }
    }
    let mut text_without_newline = false;
    for segment in &result {
        match segment {
            Segment::Text(value) if !value.is_empty() => {
                text_without_newline = !value.ends_with('\n');
            }
            Segment::Conflict { .. } if text_without_newline => {
                return vec![Segment::Conflict {
                    base: base.to_owned(),
                    a: a.to_owned(),
                    b: b.to_owned(),
                }];
            }
            Segment::Conflict { .. } => text_without_newline = false,
            Segment::Text(_) => {}
        }
    }
    result
}

fn render(segments: &[(Segment, bool)], marker_length: usize) -> (String, usize) {
    let mut out = String::new();
    let mut conflicts = 0;
    let mut pending = (String::new(), String::new(), String::new());
    let mut has_pending = false;
    let refined: Vec<Segment> = segments
        .iter()
        .flat_map(|(segment, forced)| match segment {
            Segment::Text(value) => vec![Segment::Text(value.clone())],
            Segment::Conflict { base, a, b } if *forced => vec![Segment::Conflict {
                base: base.clone(),
                a: a.clone(),
                b: b.clone(),
            }],
            Segment::Conflict { base, a, b } => refine_conflict(base, a, b),
        })
        .collect();
    for segment in &refined {
        match segment {
            Segment::Text(value) => {
                if !value.is_empty() {
                    if has_pending {
                        conflict(&mut out, &pending.0, &pending.1, &pending.2, marker_length);
                        conflicts += 1;
                        pending = (String::new(), String::new(), String::new());
                        has_pending = false;
                    }
                    out.push_str(value);
                }
            }
            Segment::Conflict { base, a, b } => {
                pending.0.push_str(base);
                pending.1.push_str(a);
                pending.2.push_str(b);
                has_pending = true;
            }
        }
    }
    if has_pending {
        conflict(&mut out, &pending.0, &pending.1, &pending.2, marker_length);
        conflicts += 1;
    }
    (out, conflicts)
}

fn invalid_utf8() -> ExitCode {
    eprintln!("{{\"error\":{{\"code\":\"invalid_utf8\",\"message\":\"input is not UTF-8\"}}}}");
    ExitCode::from(2)
}

// The library deliberately orders coincident insertions. A Git driver must
// instead leave every ordering decision visible as a conflict.
fn driver_merge(base: &str, a: &Edit, b: &Edit) -> Vec<(Segment, bool)> {
    struct SideChange<'a> {
        change: &'a Change,
        side: usize,
    }

    fn root(parents: &mut [usize], mut i: usize) -> usize {
        while parents[i] != i {
            parents[i] = parents[parents[i]];
            i = parents[i];
        }
        i
    }

    fn touches(a: &Change, b: &Change) -> bool {
        let a_insert = a.start == a.end;
        let b_insert = b.start == b.end;
        if a_insert && b_insert {
            a.start == b.start
        } else if a_insert {
            b.start <= a.start && a.start <= b.end
        } else if b_insert {
            a.start <= b.start && b.start <= a.end
        } else {
            a.start < b.end && b.start < a.end
        }
    }

    fn forces_conflict(a: &Change, b: &Change) -> bool {
        let a_insert = a.start == a.end;
        let b_insert = b.start == b.end;
        if a_insert && b_insert {
            a.start == b.start && a.text != b.text
        } else if a_insert {
            a.start == b.start || a.start == b.end
        } else if b_insert {
            b.start == a.start || b.start == a.end
        } else {
            false
        }
    }

    let changes: Vec<SideChange<'_>> = a
        .changes
        .iter()
        .map(|change| SideChange { change, side: 0 })
        .chain(
            b.changes
                .iter()
                .map(|change| SideChange { change, side: 1 }),
        )
        .collect();
    let mut parents: Vec<usize> = (0..changes.len()).collect();
    let mut has_forced_pair = false;
    for i in 0..changes.len() {
        for j in i + 1..changes.len() {
            if changes[i].side != changes[j].side && touches(changes[i].change, changes[j].change) {
                has_forced_pair |= forces_conflict(changes[i].change, changes[j].change);
                let left = root(&mut parents, i);
                let right = root(&mut parents, j);
                parents[right] = left;
            }
        }
    }
    if !has_forced_pair {
        return w::merge(base, a, b)
            .segments
            .into_iter()
            .map(|segment| (segment, false))
            .collect();
    }

    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..changes.len() {
        groups.entry(root(&mut parents, i)).or_default().push(i);
    }
    let mut groups: Vec<Vec<usize>> = groups.into_values().collect();
    groups.sort_by_key(|group| {
        group
            .iter()
            .map(|&i| (changes[i].change.start, changes[i].change.end))
            .min()
            .unwrap()
    });

    let mut byte_offsets = Vec::with_capacity(base.chars().count() + 1);
    byte_offsets.extend(base.char_indices().map(|(offset, _)| offset));
    byte_offsets.push(base.len());
    let slice = |start: usize, end: usize| &base[byte_offsets[start]..byte_offsets[end]];
    let mut result = Vec::new();
    let mut cursor = 0;
    for group in groups {
        let start = group
            .iter()
            .map(|&i| changes[i].change.start)
            .min()
            .unwrap();
        let end = group.iter().map(|&i| changes[i].change.end).max().unwrap();
        if cursor < start {
            result.push((Segment::Text(slice(cursor, start).to_owned()), false));
        }
        let side_edit = |side: usize| Edit {
            changes: group
                .iter()
                .filter(|&&i| changes[i].side == side)
                .map(|&i| {
                    let change = changes[i].change;
                    Change {
                        start: change.start - start,
                        end: change.end - start,
                        text: change.text.clone(),
                    }
                })
                .collect(),
        };
        let a_edit = side_edit(0);
        let b_edit = side_edit(1);
        let forced = group.iter().any(|&i| {
            group.iter().any(|&j| {
                changes[i].side != changes[j].side
                    && forces_conflict(changes[i].change, changes[j].change)
            })
        });
        let base_part = slice(start, end);
        if forced {
            let apply_side = |edit: &Edit| {
                let mut text = String::new();
                let mut from = 0;
                for change in &edit.changes {
                    text.push_str(slice(start + from, start + change.start));
                    text.push_str(&change.text);
                    from = change.end;
                }
                text.push_str(slice(start + from, end));
                text
            };
            result.push((
                Segment::Conflict {
                    base: base_part.to_owned(),
                    a: apply_side(&a_edit),
                    b: apply_side(&b_edit),
                },
                true,
            ));
        } else {
            result.extend(
                w::merge(base_part, &a_edit, &b_edit)
                    .segments
                    .into_iter()
                    .map(|segment| (segment, false)),
            );
        }
        cursor = end;
    }
    if cursor < byte_offsets.len() - 1 {
        result.push((
            Segment::Text(slice(cursor, byte_offsets.len() - 1).to_owned()),
            false,
        ));
    }
    result
}

fn run() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let [_, base_path, ours_path, theirs_path, marker_length, _result_path] = &args[..] else {
        return ExitCode::from(1);
    };
    let Ok(base_bytes) = fs::read(base_path) else {
        return ExitCode::from(1);
    };
    let Ok(ours_bytes) = fs::read(ours_path) else {
        return ExitCode::from(1);
    };
    let Ok(theirs_bytes) = fs::read(theirs_path) else {
        return ExitCode::from(1);
    };
    let (Ok(base), Ok(ours), Ok(theirs)) = (
        String::from_utf8(base_bytes),
        String::from_utf8(ours_bytes),
        String::from_utf8(theirs_bytes),
    ) else {
        return invalid_utf8();
    };
    let Ok(marker_length) = marker_length.parse::<usize>() else {
        return ExitCode::from(1);
    };
    let a = line_edit(&base, &ours);
    let b = line_edit(&base, &theirs);
    let merged = driver_merge(&base, &a, &b);
    let (result, conflicts) = render(&merged, marker_length);
    if fs::write(ours_path, result).is_err() {
        return ExitCode::from(1);
    }
    ExitCode::from(if conflicts == 0 { 0 } else { 1 })
}

fn main() -> ExitCode {
    run()
}
