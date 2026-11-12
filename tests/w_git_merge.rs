use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "w-git-merge-{}-{}",
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
    env!("CARGO_BIN_EXE_w-git-merge")
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
        "同\n<<<<<<< ours\n甲\n||||||| base\n二\n=======\n乙\n>>>>>>> theirs\n"
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
        "同\n<<<<<<< ours\n甲\n||||||| base\n二\n=======\n乙\n追加\n>>>>>>> theirs\n三\n"
    );
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
    git(&dir.0, &["config", "user.email", "w-test@example.invalid"]);
    let driver_path = dir.0.join("driver with space");
    fs::copy(executable(), &driver_path).unwrap();
    let command = format!("\"{}\" %O %A %B %L %P", driver_path.display());
    git(&dir.0, &["config", "merge.w.driver", &command]);
    fs::write(dir.0.join(".gitattributes"), "* merge=w\n").unwrap();
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
