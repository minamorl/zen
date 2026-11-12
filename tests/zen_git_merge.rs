use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "zen-git-merge-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn executable() -> &'static str {
    env!("CARGO_BIN_EXE_zen-git-merge")
}

fn driver(base: &[u8], ours: &[u8], theirs: &[u8], marker_size: usize) -> (Output, Vec<u8>) {
    let dir = TempDir::new();
    let base_path = dir.0.join("base");
    let ours_path = dir.0.join("ours");
    let theirs_path = dir.0.join("theirs");
    fs::write(&base_path, base).unwrap();
    fs::write(&ours_path, ours).unwrap();
    fs::write(&theirs_path, theirs).unwrap();
    let output = Command::new(executable())
        .arg(base_path)
        .arg(&ours_path)
        .arg(theirs_path)
        .arg(marker_size.to_string())
        .arg("file.txt")
        .output()
        .unwrap();
    (output, fs::read(ours_path).unwrap())
}

fn next(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    *seed
}

fn body(seed: &mut u64) -> Vec<String> {
    let len = 3 + (next(seed) % 6) as usize;
    (0..len).map(|i| format!("行{i}・甲\n")).collect()
}

fn join(lines: &[String]) -> String {
    lines.concat()
}

fn random_body(seed: &mut u64) -> Vec<String> {
    let len = 4 + (next(seed) % 7) as usize;
    let empty_at = 1 + (next(seed) as usize % (len - 2));
    let mut lines = Vec::with_capacity(len);
    for index in 0..len {
        let word = match next(seed) % 4 {
            0 => "甲",
            1 => "日本語",
            2 => "✨",
            _ => "abc",
        };
        lines.push(if index == empty_at {
            "\n".to_owned()
        } else {
            format!("{index}:{word}:{}\n", next(seed) % 100)
        });
    }
    if next(seed) & (1 << 32) == 0 {
        lines.last_mut().unwrap().pop();
    }
    lines
}

fn alter(lines: &mut Vec<String>, index: usize, operation: u64, label: &str) {
    match operation {
        0 => lines.insert(index, format!("挿入{label}\n")),
        1 => {
            lines.remove(index);
        }
        _ => lines[index] = format!("置換{label}\n"),
    }
}

