//! Cross-module text transformation laws.
//!
//! Verifies the algebraic and UTF-8 boundary invariants relating immutable
//! text, byte spans, anchors, minimal diffs, composable position maps, clips,
//! splices, patches, and revisioned annotation/extmark endpoint tracking:
//!
//! 1. Sequential vs composed position-mapping equivalence across multi-step
//!    replacements and splices.
//! 2. Exact byte-for-byte inverse-splice and inverse-diff restoration across
//!    multi-step sequences.
//! 3. Exact forward-then-inverse identity on every valid boundary in untouched
//!    regions outside all edited spans.
//! 4. Deterministic collapsed/deleted-interior Left vs Right affinity laws and
//!    multi-revision marker/extmark endpoint consistency.
//! 5. Diff, clip, and position-map cross-checks against independent reference
//!    oracles, plus typed error rejection of invalid inputs.

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use crate::editor::edit::ids::{ActorId, BufferId};
    use crate::editor::edit::patch::{Patch, PatchError};
    use crate::editor::edit::splice::{Splice, SpliceError};
    use crate::editor::interop::annotations::{
        map_agent, map_emacs, map_neovim, project_agent, project_emacs, project_neovim,
        unproject_agent, unproject_emacs, unproject_neovim,
    };
    use crate::editor::interop::coordinates::CoordKind;
    use crate::editor::test_support::{assert_law, texts, Rng};
    use crate::editor::text::anchors::{
        Affinity, Anchor, AnchorError, ReplacementEffect, TransportedSpan,
    };
    use crate::editor::text::clip::{clip, clip_lines, clip_with_byte_limit, Clip, ClipError};
    use crate::editor::text::diff::{diff, Diff};
    use crate::editor::text::lines::LineIndex;
    use crate::editor::text::position_map::{
        AnchoredSpan, ByteReplacement, PositionAnchor, PositionMap, PositionMapError,
    };
    use crate::editor::text::span::{ByteSpan, SpanError};
    use crate::editor::text::utf8::{Text, Utf8Error};
    use crate::editor::workspace::annotation_mapping::{
        self, AnnotationMappingError, MappingRevisions, RevisionedMap,
    };
    use crate::editor::workspace::annotations::{
        Annotation, AnnotationId, AnnotationKey, AnnotationStore, CollapsePolicy, NamespaceId,
    };
    use crate::editor::workspace::catalog::BufferCatalog;
    use crate::editor::workspace::version::Revision;

    /// Independent reference edit step operating purely on UTF-8 `String` byte indices.
    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RawStep {
        start: usize,
        end: usize,
        inserted: String,
    }

    #[derive(Clone, Debug)]
    struct SequenceCase {
        initial: String,
        steps: Vec<RawStep>,
    }

    /// Independent reference oracle functions that never delegate to production
    /// text-transformation types.
    mod oracle {
        use super::RawStep;

        pub(super) fn char_boundaries(source: &str) -> Vec<usize> {
            (0..=source.len())
                .filter(|&offset| source.is_char_boundary(offset))
                .collect()
        }

        pub(super) fn apply_step(source: &str, step: &RawStep) -> (String, RawStep) {
            assert!(step.start <= step.end && step.end <= source.len());
            assert!(source.is_char_boundary(step.start) && source.is_char_boundary(step.end));
            let removed = source[step.start..step.end].to_owned();
            let mut next =
                String::with_capacity(source.len() - (step.end - step.start) + step.inserted.len());
            next.push_str(&source[..step.start]);
            next.push_str(&step.inserted);
            next.push_str(&source[step.end..]);
            let inverse = RawStep {
                start: step.start,
                end: step.start + step.inserted.len(),
                inserted: removed,
            };
            (next, inverse)
        }

        pub(super) fn apply_sequence(
            initial: &str,
            steps: &[RawStep],
        ) -> (Vec<String>, Vec<RawStep>) {
            let mut snapshots = Vec::with_capacity(steps.len() + 1);
            let mut inverses = Vec::with_capacity(steps.len());
            snapshots.push(initial.to_owned());
            let mut current = initial.to_owned();
            for step in steps {
                let (next, inverse) = apply_step(&current, step);
                inverses.push(inverse);
                snapshots.push(next.clone());
                current = next;
            }
            (snapshots, inverses)
        }

        /// Decomposes each replacement into two physical operations on a byte tape:
        /// 1. Pure deletion of `[start, end)`, coalescing all pins in `[start, end]` onto `start`.
        /// 2. Pure insertion of `inserted.len()` bytes at `start`, keeping `Left` pins at `start`
        ///    and moving `Right` pins to `start + inserted.len()`.
        pub(super) fn transport_via_delete_then_insert(
            steps: &[RawStep],
            offset: usize,
            right: bool,
        ) -> usize {
            let mut pos = offset;
            for step in steps {
                // Phase 1: pure deletion of [step.start, step.end)
                let after_delete = if pos < step.start {
                    pos
                } else if pos <= step.end {
                    step.start
                } else {
                    pos - (step.end - step.start)
                };
                // Phase 2: pure insertion of step.inserted.len() bytes at step.start
                pos = if after_delete < step.start {
                    after_delete
                } else if after_delete > step.start {
                    after_delete + step.inserted.len()
                } else if right {
                    step.start + step.inserted.len()
                } else {
                    step.start
                };
            }
            pos
        }

        /// Returns initial boundaries that remain strictly outside `[step.start, step.end]`
        /// at every step in `steps`, together with their final byte offsets.
        pub(super) fn globally_untouched_boundaries(
            initial: &str,
            steps: &[RawStep],
        ) -> Vec<(usize, usize)> {
            let mut untouched = Vec::new();
            'boundary: for start_offset in char_boundaries(initial) {
                let mut pos = start_offset;
                for step in steps {
                    if step.start <= pos && pos <= step.end {
                        continue 'boundary;
                    }
                    if pos > step.end {
                        pos = pos - (step.end - step.start) + step.inserted.len();
                    }
                }
                untouched.push((start_offset, pos));
            }
            untouched
        }

        /// Checks whether the open-to-closed half-open region `[a, b)` in `initial`
        /// has no edit contacting or intersecting it at any step.
        pub(super) fn span_untouched_throughout(
            steps: &[RawStep],
            mut start: usize,
            mut end: usize,
        ) -> Option<(usize, usize)> {
            for step in steps {
                // Closed contact with [start, end] touches the span or its boundary.
                if step.start <= end && start <= step.end {
                    return None;
                }
                if step.end < start {
                    let shift = step.inserted.len() as isize - (step.end - step.start) as isize;
                    start = (start as isize + shift) as usize;
                    end = (end as isize + shift) as usize;
                }
            }
            Some((start, end))
        }

        /// Independent maximal common scalar prefix / maximal non-overlapping scalar suffix diff.
        pub(super) fn minimal_diff(before: &str, after: &str) -> Option<(usize, usize, String)> {
            if before == after {
                return None;
            }
            let before_bounds = char_boundaries(before);
            let prefix = before_bounds
                .iter()
                .copied()
                .filter(|&p| p <= after.len() && after.is_char_boundary(p))
                .filter(|&p| before[..p] == after[..p])
                .max()
                .unwrap_or(0);
            let suffix = before_bounds
                .iter()
                .copied()
                .map(|p| before.len() - p)
                .filter(|&n| {
                    n <= before.len() - prefix
                        && n <= after.len() - prefix
                        && after.is_char_boundary(after.len() - n)
                        && before[before.len() - n..] == after[after.len() - n..]
                })
                .max()
                .unwrap_or(0);
            let span_end = before.len() - suffix;
            let replacement = after[prefix..after.len() - suffix].to_owned();
            Some((prefix, span_end, replacement))
        }

        /// Independent UTF-8 scalar-prefix truncation within `max_bytes`.
        pub(super) fn truncate_utf8(slice: &str, max_bytes: usize) -> (&str, bool) {
            let mut end = 0;
            for ch in slice.chars() {
                let width = ch.len_utf8();
                if end + width > max_bytes {
                    return (&slice[..end], true);
                }
                end += width;
            }
            (&slice[..end], false)
        }

        /// Independent byte scan for LF-delimited lines (CR is content).
        pub(super) fn line_spans(source: &str) -> Vec<(usize, usize, usize)> {
            let mut spans = Vec::new();
            let mut start = 0;
            for (idx, byte) in source.bytes().enumerate() {
                if byte == b'\n' {
                    spans.push((start, idx, idx + 1));
                    start = idx + 1;
                }
            }
            spans.push((start, source.len(), source.len()));
            spans
        }

        /// Normalizes an annotation/extmark span (inverted collapses to empty at `start`).
        pub(super) fn normalize_span(start: usize, end: usize) -> (usize, usize) {
            if start > end {
                (start, start)
            } else {
                (start, end)
            }
        }
    }

    const AFFINITIES: [Affinity; 2] = [Affinity::Left, Affinity::Right];
    const PAYLOADS: &[&str] = &[
        "",
        "x",
        "ab",
        "\n",
        "\r",
        "\r\n",
        "é",
        "è",
        "界",
        "畑",
        "😀",
        "😁",
        "e\u{0301}",
        "a\r\n界😀",
    ];
    const SEEDS: &[u64] = &[0, 1, 42, 1337, u64::MAX];

    fn step(start: usize, end: usize, inserted: &str) -> RawStep {
        RawStep {
            start,
            end,
            inserted: inserted.to_owned(),
        }
    }

    fn make_catalog(buffer: BufferId, text: &Text) -> BufferCatalog {
        let mut catalog = BufferCatalog::new();
        catalog.register(buffer, text.clone()).unwrap();
        catalog
    }

    fn make_annotation(
        buffer: BufferId,
        id: u64,
        span: ByteSpan,
        start_affinity: Affinity,
        end_affinity: Affinity,
        collapse: CollapsePolicy,
    ) -> Annotation {
        Annotation {
            key: AnnotationKey {
                buffer,
                namespace: NamespaceId::new(1),
                id: AnnotationId::new(id),
            },
            owner: ActorId::new(1),
            span,
            start_affinity,
            end_affinity,
            priority: (id % 1000) as u32,
            collapse,
            label: format!("mark-{id}"),
        }
    }

    fn generate_step(rng: &mut Rng, current: &str) -> RawStep {
        let bounds = oracle::char_boundaries(current);
        let a = bounds[rng.gen_range(0..bounds.len())];
        let b = bounds[rng.gen_range(0..bounds.len())];
        let inserted = *rng.choose(PAYLOADS).expect("nonempty payloads");
        RawStep {
            start: a.min(b),
            end: a.max(b),
            inserted: inserted.to_owned(),
        }
    }

    fn generate_sequence_cases(rng: &mut Rng) -> Vec<SequenceCase> {
        let mut base_texts = texts(rng);
        for extra in [
            "éêè",
            "界畑日本語",
            "a😀😁z",
            "\r\n\n\r",
            "a\r\n界😀\ne\u{0301}\r\n終",
        ] {
            base_texts.push(extra.to_owned());
        }
        let mut cases = Vec::with_capacity(base_texts.len());
        for initial in base_texts {
            let step_count = rng.gen_range(0..7);
            let mut current = initial.clone();
            let mut steps = Vec::with_capacity(step_count);
            for _ in 0..step_count {
                let next_step = generate_step(rng, &current);
                let (next_text, _) = oracle::apply_step(&current, &next_step);
                steps.push(next_step);
                current = next_text;
            }
            cases.push(SequenceCase { initial, steps });
        }
        cases
    }

    struct BuiltChain {
        texts: Vec<Text>,
        inverse_splices: Vec<Splice>,
        effects: Vec<ReplacementEffect>,
        maps: Vec<PositionMap>,
        inverse_maps: Vec<PositionMap>,
        oracle_snapshots: Vec<String>,
    }

    fn build_chain(initial: &str, steps: &[RawStep]) -> BuiltChain {
        let (oracle_snapshots, oracle_inverses) = oracle::apply_sequence(initial, steps);
        let mut oracle_back = oracle_snapshots.last().unwrap().clone();
        for idx in (0..steps.len()).rev() {
            let (prev, _) = oracle::apply_step(&oracle_back, &oracle_inverses[idx]);
            assert_eq!(prev.as_bytes(), oracle_snapshots[idx].as_bytes());
            oracle_back = prev;
        }
        assert_eq!(oracle_back.as_bytes(), initial.as_bytes());

        let mut texts = Vec::with_capacity(steps.len() + 1);
        let mut inverse_splices = Vec::with_capacity(steps.len());
        let mut effects = Vec::with_capacity(steps.len());
        let mut maps = Vec::with_capacity(steps.len());
        let mut inverse_maps = Vec::with_capacity(steps.len());

        let mut current = Text::from_str(initial);
        texts.push(current.clone());

        for (idx, raw) in steps.iter().enumerate() {
            let span = ByteSpan::validate_in(&current, raw.start, raw.end).unwrap();
            let replacement = Text::from_str(&raw.inserted);
            let splice = Splice::new(span, replacement.clone());
            let (next, inverse) = splice.apply_with_inverse(&current).unwrap();
            assert_eq!(splice.apply(&current), Ok(next.clone()));
            assert_eq!(splice.invert(&current), Ok(inverse.clone()));
            assert_eq!(next.as_bytes(), oracle_snapshots[idx + 1].as_bytes());
            assert_eq!(
                inverse.span().start().as_usize(),
                oracle_inverses[idx].start
            );
            assert_eq!(inverse.span().end().as_usize(), oracle_inverses[idx].end);
            assert_eq!(
                inverse.replacement().as_str(),
                oracle_inverses[idx].inserted
            );
            assert_eq!(
                splice.is_noop(),
                raw.start == raw.end && raw.inserted.is_empty()
            );

            // Double inversion restores both the original text and the exact forward splice.
            let (restored, double_inv) = inverse.apply_with_inverse(&next).unwrap();
            assert_eq!(restored.as_bytes(), current.as_bytes());
            assert_eq!(double_inv, splice);

            let effect = ReplacementEffect {
                span,
                inserted_len: replacement.len_bytes(),
            };
            let forward_map = PositionMap::from_replacement(
                current.len_bytes(),
                ByteReplacement {
                    start: raw.start,
                    end: raw.end,
                    inserted_len: raw.inserted.len(),
                },
            )
            .unwrap();
            let inverse_map = PositionMap::from_replacement(
                next.len_bytes(),
                ByteReplacement {
                    start: inverse.span().start().as_usize(),
                    end: inverse.span().end().as_usize(),
                    inserted_len: inverse.replacement().len_bytes(),
                },
            )
            .unwrap();

            inverse_splices.push(inverse);
            effects.push(effect);
            maps.push(forward_map);
            inverse_maps.push(inverse_map);
            texts.push(next.clone());
            current = next;
        }

        BuiltChain {
            texts,
            inverse_splices,
            effects,
            maps,
            inverse_maps,
            oracle_snapshots,
        }
    }

    fn compose_left(initial_len: usize, maps: &[PositionMap]) -> PositionMap {
        let mut acc = PositionMap::identity(initial_len);
        for map in maps {
            acc = acc.compose(map).unwrap();
        }
        acc
    }

    fn compose_right(final_len: usize, maps: &[PositionMap]) -> PositionMap {
        let mut acc = PositionMap::identity(final_len);
        for map in maps.iter().rev() {
            acc = map.compose(&acc).unwrap();
        }
        acc
    }

    fn verify_diff_clip_pair(before: &Text, after: &Text) {
        let actual = diff(before, after);
        let expected = oracle::minimal_diff(before.as_str(), after.as_str());
        let reverse = diff(after, before);
        let expected_reverse = oracle::minimal_diff(after.as_str(), before.as_str());

        assert_eq!(actual.is_none(), before == after);
        assert_eq!(reverse.is_none(), before == after);

        let (diff_map, inv_diff_map, prefix, before_core_end, after_core_end) =
            match (actual, expected, reverse, expected_reverse) {
                (None, None, None, None) => (
                    PositionMap::identity(before.len_bytes()),
                    PositionMap::identity(after.len_bytes()),
                    before.len_bytes(),
                    before.len_bytes(),
                    after.len_bytes(),
                ),
                (
                    Some(fwd),
                    Some((start, end, replacement)),
                    Some(inv),
                    Some((inv_start, inv_end, inv_replacement)),
                ) => {
                    assert_eq!(fwd.span.start().as_usize(), start);
                    assert_eq!(fwd.span.end().as_usize(), end);
                    assert_eq!(fwd.replacement.as_str(), replacement);
                    assert_eq!(inv.span.start().as_usize(), inv_start);
                    assert_eq!(inv.span.end().as_usize(), inv_end);
                    assert_eq!(inv.replacement.as_str(), inv_replacement);

                    // Forward and reverse diffs restore exact bytes.
                    assert_eq!(fwd.apply(before), Ok(after.clone()));
                    assert_eq!(inv.apply(after), Ok(before.clone()));

                    // Minimal reverse diff equals the splice inverse of minimal forward diff.
                    let (splice_after, splice_inv) = Splice::new(fwd.span, fwd.replacement.clone())
                        .apply_with_inverse(before)
                        .unwrap();
                    assert_eq!(splice_after.as_bytes(), after.as_bytes());
                    assert_eq!(splice_inv.span(), inv.span);
                    assert_eq!(splice_inv.replacement(), &inv.replacement);

                    // Clipped cores match the opposing diff replacements.
                    assert_eq!(
                        clip(before, fwd.span).unwrap().as_str(),
                        inv.replacement.as_str()
                    );
                    assert_eq!(
                        clip(after, inv.span).unwrap().as_str(),
                        fwd.replacement.as_str()
                    );

                    // Diff-based ReplacementEffect and PositionMap preserve UTF-8 boundaries.
                    let effect = ReplacementEffect {
                        span: fwd.span,
                        inserted_len: fwd.replacement.len_bytes(),
                    };
                    let fwd_map = PositionMap::from_replacement(
                        before.len_bytes(),
                        ByteReplacement {
                            start,
                            end,
                            inserted_len: fwd.replacement.len_bytes(),
                        },
                    )
                    .unwrap();
                    let inv_map = PositionMap::from_replacement(
                        after.len_bytes(),
                        ByteReplacement {
                            start: inv_start,
                            end: inv_end,
                            inserted_len: inv.replacement.len_bytes(),
                        },
                    )
                    .unwrap();
                    let raw = [step(start, end, fwd.replacement.as_str())];
                    for boundary in before.char_boundaries() {
                        for affinity in AFFINITIES {
                            let via_effect = effect
                                .transport(
                                    Anchor {
                                        offset: boundary,
                                        affinity,
                                    },
                                    before,
                                    after,
                                )
                                .unwrap();
                            let via_map =
                                fwd_map.map_offset(boundary.as_usize(), affinity).unwrap();
                            let via_oracle = oracle::transport_via_delete_then_insert(
                                &raw,
                                boundary.as_usize(),
                                affinity == Affinity::Right,
                            );
                            assert_eq!(via_effect.offset.as_usize(), via_map);
                            assert_eq!(via_map, via_oracle);
                            assert!(after.validate_offset(via_map).is_ok());
                        }
                    }
                    (fwd_map, inv_map, start, end, inv_end)
                }
                other => panic!("diff mismatch with oracle: {other:?}"),
            };

        // Common prefix and common suffix clips agree identically and round-trip through diff maps.
        let before_prefix = Clip::from_range(before, 0..prefix).unwrap();
        let after_prefix = Clip::from_range(after, 0..prefix).unwrap();
        assert_eq!(before_prefix.as_str(), after_prefix.as_str());
        assert_eq!(diff_map.map_offset(prefix, Affinity::Left).unwrap(), prefix);
        assert_eq!(
            inv_diff_map.map_offset(prefix, Affinity::Left).unwrap(),
            prefix
        );

        let before_suffix = Clip::from_range(before, before_core_end..before.len_bytes()).unwrap();
        let after_suffix = Clip::from_range(after, after_core_end..after.len_bytes()).unwrap();
        assert_eq!(before_suffix.as_str(), after_suffix.as_str());

        // Check untouched suffix interior boundaries map cleanly and invert identically.
        for boundary in before
            .char_boundaries()
            .into_iter()
            .filter(|b| b.as_usize() > before_core_end)
        {
            let pos = boundary.as_usize();
            let mapped_l = diff_map.map_offset(pos, Affinity::Left).unwrap();
            let mapped_r = diff_map.map_offset(pos, Affinity::Right).unwrap();
            assert_eq!(mapped_l, mapped_r);
            assert!(after.validate_offset(mapped_l).is_ok());
            assert_eq!(inv_diff_map.map_offset(mapped_l, Affinity::Left), Ok(pos));
            assert_eq!(inv_diff_map.map_offset(mapped_l, Affinity::Right), Ok(pos));

            // Nested clip inside the common suffix matches between before and after.
            let sub_before = ByteSpan::validate_in(before, before_core_end, pos).unwrap();
            let sub_after = ByteSpan::validate_in(after, after_core_end, mapped_l).unwrap();
            assert_eq!(
                before_suffix.clip(sub_before).unwrap().as_str(),
                after_suffix.clip(sub_after).unwrap().as_str()
            );
        }

        // Byte-limit truncation and line clipping on `after` match independent oracles.
        let full_after = ByteSpan::full_of(after);
        let full_clip = clip(after, full_after).unwrap();
        let mut budgets = vec![0, 1, 2, 3, 4, after.len_bytes(), after.len_bytes() + 1];
        if after.len_bytes() > 4 {
            budgets.push(after.len_bytes() / 2);
        }
        for budget in budgets {
            let (limited, truncated) = clip_with_byte_limit(after, full_after, budget).unwrap();
            let (expected_slice, expected_truncated) =
                oracle::truncate_utf8(after.as_str(), budget);
            assert_eq!(limited.as_str(), expected_slice);
            assert_eq!(truncated, expected_truncated);
            assert_eq!(limited.origin().slice_in(after).unwrap(), expected_slice);
            assert_eq!(full_clip.clip(limited.origin()).unwrap(), limited);
        }

        let line_index = LineIndex::from_text(after);
        let expected_lines = oracle::line_spans(after.as_str());
        assert_eq!(line_index.line_count(), expected_lines.len());
        for start_line in 0..=expected_lines.len() {
            for end_line in start_line..=expected_lines.len() {
                let clipped = clip_lines(after, &line_index, start_line..end_line).unwrap();
                let expected_start = if start_line == expected_lines.len() {
                    after.len_bytes()
                } else {
                    expected_lines[start_line].0
                };
                let expected_end = if start_line == end_line {
                    expected_start
                } else {
                    expected_lines[end_line - 1].2
                };
                assert_eq!(
                    clipped.origin(),
                    ByteSpan::validate_in(after, expected_start, expected_end).unwrap()
                );
                assert_eq!(
                    clipped.as_str(),
                    &after.as_str()[expected_start..expected_end]
                );
            }
        }
    }

    fn verify_sequence(initial: &str, steps: &[RawStep]) {
        let chain = build_chain(initial, steps);
        let first_text = &chain.texts[0];
        let final_text = chain.texts.last().unwrap();

        // (2) Exact multi-step inverse-splice restoration (byte-for-byte at every step).
        let mut restored = final_text.clone();
        for idx in (0..steps.len()).rev() {
            restored = chain.inverse_splices[idx].apply(&restored).unwrap();
            assert_eq!(restored.as_bytes(), chain.texts[idx].as_bytes());
            assert_eq!(restored.as_bytes(), chain.oracle_snapshots[idx].as_bytes());
        }
        assert_eq!(restored.as_bytes(), initial.as_bytes());

        // (2) Exact multi-step inverse-diff restoration + step-level diff/clip cross-checks.
        let mut step_inverse_diffs: Vec<Option<Diff>> = Vec::with_capacity(steps.len());
        for idx in 0..steps.len() {
            let before = &chain.texts[idx];
            let after = &chain.texts[idx + 1];
            let inv = diff(after, before);
            step_inverse_diffs.push(inv);
        }
        let mut diff_restored = final_text.clone();
        for idx in (0..steps.len()).rev() {
            if let Some(inv_diff) = &step_inverse_diffs[idx] {
                diff_restored = inv_diff.apply(&diff_restored).unwrap();
            }
            assert_eq!(diff_restored.as_bytes(), chain.texts[idx].as_bytes());
        }
        assert_eq!(diff_restored.as_bytes(), initial.as_bytes());

        // (5) Whole-sequence diff/clip/position_map cross-check.
        verify_diff_clip_pair(first_text, final_text);

        // (1) Sequential vs composed position-mapping equivalence (left, right, split).
        let composed_left = compose_left(first_text.len_bytes(), &chain.maps);
        let composed_right = compose_right(final_text.len_bytes(), &chain.maps);
        assert_eq!(composed_left, composed_right);
        assert_eq!(
            PositionMap::identity(first_text.len_bytes())
                .compose(&composed_left)
                .unwrap(),
            composed_left
        );
        assert_eq!(
            composed_left
                .compose(&PositionMap::identity(final_text.len_bytes()))
                .unwrap(),
            composed_left
        );
        if !chain.maps.is_empty() {
            let mid = chain.maps.len() / 2;
            let prefix_map = compose_left(first_text.len_bytes(), &chain.maps[..mid]);
            let suffix_map = compose_right(final_text.len_bytes(), &chain.maps[mid..]);
            assert_eq!(prefix_map.compose(&suffix_map).unwrap(), composed_left);
        }

        let initial_bounds = first_text.char_boundaries();
        let mut prev_mapped = [0usize, 0usize];
        for (bound_idx, &boundary) in initial_bounds.iter().enumerate() {
            let mut pair = [0usize, 0usize];
            for (aff_idx, affinity) in AFFINITIES.into_iter().enumerate() {
                // Step-by-step ReplacementEffect::transport (UTF-8 validated at every step).
                let mut via_effect = Anchor {
                    offset: boundary,
                    affinity,
                };
                // Step-by-step PositionMap::map_offset / map_anchor.
                let mut via_step_offset = boundary.as_usize();
                let mut via_step_anchor = PositionAnchor::from(via_effect);

                for idx in 0..steps.len() {
                    via_effect = chain.effects[idx]
                        .transport(via_effect, &chain.texts[idx], &chain.texts[idx + 1])
                        .unwrap();
                    via_step_offset = chain.maps[idx]
                        .map_offset(via_step_offset, affinity)
                        .unwrap();
                    via_step_anchor = chain.maps[idx].map_anchor(via_step_anchor).unwrap();
                    assert_eq!(via_effect.offset.as_usize(), via_step_offset);
                    assert_eq!(via_step_anchor.offset, via_step_offset);
                    assert_eq!(via_step_anchor.affinity, affinity);
                }

                let via_composed = composed_left
                    .map_offset(boundary.as_usize(), affinity)
                    .unwrap();
                let via_composed_anchor = composed_left
                    .map_anchor(Anchor {
                        offset: boundary,
                        affinity,
                    })
                    .unwrap();
                let via_oracle = oracle::transport_via_delete_then_insert(
                    steps,
                    boundary.as_usize(),
                    affinity == Affinity::Right,
                );

                assert_eq!(via_composed, via_step_offset);
                assert_eq!(via_composed_anchor.offset, via_step_offset);
                assert_eq!(via_composed_anchor.affinity, affinity);
                assert_eq!(via_composed, via_oracle);
                assert!(final_text.validate_offset(via_composed).is_ok());

                if bound_idx > 0 {
                    assert!(prev_mapped[aff_idx] <= via_composed);
                }
                pair[aff_idx] = via_composed;
            }
            assert!(pair[0] <= pair[1]);
            prev_mapped = pair;
        }

        // (1) Range/endpoint transport equivalence across representative boundary pairs.
        let sampled_bounds: Vec<usize> = if initial_bounds.len() <= 10 {
            initial_bounds.iter().map(|b| b.as_usize()).collect()
        } else {
            let step_stride = (initial_bounds.len() / 8).max(1);
            let mut picks: Vec<usize> = initial_bounds
                .iter()
                .step_by(step_stride)
                .map(|b| b.as_usize())
                .collect();
            if *picks.last().unwrap() != first_text.len_bytes() {
                picks.push(first_text.len_bytes());
            }
            picks
        };

        let buffer_id = BufferId::new(1);
        let final_catalog = make_catalog(buffer_id, final_text);
        let composed_rev_map = RevisionedMap {
            map: &composed_left,
            source: Revision::ZERO,
            target: Revision::new(steps.len() as u64),
        };
        let total_revisions = MappingRevisions {
            base: Revision::ZERO,
            target: Revision::new(steps.len() as u64),
        };

        for (i, &start_raw) in sampled_bounds.iter().enumerate() {
            for &end_raw in &sampled_bounds[i..] {
                let start_off = first_text.validate_offset(start_raw).unwrap();
                let end_off = first_text.validate_offset(end_raw).unwrap();
                let initial_span = ByteSpan::new(start_off, end_off).unwrap();

                for sa in AFFINITIES {
                    for ea in AFFINITIES {
                        let initial_anchored = AnchoredSpan {
                            start: PositionAnchor {
                                offset: start_raw,
                                affinity: sa,
                            },
                            end: PositionAnchor {
                                offset: end_raw,
                                affinity: ea,
                            },
                        };
                        let mut step_anchored = initial_anchored;
                        let mut effect_start = Anchor {
                            offset: start_off,
                            affinity: sa,
                        };
                        let mut effect_end = Anchor {
                            offset: end_off,
                            affinity: ea,
                        };

                        // Track AnnotationStore (RetainEmpty and Evict), RevisionedMap,
                        // NeovimExtmark, EmacsOverlay, and AgentAnnotation step-by-step.
                        let base_retain = make_annotation(
                            buffer_id,
                            1,
                            initial_span,
                            sa,
                            ea,
                            CollapsePolicy::RetainEmpty,
                        );
                        let base_evict = make_annotation(
                            buffer_id,
                            2,
                            initial_span,
                            sa,
                            ea,
                            CollapsePolicy::Evict,
                        );
                        let mut store = AnnotationStore::new();
                        let init_catalog = make_catalog(buffer_id, first_text);
                        store.set(&init_catalog, base_retain.clone()).unwrap();
                        store.set(&init_catalog, base_evict.clone()).unwrap();

                        let mut step_ann = base_retain.clone();
                        let mut step_nvim = project_neovim(first_text, &base_retain).unwrap();
                        let mut step_emacs = project_emacs(first_text, &base_retain).unwrap();
                        let mut step_agent_byte =
                            project_agent(first_text, &base_retain, CoordKind::Byte).unwrap();
                        let mut step_agent_utf16 =
                            project_agent(first_text, &base_retain, CoordKind::LineColUtf16)
                                .unwrap();
                        let mut ever_collapsed_or_inverted = false;

                        for idx in 0..steps.len() {
                            step_anchored =
                                chain.maps[idx].map_anchored_span(step_anchored).unwrap();

                            if effect_start.offset <= effect_end.offset {
                                let transported = chain.effects[idx]
                                    .transport_endpoints(
                                        effect_start,
                                        effect_end,
                                        &chain.texts[idx],
                                        &chain.texts[idx + 1],
                                    )
                                    .unwrap();
                                match transported {
                                    TransportedSpan::Range { start, end } => {
                                        assert!(start.offset < end.offset);
                                        effect_start = start;
                                        effect_end = end;
                                    }
                                    TransportedSpan::Collapsed { start, end } => {
                                        assert_eq!(start.offset, end.offset);
                                        effect_start = start;
                                        effect_end = end;
                                        ever_collapsed_or_inverted = true;
                                    }
                                    TransportedSpan::Inverted { start, end } => {
                                        assert!(start.offset > end.offset);
                                        effect_start = start;
                                        effect_end = end;
                                        ever_collapsed_or_inverted = true;
                                    }
                                }
                            } else {
                                // Already-inverted endpoints are rejected by transport_endpoints,
                                // while individual transport tracks each endpoint accurately.
                                assert_eq!(
                                    chain.effects[idx].transport_endpoints(
                                        effect_start,
                                        effect_end,
                                        &chain.texts[idx],
                                        &chain.texts[idx + 1],
                                    ),
                                    Err(AnchorError::InvalidBounds)
                                );
                                effect_start = chain.effects[idx]
                                    .transport(
                                        effect_start,
                                        &chain.texts[idx],
                                        &chain.texts[idx + 1],
                                    )
                                    .unwrap();
                                effect_end = chain.effects[idx]
                                    .transport(effect_end, &chain.texts[idx], &chain.texts[idx + 1])
                                    .unwrap();
                                // Once inverted (Right start > Left end), it stays collapsed/inverted.
                                assert!(effect_start.offset >= effect_end.offset);
                                ever_collapsed_or_inverted = true;
                            }

                            assert_eq!(effect_start.offset.as_usize(), step_anchored.start.offset);
                            assert_eq!(effect_end.offset.as_usize(), step_anchored.end.offset);

                            // Advance store and frontend projections by one revision.
                            store
                                .transport(
                                    buffer_id,
                                    chain.effects[idx],
                                    &chain.texts[idx],
                                    &chain.texts[idx + 1],
                                )
                                .unwrap();
                            let rev_from = Revision::new(idx as u64);
                            let rev_to = Revision::new((idx + 1) as u64);
                            let rev_map = RevisionedMap {
                                map: &chain.maps[idx],
                                source: rev_from,
                                target: rev_to,
                            };
                            let revs = MappingRevisions {
                                base: rev_from,
                                target: rev_to,
                            };
                            step_ann = annotation_mapping::map_annotation(
                                &step_ann,
                                &chain.texts[idx],
                                &chain.texts[idx + 1],
                                &rev_map,
                                revs,
                            )
                            .unwrap();
                            step_nvim = map_neovim(
                                &step_nvim,
                                &chain.texts[idx],
                                &chain.texts[idx + 1],
                                &rev_map,
                                revs,
                            )
                            .unwrap();
                            step_emacs = map_emacs(
                                &step_emacs,
                                &chain.texts[idx],
                                &chain.texts[idx + 1],
                                &rev_map,
                                revs,
                            )
                            .unwrap();
                            step_agent_byte = map_agent(
                                &step_agent_byte,
                                &chain.texts[idx],
                                &chain.texts[idx + 1],
                                &rev_map,
                                revs,
                            )
                            .unwrap();
                            step_agent_utf16 = map_agent(
                                &step_agent_utf16,
                                &chain.texts[idx],
                                &chain.texts[idx + 1],
                                &rev_map,
                                revs,
                            )
                            .unwrap();
                        }

                        // Composed AnchoredSpan equals sequential AnchoredSpan and oracle.
                        let composed_anchored =
                            composed_left.map_anchored_span(initial_anchored).unwrap();
                        assert_eq!(composed_anchored, step_anchored);
                        let expected_start = oracle::transport_via_delete_then_insert(
                            steps,
                            start_raw,
                            sa == Affinity::Right,
                        );
                        let expected_end = oracle::transport_via_delete_then_insert(
                            steps,
                            end_raw,
                            ea == Affinity::Right,
                        );
                        assert_eq!(composed_anchored.start.offset, expected_start);
                        assert_eq!(composed_anchored.end.offset, expected_end);

                        // (4) Multi-revision marker/extmark tracking matches composed RevisionedMap
                        // and oracle normalization.
                        let (norm_s, norm_e) = oracle::normalize_span(expected_start, expected_end);
                        let expected_norm_span =
                            ByteSpan::validate_in(final_text, norm_s, norm_e).unwrap();
                        let composed_ann = annotation_mapping::map_annotation(
                            &base_retain,
                            first_text,
                            final_text,
                            &composed_rev_map,
                            total_revisions,
                        )
                        .unwrap();
                        assert_eq!(composed_ann.span, expected_norm_span);
                        assert_eq!(step_ann, composed_ann);
                        assert_eq!(store.get(base_retain.key), Some(&composed_ann));
                        if ever_collapsed_or_inverted {
                            assert_eq!(store.get(base_evict.key), None);
                        } else {
                            let mut expected_evict = base_evict.clone();
                            expected_evict.span = expected_norm_span;
                            assert_eq!(store.get(base_evict.key), Some(&expected_evict));
                        }

                        let composed_nvim = map_neovim(
                            &project_neovim(first_text, &base_retain).unwrap(),
                            first_text,
                            final_text,
                            &composed_rev_map,
                            total_revisions,
                        )
                        .unwrap();
                        assert_eq!(step_nvim, composed_nvim);
                        assert_eq!(
                            unproject_neovim(final_text, &composed_nvim).unwrap(),
                            composed_ann
                        );

                        let composed_emacs = map_emacs(
                            &project_emacs(first_text, &base_retain).unwrap(),
                            first_text,
                            final_text,
                            &composed_rev_map,
                            total_revisions,
                        )
                        .unwrap();
                        assert_eq!(step_emacs, composed_emacs);
                        assert_eq!(
                            unproject_emacs(final_text, &composed_emacs).unwrap(),
                            composed_ann
                        );

                        let composed_agent_byte = map_agent(
                            &project_agent(first_text, &base_retain, CoordKind::Byte).unwrap(),
                            first_text,
                            final_text,
                            &composed_rev_map,
                            total_revisions,
                        )
                        .unwrap();
                        assert_eq!(step_agent_byte, composed_agent_byte);
                        assert_eq!(
                            unproject_agent(final_text, &composed_agent_byte).unwrap(),
                            composed_ann
                        );

                        let composed_agent_utf16 = map_agent(
                            &project_agent(first_text, &base_retain, CoordKind::LineColUtf16)
                                .unwrap(),
                            first_text,
                            final_text,
                            &composed_rev_map,
                            total_revisions,
                        )
                        .unwrap();
                        assert_eq!(step_agent_utf16, composed_agent_utf16);
                        assert_eq!(
                            unproject_agent(final_text, &composed_agent_utf16).unwrap(),
                            composed_ann
                        );
                    }
                }
            }
        }

        // Verify map_store composition equivalence on a multi-annotation store.
        if !steps.is_empty() {
            let init_catalog = make_catalog(buffer_id, first_text);
            let mut batch_store = AnnotationStore::new();
            let full_span = ByteSpan::full_of(first_text);
            for (idx, (sa, ea)) in [
                (Affinity::Left, Affinity::Left),
                (Affinity::Left, Affinity::Right),
                (Affinity::Right, Affinity::Left),
                (Affinity::Right, Affinity::Right),
            ]
            .into_iter()
            .enumerate()
            {
                batch_store
                    .set(
                        &init_catalog,
                        make_annotation(
                            buffer_id,
                            idx as u64,
                            full_span,
                            sa,
                            ea,
                            CollapsePolicy::RetainEmpty,
                        ),
                    )
                    .unwrap();
            }
            let mut step_store = batch_store.clone();
            for idx in 0..steps.len() {
                let target_cat = make_catalog(buffer_id, &chain.texts[idx + 1]);
                let rev_from = Revision::new(idx as u64);
                let rev_to = Revision::new((idx + 1) as u64);
                step_store = annotation_mapping::map_store(
                    &step_store,
                    buffer_id,
                    &chain.texts[idx],
                    &target_cat,
                    &RevisionedMap {
                        map: &chain.maps[idx],
                        source: rev_from,
                        target: rev_to,
                    },
                    MappingRevisions {
                        base: rev_from,
                        target: rev_to,
                    },
                )
                .unwrap();
            }
            let direct_store = annotation_mapping::map_store(
                &batch_store,
                buffer_id,
                first_text,
                &final_catalog,
                &composed_rev_map,
                total_revisions,
            )
            .unwrap();
            assert_eq!(step_store, direct_store);
        }

        // (3) Untouched-position exact identity: forward then inverse is identity on every
        // boundary outside every edited span.
        let reversed_inverse_maps: Vec<PositionMap> =
            chain.inverse_maps.iter().rev().cloned().collect();
        let composed_inverse = compose_left(final_text.len_bytes(), &reversed_inverse_maps);
        let roundtrip_map = composed_left.compose(&composed_inverse).unwrap();
        let untouched = oracle::globally_untouched_boundaries(initial, steps);
        for &(orig_offset, final_offset) in &untouched {
            for affinity in AFFINITIES {
                assert_eq!(
                    composed_left.map_offset(orig_offset, affinity),
                    Ok(final_offset)
                );
                assert_eq!(
                    composed_inverse.map_offset(final_offset, affinity),
                    Ok(orig_offset)
                );
                assert_eq!(
                    roundtrip_map.map_offset(orig_offset, affinity),
                    Ok(orig_offset)
                );
            }
            assert!(final_text.validate_offset(final_offset).is_ok());
        }
        // Any span whose closed interval stays strictly outside every edited span preserves exact
        // clipped bytes between initial and final text.
        for pair in untouched.windows(2) {
            let (orig_a, final_a) = pair[0];
            let (orig_b, final_b) = pair[1];
            if let Some((exp_a, exp_b)) = oracle::span_untouched_throughout(steps, orig_a, orig_b) {
                assert_eq!((final_a, final_b), (exp_a, exp_b));
                assert_eq!(
                    Clip::from_range(first_text, orig_a..orig_b)
                        .unwrap()
                        .as_str(),
                    Clip::from_range(final_text, final_a..final_b)
                        .unwrap()
                        .as_str()
                );
            }
        }

        // (4) Step-level strict deleted/replaced interior collapse laws.
        for idx in 0..steps.len() {
            let before = &chain.texts[idx];
            let after = &chain.texts[idx + 1];
            let raw = &steps[idx];
            let remaining_map = compose_left(after.len_bytes(), &chain.maps[idx + 1..]);
            for boundary in before.char_boundaries() {
                let pos = boundary.as_usize();
                if raw.start < pos && pos < raw.end {
                    let left_here = chain.effects[idx]
                        .transport(
                            Anchor {
                                offset: boundary,
                                affinity: Affinity::Left,
                            },
                            before,
                            after,
                        )
                        .unwrap();
                    let right_here = chain.effects[idx]
                        .transport(
                            Anchor {
                                offset: boundary,
                                affinity: Affinity::Right,
                            },
                            before,
                            after,
                        )
                        .unwrap();
                    assert_eq!(left_here.offset.as_usize(), raw.start);
                    assert_eq!(right_here.offset.as_usize(), raw.start + raw.inserted.len());
                    if raw.inserted.is_empty() {
                        assert_eq!(left_here.offset, right_here.offset);
                    } else {
                        assert!(left_here.offset < right_here.offset);
                    }
                    // Collapsed interior survives remaining steps consistently via composition.
                    let mut left_seq = left_here;
                    let mut right_seq = right_here;
                    for later in idx + 1..steps.len() {
                        left_seq = chain.effects[later]
                            .transport(left_seq, &chain.texts[later], &chain.texts[later + 1])
                            .unwrap();
                        right_seq = chain.effects[later]
                            .transport(right_seq, &chain.texts[later], &chain.texts[later + 1])
                            .unwrap();
                    }
                    assert_eq!(
                        remaining_map.map_offset(raw.start, Affinity::Left),
                        Ok(left_seq.offset.as_usize())
                    );
                    assert_eq!(
                        remaining_map.map_offset(raw.start + raw.inserted.len(), Affinity::Right),
                        Ok(right_seq.offset.as_usize())
                    );
                    assert!(left_seq.offset <= right_seq.offset);
                }
            }
        }
    }

    #[test]
    fn deterministic_multibyte_crlf_and_empty_transform_laws() {
        let cases = vec![
            // Empty text -> insert -> replace -> delete back to empty.
            SequenceCase {
                initial: String::new(),
                steps: vec![
                    step(0, 0, "界😀"),
                    step(3, 7, "e\u{0301}\r\n"),
                    step(0, 8, ""),
                    step(0, 0, ""),
                ],
            },
            // Pure ASCII with interleaved no-ops, deletions, insertions, and replacements.
            SequenceCase {
                initial: "abcdef".to_owned(),
                steps: vec![
                    step(2, 2, ""),
                    step(1, 4, "XY"),
                    step(0, 0, ">>"),
                    step(5, 7, "Z"),
                    step(2, 5, ""),
                ],
            },
            // Shared leading UTF-8 bytes across 2-byte, 3-byte CJK, and 4-byte emoji.
            SequenceCase {
                initial: "é界😀".to_owned(),
                steps: vec![
                    step(0, 2, "è"),
                    step(2, 5, "畑"),
                    step(5, 9, "😁"),
                    step(2, 9, "日本語"),
                ],
            },
            // Mixed LF, CRLF, and bare CR splitting and rejoining.
            SequenceCase {
                initial: "a\r\nb\nc\rd\r\n".to_owned(),
                steps: vec![
                    step(1, 2, ""),     // CRLF -> LF
                    step(4, 4, "\r"),   // LF -> CRLF
                    step(6, 7, "\r\n"), // bare CR -> CRLF
                    step(0, 1, "界😀"), // prefix replacement
                    step(8, 12, ""),    // delete across line breaks
                ],
            },
            // Combining marks + CJK + emoji + CRLF torture sequence.
            SequenceCase {
                initial: "a\r\n界😀\ne\u{0301}\r\n終".to_owned(),
                steps: vec![
                    step(3, 10, "新"),  // replace 界😀 (7 bytes) with 新 (3 bytes)
                    step(6, 7, "\r\n"), // replace LF with CRLF
                    step(8, 11, ""),    // delete e + combining acute (3 bytes)
                    step(0, 0, "😀\n"), // prepend emoji + LF
                ],
            },
            // Zero-step identity sequence on multi-byte text.
            SequenceCase {
                initial: "日本語\r\n😀".to_owned(),
                steps: Vec::new(),
            },
        ];

        for case in &cases {
            verify_sequence(&case.initial, &case.steps);
        }
    }

    #[test]
    fn randomized_multistep_transform_laws() {
        assert_law(SEEDS, generate_sequence_cases, |cases| {
            for case in cases {
                verify_sequence(&case.initial, &case.steps);
            }
        });
    }

    #[test]
    fn untouched_regions_exact_identity_and_patch_equivalence() {
        // Explicit multi-step edits with untouched prefix, untouched middle, and untouched suffix.
        let initial = "前a\r\n[編集1]中界😀[編集2]後e\u{0301}\n";
        let edit1_start = initial.find("[編集1]").unwrap();
        let edit1_end = edit1_start + "[編集1]".len();
        let steps = vec![
            step(edit1_start, edit1_end, "😀\r\n"),
            // Second edit targets "[編集2]" in the updated coordinates.
            {
                let (mid, _) = oracle::apply_step(initial, &step(edit1_start, edit1_end, "😀\r\n"));
                let edit2_start = mid.find("[編集2]").unwrap();
                let edit2_end = edit2_start + "[編集2]".len();
                step(edit2_start, edit2_end, "X")
            },
        ];
        let untouched = oracle::globally_untouched_boundaries(initial, &steps);
        // Confirm nontrivial untouched boundaries strictly outside every edited span
        // exist in prefix, middle, and suffix, while closed edit edges are distinguished.
        let edit1_right_edge = initial.find('中').unwrap();
        let middle_pos = initial.find('界').unwrap();
        let suffix_pos = initial.find('e').unwrap();
        assert_eq!(edit1_right_edge, edit1_end);
        assert!(!untouched.iter().any(|&(orig, _)| orig == edit1_right_edge));
        assert!(untouched.iter().any(|&(orig, _)| orig == 0));
        assert!(untouched.iter().any(|&(orig, _)| orig == middle_pos));
        assert!(untouched.iter().any(|&(orig, _)| orig == suffix_pos));
        assert!(untouched.iter().any(|&(orig, _)| orig == initial.len()));
        verify_sequence(initial, &steps);

        // Randomized sequences that intentionally leave untouched prefix/middle/suffix bands.
        assert_law(
            SEEDS,
            |rng: &mut Rng| {
                let mut generated = Vec::new();
                for _ in 0..24 {
                    let prefix = *rng.choose(&["a", "界", "😀\r\n", "e\u{0301}"]).unwrap();
                    let slot_a = *rng.choose(&["x", "日本語", "😀😁", "a\r\nb"]).unwrap();
                    let middle = *rng.choose(&["m", "中界", "\r\n😀"]).unwrap();
                    let slot_b = *rng.choose(&["", "y", "éè", "界\n"]).unwrap();
                    let suffix = *rng.choose(&["z", "終", "😀"]).unwrap();
                    let full = format!("{prefix}{slot_a}{middle}{slot_b}{suffix}");
                    let a_start = prefix.len();
                    let a_end = a_start + slot_a.len();
                    let b_start = a_end + middle.len();
                    let b_end = b_start + slot_b.len();
                    let ins_a = *rng.choose(PAYLOADS).unwrap();
                    let ins_b = *rng.choose(PAYLOADS).unwrap();
                    generated.push((full, (a_start, a_end, ins_a), (b_start, b_end, ins_b)));
                }
                generated
            },
            |cases| {
                for (source, (a_s, a_e, ins_a), (b_s, b_e, ins_b)) in cases {
                    let base = Text::from_str(source);
                    let spl_a = Splice::new(
                        ByteSpan::validate_in(&base, *a_s, *a_e).unwrap(),
                        Text::from_str(ins_a),
                    );
                    let spl_b = Splice::new(
                        ByteSpan::validate_in(&base, *b_s, *b_e).unwrap(),
                        Text::from_str(ins_b),
                    );
                    // Strictly separated splices form a valid Patch.
                    let patch = Patch::new(vec![spl_b.clone(), spl_a.clone()]).unwrap();
                    let (patch_after, patch_inv) = patch.apply_with_inverse(&base).unwrap();
                    assert_eq!(patch_inv.apply(&patch_after), Ok(base.clone()));

                    // Descending-order sequence equals Patch application.
                    let desc_steps = vec![step(*b_s, *b_e, ins_b), step(*a_s, *a_e, ins_a)];
                    let desc_chain = build_chain(source, &desc_steps);
                    assert_eq!(desc_chain.texts.last().unwrap(), &patch_after);

                    // Ascending-order sequence with shifted second splice yields identical map.
                    let delta_a = ins_a.len() as isize - (*a_e - *a_s) as isize;
                    let asc_steps = vec![
                        step(*a_s, *a_e, ins_a),
                        step(
                            (*b_s as isize + delta_a) as usize,
                            (*b_e as isize + delta_a) as usize,
                            ins_b,
                        ),
                    ];
                    let asc_chain = build_chain(source, &asc_steps);
                    assert_eq!(asc_chain.texts.last().unwrap(), &patch_after);

                    let desc_map = compose_left(base.len_bytes(), &desc_chain.maps);
                    let asc_map = compose_left(base.len_bytes(), &asc_chain.maps);
                    let inv_desc = compose_left(
                        patch_after.len_bytes(),
                        &desc_chain
                            .inverse_maps
                            .iter()
                            .rev()
                            .cloned()
                            .collect::<Vec<_>>(),
                    );
                    for boundary in base.char_boundaries() {
                        let p = boundary.as_usize();
                        for affinity in AFFINITIES {
                            let mapped_desc = desc_map.map_offset(p, affinity).unwrap();
                            let mapped_asc = asc_map.map_offset(p, affinity).unwrap();
                            assert_eq!(mapped_desc, mapped_asc);
                            if (p < *a_s || p > *a_e) && (p < *b_s || p > *b_e) {
                                assert_eq!(inv_desc.map_offset(mapped_desc, affinity), Ok(p));
                            }
                        }
                    }

                    // Untouched prefix, middle, and suffix slices survive verbatim.
                    let mid_start = *a_e;
                    let mid_end = *b_s;
                    let mapped_mid_start = desc_map.map_offset(mid_start, Affinity::Right).unwrap();
                    let mapped_mid_end = desc_map.map_offset(mid_end, Affinity::Left).unwrap();
                    assert_eq!(
                        Clip::from_range(&base, mid_start..mid_end)
                            .unwrap()
                            .as_str(),
                        Clip::from_range(&patch_after, mapped_mid_start..mapped_mid_end)
                            .unwrap()
                            .as_str()
                    );
                    verify_sequence(source, &desc_steps);
                }
            },
        );
    }

    #[test]
    fn deleted_interior_collapse_history_and_extmark_revisions() {
        // 1. Multi-step anchor history retention vs single-diff collapse counterexample
        //    (both ASCII and multi-byte UTF-8 CJK/emoji).
        for (source, ins_prefix, expected_final) in
            [("abc", "xy", "xy"), ("界😀", "新\r\n", "新\r\n")]
        {
            let before = Text::from_str(source);
            let step_insert = step(0, 0, ins_prefix);
            let step_delete = step(ins_prefix.len(), ins_prefix.len() + source.len(), "");
            let chain = build_chain(source, &[step_insert.clone(), step_delete.clone()]);
            let after = chain.texts.last().unwrap();
            assert_eq!(after.as_str(), expected_final);

            let composed = compose_left(before.len_bytes(), &chain.maps);
            let end_pos = before.len_bytes();
            // Step 1 shifts end_pos -> ins_prefix.len() + end_pos; Step 2 collapses Left to ins_prefix.len().
            assert_eq!(
                composed.map_offset(end_pos, Affinity::Left),
                Ok(ins_prefix.len())
            );
            assert_eq!(
                composed.map_offset(end_pos, Affinity::Right),
                Ok(ins_prefix.len())
            );

            // Contrast with a single-step minimal diff, which cannot see the intermediate insert-then-delete.
            let single_diff = diff(&before, after).unwrap();
            let single_map = PositionMap::from_replacement(
                before.len_bytes(),
                ByteReplacement {
                    start: single_diff.span.start().as_usize(),
                    end: single_diff.span.end().as_usize(),
                    inserted_len: single_diff.replacement.len_bytes(),
                },
            )
            .unwrap();
            assert_eq!(single_map.map_offset(end_pos, Affinity::Left), Ok(0));
            assert_ne!(
                composed.map_offset(end_pos, Affinity::Left),
                single_map.map_offset(end_pos, Affinity::Left)
            );
            verify_sequence(source, &[step_insert, step_delete]);
        }

        // 2. Exhaustive strictly-interior collapse & inversion across multi-revision edits
        //    on multi-byte text with marker/extmark tracking.
        let initial = "a界😀e\u{0301}z";
        // Boundaries of "a界😀e\u{0301}z": 0('a'), 1('界'), 4('😀'), 8('e'), 9('\u{301}'), 11('z'), 12
        let steps = vec![
            // Replace "界😀e\u{0301}" (1..11) with "新\r\n" (5 bytes).
            // Interior boundaries 4, 8, 9 are strictly inside (1, 11).
            step(1, 11, "新\r\n"),
            // Insert "!" right at the left collapse edge (offset 1).
            step(1, 1, "!"),
            // Insert "?" right at the right collapse edge (offset 1 + 1 + 5 = 7).
            step(7, 7, "?"),
            // Delete the inserted "新" (2..5) so both collapsed trajectories shift again.
            step(2, 5, ""),
        ];
        let chain = build_chain(initial, &steps);
        let composed = compose_left(chain.texts[0].len_bytes(), &chain.maps);
        // Trace interior boundary 4 ('😀'):
        // Left:  4 -> 1 (step 0) -> 1 (step 1, Left at insert 1) -> 1 (step 2) -> 1 (step 3)
        // Right: 4 -> 6 (step 0) -> 7 (step 1) -> 8 (step 2, Right at insert 7) -> 5 (step 3, shifted by -3)
        for interior in [4usize, 8, 9] {
            assert_eq!(composed.map_offset(interior, Affinity::Left), Ok(1));
            assert_eq!(composed.map_offset(interior, Affinity::Right), Ok(5));
        }

        // Span [4, 9) with (Right, Left) inverts at step 0 to (6, 1) and tracks across all revisions:
        // Unnormalized AnchoredSpan ends at (5, 1) (still inverted);
        // Normalized Annotation / NeovimExtmark / EmacsOverlay collapses at step 0 to empty at 6,
        // and follows start (Right) through steps 1..3 to empty span [5, 5)!
        let inverted_raw = composed
            .map_anchored_span(AnchoredSpan {
                start: PositionAnchor {
                    offset: 4,
                    affinity: Affinity::Right,
                },
                end: PositionAnchor {
                    offset: 9,
                    affinity: Affinity::Left,
                },
            })
            .unwrap();
        assert_eq!((inverted_raw.start.offset, inverted_raw.end.offset), (5, 1));

        let buffer_id = BufferId::new(9);
        let init_text = &chain.texts[0];
        let final_text = chain.texts.last().unwrap();
        let base_ann = make_annotation(
            buffer_id,
            42,
            ByteSpan::validate_in(init_text, 4, 9).unwrap(),
            Affinity::Right,
            Affinity::Left,
            CollapsePolicy::RetainEmpty,
        );
        let rev_composed = RevisionedMap {
            map: &composed,
            source: Revision::ZERO,
            target: Revision::new(steps.len() as u64),
        };
        let total_revs = MappingRevisions {
            base: Revision::ZERO,
            target: Revision::new(steps.len() as u64),
        };
        let mapped_ann = annotation_mapping::map_annotation(
            &base_ann,
            init_text,
            final_text,
            &rev_composed,
            total_revs,
        )
        .unwrap();
        assert_eq!(
            mapped_ann.span,
            ByteSpan::validate_in(final_text, 5, 5).unwrap()
        );
        let extmark = project_neovim(init_text, &base_ann).unwrap();
        let mapped_extmark =
            map_neovim(&extmark, init_text, final_text, &rev_composed, total_revs).unwrap();
        assert_eq!(
            unproject_neovim(final_text, &mapped_extmark).unwrap(),
            mapped_ann
        );
        verify_sequence(initial, &steps);

        // 3. Verify RevisionedMap::compose step-by-step chaining produces the exact same
        //    composed RevisionedMap across multi-revision histories.
        let r01 = RevisionedMap {
            map: &chain.maps[0],
            source: Revision::ZERO,
            target: Revision::new(1),
        };
        let r12 = RevisionedMap {
            map: &chain.maps[1],
            source: Revision::new(1),
            target: Revision::new(2),
        };
        let r23 = RevisionedMap {
            map: &chain.maps[2],
            source: Revision::new(2),
            target: Revision::new(3),
        };
        let r34 = RevisionedMap {
            map: &chain.maps[3],
            source: Revision::new(3),
            target: Revision::new(4),
        };
        let chained = r01
            .compose(&r12)
            .unwrap()
            .as_revisioned()
            .compose(&r23)
            .unwrap()
            .as_revisioned()
            .compose(&r34)
            .unwrap();
        assert_eq!(
            annotation_mapping::map_annotation(
                &base_ann,
                init_text,
                final_text,
                &chained.as_revisioned(),
                total_revs
            )
            .unwrap(),
            mapped_ann
        );
    }

    #[test]
    fn diff_clip_position_map_cross_checks() {
        // Targeted tricky pairs: shared leading UTF-8 bytes, overlapping repeats, LF/CRLF, empty.
        for (a, b) in [
            ("", ""),
            ("", "😀"),
            ("😀", ""),
            ("é", "è"),
            ("界", "畑"),
            ("😀", "😁"),
            ("aa", "aaa"),
            ("aaa", "aa"),
            ("aba", "aa"),
            ("a\r\nb", "a\nb"),
            ("a\nb\r\n", "a\r\nb\n"),
            ("界😀\r\n終", "界😁\n終"),
            ("e\u{0301}😀", "e\u{0302}😀"),
        ] {
            let ta = Text::from_str(a);
            let tb = Text::from_str(b);
            verify_diff_clip_pair(&ta, &tb);
            verify_diff_clip_pair(&tb, &ta);
        }

        // Randomized corpus cross-check using repo test_support::{assert_law, texts, Rng}.
        assert_law(
            SEEDS,
            |rng: &mut Rng| {
                let corpus = texts(rng);
                let mut pairs = Vec::with_capacity(64);
                for _ in 0..64 {
                    let a = rng.choose(&corpus).unwrap().clone();
                    let b = rng.choose(&corpus).unwrap().clone();
                    pairs.push((a, b));
                }
                pairs
            },
            |pairs| {
                for (a, b) in pairs {
                    verify_diff_clip_pair(&Text::from_str(a), &Text::from_str(b));
                }
            },
        );
    }

    #[test]
    fn rejects_invalid_out_of_bounds_and_non_boundary_inputs() {
        // 1. UTF-8 invalid byte sequences.
        for bad_bytes in [
            &[0xff][..],
            &[0xc0, 0x80],
            &[0xe3, 0x81],
            &[0xed, 0xa0, 0x80],
            &[0xf0, 0x9f, 0x98],
            &[0xf4, 0x90, 0x80, 0x80],
        ] {
            assert_eq!(Text::from_bytes(bad_bytes), Err(Utf8Error::InvalidUtf8));
        }

        // 2. AnchorError::InvalidBounds and AnchorError::Overflow.
        let before = Text::from_str("a界😀z");
        let after_ok = Text::from_str("a新z");
        let effect = ReplacementEffect {
            span: ByteSpan::validate_in(&before, 1, 8).unwrap(),
            inserted_len: "新".len(),
        };
        let anchor_zero = Anchor {
            offset: before.validate_offset(0).unwrap(),
            affinity: Affinity::Left,
        };
        // Exterior prefix mismatch.
        assert_eq!(
            effect.transport(anchor_zero, &before, &Text::from_str("X新z")),
            Err(AnchorError::InvalidBounds)
        );
        // Exterior suffix mismatch.
        assert_eq!(
            effect.transport(anchor_zero, &before, &Text::from_str("a新X")),
            Err(AnchorError::InvalidBounds)
        );
        // Target length mismatch.
        assert_eq!(
            effect.transport(anchor_zero, &before, &Text::from_str("a新zz")),
            Err(AnchorError::InvalidBounds)
        );
        // Target inserted_end falls inside a multi-byte scalar (same total byte length 5!).
        let bad_target = Text::from_str("a😀"); // len 5 == 1 + 3 + 1, but offset 4 is inside '😀' (1..5)
        assert_eq!(bad_target.len_bytes(), after_ok.len_bytes());
        assert_eq!(
            effect.transport(anchor_zero, &before, &bad_target),
            Err(AnchorError::InvalidBounds)
        );
        // Inverted endpoints rejected by transport_endpoints.
        assert_eq!(
            effect.transport_endpoints(
                Anchor {
                    offset: before.validate_offset(8).unwrap(),
                    affinity: Affinity::Left,
                },
                Anchor {
                    offset: before.validate_offset(1).unwrap(),
                    affinity: Affinity::Right,
                },
                &before,
                &after_ok,
            ),
            Err(AnchorError::InvalidBounds)
        );
        // Overflow on inserted_end and on resulting length.
        assert_eq!(
            ReplacementEffect {
                span: ByteSpan::validate_in(&before, 1, 1).unwrap(),
                inserted_len: usize::MAX,
            }
            .transport(anchor_zero, &before, &after_ok),
            Err(AnchorError::Overflow)
        );
        assert_eq!(
            ReplacementEffect {
                span: ByteSpan::validate_in(&before, 0, 1).unwrap(),
                inserted_len: usize::MAX,
            }
            .transport(anchor_zero, &before, &after_ok),
            Err(AnchorError::Overflow)
        );

        // 3. PositionMapError variants.
        assert_eq!(
            PositionMap::from_replacement(
                4,
                ByteReplacement {
                    start: 3,
                    end: 1,
                    inserted_len: 0,
                },
            ),
            Err(PositionMapError::InvalidReplacement {
                start: 3,
                end: 1,
                source_len: 4,
            })
        );
        assert_eq!(
            PositionMap::from_replacement(
                4,
                ByteReplacement {
                    start: 0,
                    end: 5,
                    inserted_len: 0,
                },
            ),
            Err(PositionMapError::InvalidReplacement {
                start: 0,
                end: 5,
                source_len: 4,
            })
        );
        assert_eq!(
            PositionMap::from_replacement(
                4,
                ByteReplacement {
                    start: 1,
                    end: 2,
                    inserted_len: usize::MAX,
                },
            ),
            Err(PositionMapError::Overflow)
        );
        let map_a = PositionMap::identity(4);
        let map_b = PositionMap::identity(5);
        assert_eq!(
            map_a.compose(&map_b),
            Err(PositionMapError::BoundaryMismatch {
                target_len: 4,
                source_len: 5,
            })
        );
        assert_eq!(
            map_a.map_offset(5, Affinity::Left),
            Err(PositionMapError::OutOfBounds {
                offset: 5,
                source_len: 4,
            })
        );
        assert_eq!(
            map_a.map_anchored_span(AnchoredSpan {
                start: PositionAnchor {
                    offset: 0,
                    affinity: Affinity::Left,
                },
                end: PositionAnchor {
                    offset: 5,
                    affinity: Affinity::Right,
                },
            }),
            Err(PositionMapError::OutOfBounds {
                offset: 5,
                source_len: 4,
            })
        );

        // 4. PatchError::Contact and SpliceError::InvertedRange conversion.
        let s0 = ByteSpan::validate_in(&before, 0, 1).unwrap();
        let s1 = ByteSpan::validate_in(&before, 1, 4).unwrap();
        assert_eq!(
            Patch::new(vec![Splice::delete(s0), Splice::delete(s1)]),
            Err(PatchError::Contact {
                previous: s0,
                next: s1,
            })
        );
        assert_eq!(
            SpliceError::from(ByteSpan::validate_in(&before, 8, 1).unwrap_err()),
            SpliceError::InvertedRange { start: 8, end: 1 }
        );

        // 5. ClipError variants including stale LineIndex after a line-changing splice.
        let multiline_before = Text::from_str("a\r\n界\nb");
        let stale_index = LineIndex::from_text(&multiline_before);
        let multiline_after = Splice::insert(
            multiline_before.validate_offset(1).unwrap(),
            Text::from_str("\n"),
        )
        .apply(&multiline_before)
        .unwrap();
        assert_eq!(
            clip_lines(&multiline_after, &stale_index, 0..1),
            Err(ClipError::IndexMismatch)
        );
        let fresh_index = LineIndex::from_text(&multiline_after);
        assert_eq!(
            clip_lines(&multiline_after, &fresh_index, Range { start: 2, end: 1 }),
            Err(ClipError::InvertedLines { start: 2, end: 1 })
        );
        assert_eq!(
            clip_lines(
                &multiline_after,
                &fresh_index,
                0..fresh_index.line_count() + 1
            ),
            Err(ClipError::LineOutOfRange {
                line: fresh_index.line_count() + 1,
                line_count: fresh_index.line_count(),
            })
        );

        // 6. AnnotationMappingError: revision gap, length mismatch, and invalid target boundary.
        let rev_one = RevisionedMap {
            map: &map_a,
            source: Revision::ZERO,
            target: Revision::new(1),
        };
        let rev_gap = RevisionedMap {
            map: &map_a,
            source: Revision::new(2),
            target: Revision::new(3),
        };
        assert_eq!(
            rev_one.compose(&rev_gap),
            Err(AnnotationMappingError::RevisionMismatch {
                expected: Revision::new(2),
                actual: Revision::new(1),
            })
        );
        let ascii4 = Text::from_str("abcd");
        let emoji4 = Text::from_str("😀");
        let ann = make_annotation(
            BufferId::new(1),
            1,
            ByteSpan::validate_in(&ascii4, 1, 2).unwrap(),
            Affinity::Left,
            Affinity::Right,
            CollapsePolicy::RetainEmpty,
        );
        let same_revs = MappingRevisions {
            base: Revision::ZERO,
            target: Revision::new(1),
        };
        assert_eq!(
            annotation_mapping::map_annotation(&ann, &ascii4, &emoji4, &rev_one, same_revs),
            Err(AnnotationMappingError::InvalidOffset(
                Utf8Error::NotCharBoundary { offset: 1 }
            ))
        );
        assert_eq!(
            annotation_mapping::map_annotation(&ann, &emoji4, &ascii4, &rev_one, same_revs),
            Err(AnnotationMappingError::InvalidSpan(
                SpanError::NotCharBoundary { offset: 1 }
            ))
        );
        assert_eq!(
            annotation_mapping::map_annotation(
                &ann,
                &Text::from_str("ab"),
                &ascii4,
                &rev_one,
                same_revs
            ),
            Err(AnnotationMappingError::SourceLengthMismatch {
                expected: 4,
                actual: 2,
            })
        );
        assert_eq!(
            annotation_mapping::map_annotation(
                &ann,
                &ascii4,
                &Text::from_str("ab"),
                &rev_one,
                same_revs
            ),
            Err(AnnotationMappingError::TargetLengthMismatch {
                expected: 4,
                actual: 2,
            })
        );

        // 7. Randomized cross-module rejection of every non-boundary, out-of-bounds, and
        //    inverted offset across generated UTF-8/CJK/emoji/CRLF texts.
        assert_law(SEEDS, texts, |corpus| {
            for source in corpus {
                let text = Text::from_str(source);
                let len = text.len_bytes();
                let foreign = Text::from_str(&"a".repeat(len + 2));
                let beyond_offset = foreign.validate_offset(len + 1).unwrap();
                let zero = text.validate_offset(0).unwrap();

                // Out-of-bounds rejections across utf8, span, splice, diff, clip, position_map.
                assert_eq!(
                    text.validate_offset(len + 1),
                    Err(Utf8Error::OutOfBounds {
                        offset: len + 1,
                        len,
                    })
                );
                assert_eq!(
                    text.split_at(beyond_offset),
                    Err(Utf8Error::OutOfBounds {
                        offset: len + 1,
                        len,
                    })
                );
                assert_eq!(
                    ByteSpan::validate_in(&text, 0, len + 1),
                    Err(SpanError::OutOfBounds {
                        offset: len + 1,
                        len,
                    })
                );
                let oob_span = ByteSpan::new(zero, beyond_offset).unwrap();
                assert_eq!(
                    Splice::delete(oob_span).apply(&text),
                    Err(SpliceError::OutOfBounds {
                        offset: len + 1,
                        len,
                    })
                );
                assert_eq!(
                    Splice::delete(oob_span).invert(&text),
                    Err(SpliceError::OutOfBounds {
                        offset: len + 1,
                        len,
                    })
                );
                assert_eq!(
                    Diff {
                        span: oob_span,
                        replacement: Text::empty(),
                    }
                    .apply(&text),
                    Err(SpanError::OutOfBounds {
                        offset: len + 1,
                        len,
                    })
                );
                assert_eq!(
                    clip(&text, oob_span),
                    Err(ClipError::Span(SpanError::OutOfBounds {
                        offset: len + 1,
                        len,
                    }))
                );
                assert_eq!(
                    PositionMap::identity(len).map_offset(len + 1, Affinity::Left),
                    Err(PositionMapError::OutOfBounds {
                        offset: len + 1,
                        source_len: len,
                    })
                );

                // Every non-character-boundary offset rejected cleanly by every module.
                for offset in 0..len {
                    if source.is_char_boundary(offset) {
                        continue;
                    }
                    let bad_off = foreign.validate_offset(offset).unwrap();
                    let bad_span = ByteSpan::new(zero, bad_off).unwrap();
                    assert_eq!(
                        text.validate_offset(offset),
                        Err(Utf8Error::NotCharBoundary { offset })
                    );
                    assert_eq!(
                        text.split_at(bad_off),
                        Err(Utf8Error::NotCharBoundary { offset })
                    );
                    assert_eq!(
                        ByteSpan::validate_in(&text, 0, offset),
                        Err(SpanError::NotCharBoundary { offset })
                    );
                    assert_eq!(
                        bad_span.slice_in(&text),
                        Err(SpanError::NotCharBoundary { offset })
                    );
                    assert_eq!(
                        Splice::delete(bad_span).apply(&text),
                        Err(SpliceError::NotCharBoundary { offset })
                    );
                    assert_eq!(
                        Splice::delete(bad_span).apply_with_inverse(&text),
                        Err(SpliceError::NotCharBoundary { offset })
                    );
                    assert_eq!(
                        Diff {
                            span: bad_span,
                            replacement: Text::from_str("x"),
                        }
                        .apply(&text),
                        Err(SpanError::NotCharBoundary { offset })
                    );
                    assert_eq!(
                        clip(&text, bad_span),
                        Err(ClipError::Span(SpanError::NotCharBoundary { offset }))
                    );
                    assert_eq!(
                        clip_with_byte_limit(&text, bad_span, len),
                        Err(ClipError::Span(SpanError::NotCharBoundary { offset }))
                    );
                    let noop_effect = ReplacementEffect {
                        span: ByteSpan::empty_at(zero),
                        inserted_len: 0,
                    };
                    assert_eq!(
                        noop_effect.transport(
                            Anchor {
                                offset: bad_off,
                                affinity: Affinity::Left,
                            },
                            &text,
                            &text,
                        ),
                        Err(AnchorError::InvalidBounds)
                    );
                }

                // Inverted ranges and OutsideParent clips rejected cleanly.
                let bounds = text.char_boundaries();
                if bounds.len() >= 2 {
                    let last = *bounds.last().unwrap();
                    assert_eq!(
                        text.slice(last, zero),
                        Err(Utf8Error::InvertedRange {
                            start: last.as_usize(),
                            end: 0,
                        })
                    );
                    assert_eq!(
                        ByteSpan::new(last, zero),
                        Err(SpanError::InvertedRange {
                            start: last.as_usize(),
                            end: 0,
                        })
                    );
                    assert_eq!(
                        Clip::from_range(
                            &text,
                            Range {
                                start: last.as_usize(),
                                end: 0,
                            }
                        ),
                        Err(ClipError::Span(SpanError::InvertedRange {
                            start: last.as_usize(),
                            end: 0,
                        }))
                    );
                    let parent = Clip::new(&text, ByteSpan::empty_at(last)).unwrap();
                    let outside = ByteSpan::empty_at(zero);
                    assert_eq!(
                        parent.clip(outside),
                        Err(ClipError::OutsideParent {
                            requested: outside,
                            parent: parent.origin(),
                        })
                    );
                }
            }
        });
    }
}
