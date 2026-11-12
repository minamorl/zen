use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::process::ExitCode;

use zen::{Change, Edit, Segment};

fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

// Myers' midpoint search uses two furthest-reaching frontiers. The arrays are
// linear in the input length, including when every line is the same.
fn midpoint(old: &[&str], new: &[&str]) -> Option<(usize, usize)> {
    let (n, m) = (old.len() as isize, new.len() as isize);
    let max_d = (n + m + 1) / 2;
    let offset = max_d + 1;
    let size = (2 * offset + 1) as usize;
    let mut forward = vec![-1_isize; size];
    let mut backward = vec![-1_isize; size];
    forward[(offset + 1) as usize] = 0;
    backward[(offset + 1) as usize] = 0;
    let delta = n - m;
    let odd = delta % 2 != 0;

    for d in 0..=max_d {
        for k in (-d..=d).step_by(2) {
            let index = (offset + k) as usize;
            let mut x = if k == -d || (k != d && forward[index - 1] < forward[index + 1]) {
                forward[index + 1]
            } else {
                forward[index - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && x >= 0 && y >= 0 && old[x as usize] == new[y as usize] {
                x += 1;
                y += 1;
            }
            forward[index] = x;
            if odd && d > 0 {
                let reverse_k = delta - k;
                if reverse_k > -d && reverse_k < d {
                    let reverse_x = backward[(offset + reverse_k) as usize];
                    if reverse_x >= 0 && x + reverse_x >= n {
                        return Some((x as usize, y as usize));
                    }
                }
            }
        }
        for k in (-d..=d).step_by(2) {
            let index = (offset + k) as usize;
            let mut x = if k == -d || (k != d && backward[index - 1] < backward[index + 1]) {
                backward[index + 1]
            } else {
                backward[index - 1] + 1
            };
            let mut y = x - k;
            while x < n
                && y < m
                && x >= 0
                && y >= 0
                && old[(n - x - 1) as usize] == new[(m - y - 1) as usize]
            {
                x += 1;
                y += 1;
            }
            backward[index] = x;
            if !odd {
                let front_k = delta - k;
                if front_k >= -d && front_k <= d {
                    let front_x = forward[(offset + front_k) as usize];
                    if front_x >= 0 && front_x + x >= n {
                        return Some(((n - x) as usize, (m - y) as usize));
                    }
                }
            }
        }
    }
    None
}

fn myers_matches(
    old: &[&str],
    new: &[&str],
    old_at: usize,
    new_at: usize,
    out: &mut Vec<(usize, usize)>,
) {
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        out.push((old_at + prefix, new_at + prefix));
        prefix += 1;
    }
    let (old, new) = (&old[prefix..], &new[prefix..]);
    let (old_at, new_at) = (old_at + prefix, new_at + prefix);
    let mut suffix = 0;
    while suffix < old.len()
        && suffix < new.len()
        && old[old.len() - suffix - 1] == new[new.len() - suffix - 1]
    {
        suffix += 1;
    }
    let (middle_old, middle_new) = (&old[..old.len() - suffix], &new[..new.len() - suffix]);
    if !middle_old.is_empty() && !middle_new.is_empty() {
        if let Some((x, y)) = midpoint(middle_old, middle_new) {
            if (x, y) != (0, 0) && (x, y) != (middle_old.len(), middle_new.len()) {
                myers_matches(&middle_old[..x], &middle_new[..y], old_at, new_at, out);
                myers_matches(
                    &middle_old[x..],
                    &middle_new[y..],
                    old_at + x,
                    new_at + y,
                    out,
                );
            }
        }
    }
    for index in 0..suffix {
        out.push((
            old_at + old.len() - suffix + index,
            new_at + new.len() - suffix + index,
        ));
    }
}

fn lcs_matches(old: &[&str], new: &[&str]) -> Vec<(usize, usize)> {
    let mut matches = Vec::new();
    myers_matches(old, new, 0, 0, &mut matches);
    matches
}

fn line_edit(base: &str, side: &str) -> Edit {
    let old = lines(base);
    let new = lines(side);
    let matches = lcs_matches(&old, &new);

    let mut offsets = Vec::with_capacity(old.len() + 1);
    offsets.push(0);
    for line in &old {
        offsets.push(offsets.last().copied().unwrap_or(0) + line.chars().count());
    }

    let mut changes = Vec::new();
    let (mut start_i, mut start_j) = (0, 0);
    for (i, j) in matches {
        if i != start_i || j != start_j {
            changes.push(Change {
                start: offsets[start_i],
                end: offsets[i],
                text: new[start_j..j].concat(),
            });
        }
        start_i = i + 1;
        start_j = j + 1;
    }
    if start_i != old.len() || start_j != new.len() {
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

fn render(segments: &[(Segment, bool)], marker_length: usize) -> (String, usize) {
    let mut out = String::new();
    let mut conflicts = 0;
    let mut pending = (String::new(), String::new(), String::new());
    let mut has_pending = false;
    for (segment, _) in segments {
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
        return zen::merge(base, a, b)
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
                zen::merge(base_part, &a_edit, &b_edit)
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
    if args.get(1).is_some_and(|argument| argument == "--help") {
        println!("Usage: zen-git-merge %O %A %B %L %P\nGit passes the base, ours, theirs, marker length, and path. The merged result is written to %A.");
        return ExitCode::SUCCESS;
    }
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

#[cfg(test)]
mod tests {
    use super::lcs_matches;

    #[test]
    fn myers_finds_an_lcs_with_repeated_lines() {
        let bodies: Vec<Vec<&str>> = (0..=5)
            .flat_map(|len| {
                (0..1 << len).map(move |bits| {
                    (0..len)
                        .map(|index| {
                            if bits & (1 << index) == 0 {
                                "同\n"
                            } else {
                                "異\n"
                            }
                        })
                        .collect()
                })
            })
            .collect();
        for old in &bodies {
            for new in &bodies {
                let matches = lcs_matches(old, new);
                let mut lengths = vec![vec![0; new.len() + 1]; old.len() + 1];
                for i in (0..old.len()).rev() {
                    for j in (0..new.len()).rev() {
                        lengths[i][j] = if old[i] == new[j] {
                            1 + lengths[i + 1][j + 1]
                        } else {
                            lengths[i + 1][j].max(lengths[i][j + 1])
                        };
                    }
                }
                assert_eq!(
                    matches.len(),
                    lengths[0][0],
                    "old={old:?} new={new:?} matches={matches:?}"
                );
                for (index, &(i, j)) in matches.iter().enumerate() {
                    assert_eq!(old[i], new[j]);
                    if index > 0 {
                        assert!(matches[index - 1].0 < i && matches[index - 1].1 < j);
                    }
                }
            }
        }
    }
}