#[test]
fn randomized_lines_and_edit_kinds() {
    let mut seed = 0x7e9a_21d4_c350_u64;
    let mut with_final_newline = 0;
    let mut without_final_newline = 0;
    let mut generated_bases = Vec::new();
    for trial in 0..120 {
        let case_seed = seed;
        let lines = random_body(&mut seed);
        let base = join(&lines);
        generated_bases.push((case_seed, base.clone()));
        if base.ends_with('\n') {
            with_final_newline += 1;
        } else {
            without_final_newline += 1;
        }
        let i = 1 + (next(&mut seed) as usize % (lines.len() - 1));
        let j = 1 + (next(&mut seed) as usize % (lines.len() - 1));
        let a_operation = next(&mut seed) % 3;
        let b_operation = next(&mut seed) % 3;
        let mut a = lines.clone();
        let mut b = lines.clone();
        alter(&mut a, i, a_operation, &format!("甲{trial}"));
        alter(&mut b, j, b_operation, &format!("乙{trial}"));
        let ours = join(&a);
        let theirs = join(&b);
        let context = format!(
            "seed={case_seed:#x} trial={trial} i={i} j={j} a_operation={a_operation} b_operation={b_operation} base={base:?} ours={ours:?} theirs={theirs:?}"
        );

        for (left, right, expected) in [
            (&ours, &base, &ours),
            (&base, &theirs, &theirs),
            (&ours, &ours, &ours),
        ] {
            let (output, result) = driver(base.as_bytes(), left.as_bytes(), right.as_bytes(), 7);
            assert_eq!(output.status.code(), Some(0), "D1/D2 {context}");
            assert_eq!(result, expected.as_bytes(), "D1/D2 {context}");
        }

        let conflicts = ours != theirs
            && (i == j
                || (a_operation == 0 && b_operation != 0 && i == j + 1)
                || (b_operation == 0 && a_operation != 0 && j == i + 1));
        if conflicts {
            for (left, right) in [(&ours, &theirs), (&theirs, &ours)] {
                let (output, result) =
                    driver(base.as_bytes(), left.as_bytes(), right.as_bytes(), 7);
                let rendered = String::from_utf8(result).unwrap_or_else(|error| {
                    panic!("D4 {context} result_bytes={:?}", error.into_bytes())
                });
                assert_eq!(output.status.code(), Some(1), "D4 {context}");
                let (_, after_ours) = rendered
                    .split_once("<<<<<<< ours\n")
                    .unwrap_or_else(|| panic!("D4 {context} result={rendered:?}"));
                let (_, after_base) = after_ours
                    .split_once("||||||| base\n")
                    .unwrap_or_else(|| panic!("D4 {context} result={rendered:?}"));
                let (_, after_separator) = after_base
                    .split_once("=======\n")
                    .unwrap_or_else(|| panic!("D4 {context} result={rendered:?}"));
                assert!(
                    after_separator.contains(">>>>>>> theirs\n"),
                    "D4 {context} result={rendered:?}"
                );
            }
        } else {
            let expected = if ours == theirs {
                ours.clone()
            } else {
                let mut both = lines.clone();
                let mut combined = [
                    (i, a_operation, format!("甲{trial}")),
                    (j, b_operation, format!("乙{trial}")),
                ];
                combined.sort_by_key(|(index, _, _)| std::cmp::Reverse(*index));
                for (index, operation, label) in combined {
                    alter(&mut both, index, operation, &label);
                }
                join(&both)
            };
            for (left, right) in [(&ours, &theirs), (&theirs, &ours)] {
                let (output, result) =
                    driver(base.as_bytes(), left.as_bytes(), right.as_bytes(), 7);
                assert_eq!(output.status.code(), Some(0), "D2/D3 {context}");
                assert_eq!(result, expected.as_bytes(), "D2/D3 {context}");
            }
        }

        let mut a_overlap = lines.clone();
        let mut b_overlap = lines.clone();
        a_overlap[i] = format!("重なり甲{trial}\n");
        b_overlap[i] = format!("重なり乙{trial}\n");
        let overlap_ours = join(&a_overlap);
        let overlap_theirs = join(&b_overlap);
        let overlap_context = format!(
            "seed={case_seed:#x} trial={trial} base={base:?} ours={overlap_ours:?} theirs={overlap_theirs:?}"
        );
        let (output, result) = driver(
            base.as_bytes(),
            overlap_ours.as_bytes(),
            overlap_theirs.as_bytes(),
            7,
        );
        let rendered = String::from_utf8(result).unwrap_or_else(|error| {
            panic!("D4 {overlap_context} result_bytes={:?}", error.into_bytes())
        });
        assert_eq!(output.status.code(), Some(1), "D4 {overlap_context}");
        assert!(rendered.contains("<<<<<<< ours\n"), "D4 {overlap_context}");
        assert!(rendered.contains(&lines[i]), "D4 {overlap_context}");
        assert!(rendered.contains(&a_overlap[i]), "D4 {overlap_context}");
        assert!(rendered.contains(&b_overlap[i]), "D4 {overlap_context}");
    }
    assert!(
        with_final_newline > 0,
        "seed=0x7e9a21d4c350 generated_bases={generated_bases:?}"
    );
    assert!(
        without_final_newline > 0,
        "seed=0x7e9a21d4c350 generated_bases={generated_bases:?}"
    );
}

#[test]
fn merges_one_hundred_thousand_lines_with_sparse_changes() {
    let base_lines: Vec<String> = (0..100_000)
        .map(|index| format!("行{index:06}・日本語\n"))
        .collect();
    let mut ours_lines = base_lines.clone();
    let mut theirs_lines = base_lines.clone();
    let mut expected_lines = base_lines.clone();
    for index in [10_000, 50_000, 90_000] {
        ours_lines[index] = format!("手元{index}\n");
        expected_lines[index] = ours_lines[index].clone();
    }
    for index in [20_000, 60_000, 80_000] {
        theirs_lines[index] = format!("相手{index}\n");
        expected_lines[index] = theirs_lines[index].clone();
    }
    let base = join(&base_lines);
    let ours = join(&ours_lines);
    let theirs = join(&theirs_lines);
    let expected = join(&expected_lines);
    let (output, result) = driver(base.as_bytes(), ours.as_bytes(), theirs.as_bytes(), 7);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(result, expected.as_bytes());
}

