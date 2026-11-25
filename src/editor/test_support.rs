//! Deterministic data generation for editor core tests.

use std::fmt::Debug;
use std::ops::Range;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// SplitMix64 with an explicitly supplied seed.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        value ^ (value >> 31)
    }

    /// Returns a uniformly distributed value in a nonempty range.
    pub(crate) fn gen_range(&mut self, range: Range<usize>) -> usize {
        assert!(range.start < range.end, "range must be nonempty");
        let width = (range.end - range.start) as u64;
        let threshold = width.wrapping_neg() % width;
        loop {
            let value = self.next_u64();
            if value >= threshold {
                return range.start + (value % width) as usize;
            }
        }
    }

    pub(crate) fn gen_bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    pub(crate) fn choose<'a, T>(&mut self, values: &'a [T]) -> Option<&'a T> {
        (!values.is_empty()).then(|| &values[self.gen_range(0..values.len())])
    }

    pub(crate) fn shuffle<T>(&mut self, values: &mut [T]) {
        for i in (1..values.len()).rev() {
            values.swap(i, self.gen_range(0..i + 1));
        }
    }
}

/// Edge cases are included regardless of the random seed.
pub(crate) fn texts(rng: &mut Rng) -> Vec<String> {
    let mut result: Vec<String> = [
        "",
        "ascii",
        "one\ntwo",
        "trailing\n",
        "a\rb\r\nc",
        "日本語漢字",
        "e\u{0301}",
        "😀",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let fragments = ["a", "\n", "\r", "\r\n", "界", "e\u{0301}", "😀"];
    for _ in 0..32 {
        let mut text = String::new();
        for _ in 0..rng.gen_range(0..16) {
            text.push_str(rng.choose(&fragments).expect("nonempty corpus"));
        }
        result.push(text);
    }
    result
}

/// A scalar position and its byte boundary in the same text.
pub(crate) fn scalar_boundary(rng: &mut Rng, text: &str) -> (usize, usize) {
    let boundaries: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .collect();
    let scalar = rng.gen_range(0..boundaries.len());
    (scalar, boundaries[scalar])
}

/// A byte position, including positions inside multi-byte characters.
pub(crate) fn byte_position(rng: &mut Rng, text: &str) -> usize {
    rng.gen_range(0..text.len() + 1)
}

/// Produce a valid byte range and an invalid range (reversed or out of bounds).
pub(crate) fn ranges(rng: &mut Rng, text: &str) -> (Range<usize>, Range<usize>) {
    let a = scalar_boundary(rng, text).1;
    let b = scalar_boundary(rng, text).1;
    let valid = a.min(b)..a.max(b);
    let invalid = if !text.is_empty() && rng.gen_bool() {
        text.len()..0
    } else {
        0..text.len() + 1
    };
    (valid, invalid)
}

/// Reports the seed and generated input when a property panics.
pub(crate) fn assert_law<T: Debug>(
    seeds: &[u64],
    mut generate: impl FnMut(&mut Rng) -> T,
    mut law: impl FnMut(&T),
) {
    for &seed in seeds {
        let mut rng = Rng::new(seed);
        let value = catch_unwind(AssertUnwindSafe(|| generate(&mut rng))).unwrap_or_else(|error| {
            let message = error
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| error.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            panic!(
                "seed: {seed}, counterexample: unavailable (generation panicked), panic: {message}"
            );
        });
        if let Err(error) = catch_unwind(AssertUnwindSafe(|| law(&value))) {
            let message = error
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| error.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            panic!("seed: {seed}, counterexample: {value:?}, panic: {message}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_and_corpus_are_deterministic() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        assert_eq!(a.next_u64(), 0xbdd732262feb6e95);
        assert_eq!(b.next_u64(), 0xbdd732262feb6e95);
        assert_eq!(texts(&mut a), texts(&mut b));
        let mut x = [1, 2, 3, 4];
        let mut y = x;
        a.shuffle(&mut x);
        b.shuffle(&mut y);
        assert_eq!(x, y);
        assert_eq!(a.choose::<u8>(&[]), None);
    }

    #[test]
    fn generated_texts_and_ranges_are_valid() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            let mut rng = Rng::new(5);
            for text in corpus {
                assert!(std::str::from_utf8(text.as_bytes()).is_ok());
                let (scalar, byte) = scalar_boundary(&mut rng, text);
                assert_eq!(text[..byte].chars().count(), scalar);
                assert!(byte_position(&mut rng, text) <= text.len());
                let (valid, invalid) = ranges(&mut rng, text);
                assert!(text.get(valid).is_some());
                assert!(text.get(invalid).is_none());
            }
        });
    }

    #[test]
    fn reports_generator_and_law_failures() {
        let generator =
            catch_unwind(|| assert_law(&[42], |_| -> () { panic!("generator probe") }, |_| {}))
                .unwrap_err();
        let generator = generator.downcast_ref::<String>().unwrap();
        assert!(generator.contains("seed: 42") && generator.contains("generator probe"));
        let law = catch_unwind(|| assert_law(&[42], |_| 7, |_| panic!("law probe"))).unwrap_err();
        let law = law.downcast_ref::<String>().unwrap();
        assert!(
            law.contains("seed: 42")
                && law.contains("counterexample: 7")
                && law.contains("law probe")
        );
    }
}
