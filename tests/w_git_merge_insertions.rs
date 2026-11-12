use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_CASE: AtomicUsize = AtomicUsize::new(0);

struct Inputs(PathBuf);

impl Inputs {
    fn new(base: &str, ours: &str, theirs: &str) -> Self {
        let mut directory = std::env::var_os("TMPDIR")
            .or_else(|| std::env::var_os("RUNNER_TEMP"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        directory.push(format!(
            "w-git-merge-insertions-{}-{}",
            std::process::id(),
            NEXT_CASE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("base"), base).unwrap();
        fs::write(directory.join("ours"), ours).unwrap();
        fs::write(directory.join("theirs"), theirs).unwrap();
        Self(directory)
    }

    fn run(&self) -> (i32, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_w-git-merge"))
            .args([
                self.0.join("base").to_str().unwrap(),
                self.0.join("ours").to_str().unwrap(),
                self.0.join("theirs").to_str().unwrap(),
                "7",
                "file.txt",
            ])
            .output()
            .unwrap();
        assert!(output.stdout.is_empty(), "stdout: {output:?}");
        assert!(output.stderr.is_empty(), "stderr: {output:?}");
        (
            output.status.code().unwrap(),
            fs::read_to_string(self.0.join("ours")).unwrap(),
        )
    }
}

impl Drop for Inputs {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn different_insertions_at_same_position_conflict() {
    let inputs = Inputs::new("甲\n", "青\n甲\n", "赤\n甲\n");
    assert_eq!(
        inputs.run(),
        (
            1,
            "<<<<<<< ours\n青\n||||||| base\n=======\n赤\n>>>>>>> theirs\n甲\n".into()
        )
    );
}

#[test]
fn identical_insertions_at_same_position_apply_once() {
    let inputs = Inputs::new("甲\n", "青\n甲\n", "青\n甲\n");
    assert_eq!(inputs.run(), (0, "青\n甲\n".into()));
}

#[test]
fn insertion_at_replacement_start_conflicts_with_whole_replacement() {
    let inputs = Inputs::new("元\n後\n", "共\n甲\n後\n", "乙\n元\n後\n");
    assert_eq!(
        inputs.run(),
        (
            1,
            "<<<<<<< ours\n共\n甲\n||||||| base\n元\n=======\n乙\n元\n>>>>>>> theirs\n後\n".into()
        )
    );
}

#[test]
fn insertion_at_replacement_end_conflicts() {
    let inputs = Inputs::new("元\n後\n", "甲\n後\n", "元\n乙\n後\n");
    assert_eq!(
        inputs.run(),
        (
            1,
            "<<<<<<< ours\n甲\n||||||| base\n元\n=======\n元\n乙\n>>>>>>> theirs\n後\n".into()
        )
    );
}

#[test]
fn adjacent_nonempty_changes_remain_clean() {
    let inputs = Inputs::new("元\n後\n", "甲\n後\n", "元\n乙\n");
    assert_eq!(inputs.run(), (0, "甲\n乙\n".into()));
}

#[test]
fn forced_conflict_keeps_distant_clean_changes() {
    let inputs = Inputs::new("甲\n中\n終\n", "青\n甲\n中\n結\n", "赤\n甲\n改\n終\n");
    assert_eq!(
        inputs.run(),
        (
            1,
            "<<<<<<< ours\n青\n||||||| base\n=======\n赤\n>>>>>>> theirs\n甲\n改\n結\n".into()
        )
    );
}