#[test]
fn merges_one_hundred_thousand_repeated_lines_with_sparse_changes_under_ten_seconds() {
    let base_lines = vec!["同じ行・日本語\n".to_owned(); 100_000];
    let mut ours_lines = base_lines.clone();
    let mut theirs_lines = base_lines.clone();
    let mut expected_lines = base_lines.clone();
    for index in [1_000, 1_001, 1_002] {
        ours_lines[index] = format!("手元{index}\n");
        expected_lines[index] = ours_lines[index].clone();
    }
    for index in [98_000, 98_001, 98_002] {
        theirs_lines[index] = format!("相手{index}\n");
        expected_lines[index] = theirs_lines[index].clone();
    }
    let base = join(&base_lines);
    let ours = join(&ours_lines);
    let theirs = join(&theirs_lines);
    let expected = join(&expected_lines);
    let started = Instant::now();
    let (output, result) = driver(base.as_bytes(), ours.as_bytes(), theirs.as_bytes(), 7);
    let elapsed = started.elapsed();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(result, expected.as_bytes());
    assert!(elapsed <= Duration::from_secs(10), "elapsed={elapsed:?}");
}

#[test]
fn properties_d1_d2_d3_d4() {
    let mut seed = 0x2a51_07c9_u64;
    for trial in 0..80 {
        let lines = body(&mut seed);
        let base = join(&lines);
        let i = (next(&mut seed) as usize) % lines.len();
        let mut j = (next(&mut seed) as usize) % (lines.len() - 1);
        if j >= i {
            j += 1;
        }
        let mut a = lines.clone();
        let mut b = lines.clone();
        a[i] = format!("変更甲{trial}\n");
        b[j] = format!("変更乙{trial}\n");
        let ours = join(&a);
        let theirs = join(&b);
        let context = format!("trial={trial} base={base:?} ours={ours:?} theirs={theirs:?}");

        let (output, result) = driver(base.as_bytes(), ours.as_bytes(), base.as_bytes(), 7);
        assert_eq!(output.status.code(), Some(0), "D1 {context}");
        assert_eq!(result, ours.as_bytes(), "D1 {context}");
        let (output, result) = driver(base.as_bytes(), base.as_bytes(), theirs.as_bytes(), 7);
        assert_eq!(output.status.code(), Some(0), "D1 reverse {context}");
        assert_eq!(result, theirs.as_bytes(), "D1 reverse {context}");

        let (output, result) = driver(base.as_bytes(), ours.as_bytes(), ours.as_bytes(), 7);
        assert_eq!(output.status.code(), Some(0), "D2 {context}");
        assert_eq!(result, ours.as_bytes(), "D2 {context}");

        let mut both = lines.clone();
        both[i] = a[i].clone();
        both[j] = b[j].clone();
        let expected = join(&both);
        for (left, right) in [(&ours, &theirs), (&theirs, &ours)] {
            let (output, result) = driver(base.as_bytes(), left.as_bytes(), right.as_bytes(), 7);
            assert_eq!(output.status.code(), Some(0), "D3 {context}");
            assert_eq!(result, expected.as_bytes(), "D3 {context}");
        }

        b[i] = format!("別の変更乙{trial}\n");
        b[j] = lines[j].clone();
        let overlap = join(&b);
        let (output, result) = driver(base.as_bytes(), ours.as_bytes(), overlap.as_bytes(), 9);
        let rendered = String::from_utf8(result).unwrap();
        assert_eq!(output.status.code(), Some(1), "D4 {context}");
        assert!(rendered.contains("<<<<<<<<< ours\n"), "D4 {context}");
        assert!(rendered.contains("||||||||| base\n"), "D4 {context}");
        assert!(rendered.contains("=========\n"), "D4 {context}");
        assert!(rendered.contains(">>>>>>>>> theirs\n"), "D4 {context}");
        assert!(rendered.contains(&lines[i]), "D4 {context}");
        assert!(rendered.contains(&a[i]), "D4 {context}");
        assert!(rendered.contains(&b[i]), "D4 {context}");
    }
}

#[test]
fn invalid_utf8_preserves_ours() {
    for input in 0..3 {
        let mut files = [
            "甲\n".as_bytes().to_vec(),
            "乙\n".as_bytes().to_vec(),
            "丙\n".as_bytes().to_vec(),
        ];
        files[input] = vec![0xff];
        let (output, ours) = driver(&files[0], &files[1], &files[2], 7);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(ours, files[1]);
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "{\"error\":{\"code\":\"invalid_utf8\",\"message\":\"input is not UTF-8\"}}\n"
        );
    }
}

#[test]
fn identical_and_different_overlaps_in_one_file() {
    let base = "一\n二\n三\n四\n";
    let ours = "同\n二\n甲\n四\n";
    let theirs = "同\n二\n乙\n四\n";
    let (output, bytes) = driver(base.as_bytes(), ours.as_bytes(), theirs.as_bytes(), 7);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        "同\n二\n<<<<<<< ours\n甲\n||||||| base\n三\n=======\n乙\n>>>>>>> theirs\n四\n"
    );
}

#[test]
fn adjacent_identical_and_different_overlaps() {
    let base = "一\n二\n";
    let ours = "同\n甲\n";
    let theirs = "同\n乙\n";
    let (output, bytes) = driver(base.as_bytes(), ours.as_bytes(), theirs.as_bytes(), 7);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        "<<<<<<< ours\n同\n甲\n||||||| base\n一\n二\n=======\n同\n乙\n>>>>>>> theirs\n"
    );
}

#[test]
fn adjacent_identical_and_different_with_extra_line() {
    let base = "一\n二\n三\n";
    let ours = "同\n甲\n三\n";
    let theirs = "同\n乙\n追加\n三\n";
    let (output, bytes) = driver(base.as_bytes(), ours.as_bytes(), theirs.as_bytes(), 7);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        "<<<<<<< ours\n同\n甲\n||||||| base\n一\n二\n=======\n同\n乙\n追加\n>>>>>>> theirs\n三\n"
    );
}

#[test]
fn common_suffix_and_prefix_anchor_repeated_lines() {
    let base = "同じ行\n同じ行\n同じ行\n";
    let ours = "甲\n同じ行\n同じ行\n";
    let theirs = "同じ行\n同じ行\n乙\n";
    let (output, bytes) = driver(base.as_bytes(), ours.as_bytes(), theirs.as_bytes(), 7);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8(bytes).unwrap(), "甲\n同じ行\n乙\n");
}

#[test]
fn conflict_without_final_newline() {
    let (output, bytes) = driver("甲".as_bytes(), "乙".as_bytes(), "丙".as_bytes(), 7);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        "<<<<<<< ours\n乙\n||||||| base\n甲\n=======\n丙\n>>>>>>> theirs\n"
    );
}

#[test]
fn conflict_markers_start_at_line_boundaries() {
    let (output, bytes) = driver(b"A\nB", b"XB", b"A\nY", 7);
    let content = String::from_utf8(bytes).unwrap();
    assert_eq!(output.status.code(), Some(1), "{content:?}");
    assert!(content.starts_with("<<<<<<< ours\n"), "{content:?}");
    assert!(!content.contains("XB<<<<<<<"), "{content:?}");
}

fn git(repo: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git_merge_case(conflict: bool) {
    let dir = TempDir::new();
    git(&dir.0, &["init", "-q", "-b", "main"]);
    git(&dir.0, &["config", "user.name", "W Test"]);
    git(
        &dir.0,
        &["config", "user.email", "zen-test@example.invalid"],
    );
    let driver_path = dir.0.join("driver with space");
    fs::copy(executable(), &driver_path).unwrap();
    let command = format!("\"{}\" %O %A %B %L %P", driver_path.display());
    git(&dir.0, &["config", "merge.zen.driver", &command]);
    fs::write(dir.0.join(".gitattributes"), "* merge=zen\n").unwrap();
    fs::write(dir.0.join("file.txt"), "甲\n乙\n丙\n").unwrap();
    git(&dir.0, &["add", ".gitattributes", "file.txt"]);
    git(&dir.0, &["commit", "-qm", "base"]);
    git(&dir.0, &["checkout", "-qb", "theirs"]);
    fs::write(dir.0.join("file.txt"), "甲\n乙\n相手\n").unwrap();
    git(&dir.0, &["commit", "-qam", "theirs"]);
    git(&dir.0, &["checkout", "-q", "main"]);
    let ours = if conflict {
        "甲\n乙\n手元\n"
    } else {
        "手元\n乙\n丙\n"
    };
    fs::write(dir.0.join("file.txt"), ours).unwrap();
    git(&dir.0, &["commit", "-qam", "ours"]);
    let output = Command::new("git")
        .args(["merge", "--no-edit", "theirs"])
        .current_dir(&dir.0)
        .output()
        .unwrap();
    let content = fs::read_to_string(dir.0.join("file.txt")).unwrap();
    if conflict {
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            content,
            "甲\n乙\n<<<<<<< ours\n手元\n||||||| base\n丙\n=======\n相手\n>>>>>>> theirs\n"
        );
    } else {
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(content, "手元\n乙\n相手\n");
    }
}

#[test]
fn git_merges_cleanly_with_custom_driver() {
    git_merge_case(false);
}

#[test]
fn git_preserves_driver_conflict() {
    git_merge_case(true);
}
