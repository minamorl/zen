//! Mapping annotations through byte-position maps.
//!
//! Spans are revalidated against the supplied base text before mapping, and
//! retained mapped endpoints against the target text. Text lengths must match the
//! map's domain and codomain, even for an empty store. A map contains neither
//! text nor revision identifiers; callers must ensure it describes the actual
//! edit history between the supplied texts and revisions. Equal lengths alone
//! cannot establish that correspondence.
//! A composed map can validate only its outer texts. Its results agree with
//! sequential mapping when each intermediate mapping succeeds, including UTF-8
//! validation. A composed map cannot detect an invalid intermediate boundary.
//! Use `RevisionedMap::compose` to check intermediate revision continuity;
//! callers remain responsible for validating intermediate texts.
//!
//! An inversion is retained as a point at the mapped start. Only a right
//! start and left end can invert: monotonicity and left <= right preserve that
//! inversion (or equality) through later maps. Therefore replacing its end
//! with its start does not change the normalized result of later maps.

use std::fmt;

use crate::editor::edit::ids::BufferId;
use crate::editor::text::position_map::{PositionMap, PositionMapError};
use crate::editor::text::span::{ByteSpan, SpanError};
use crate::editor::text::utf8::{Text, Utf8Error};
use crate::editor::workspace::annotations::{Annotation, AnnotationError, AnnotationStore};
use crate::editor::workspace::catalog::BufferCatalog;
use crate::editor::workspace::version::Revision;

/// Failure to map an annotation or store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnnotationMappingError {
    /// The base text length does not match the map's source length.
    SourceLengthMismatch { expected: usize, actual: usize },
    /// The target text length does not match the map's target length.
    TargetLengthMismatch { expected: usize, actual: usize },
    /// A supplied revision differs from the map's declared boundary.
    RevisionMismatch {
        expected: Revision,
        actual: Revision,
    },
    /// A selected buffer is missing or closed, or insertion failed.
    Annotation(AnnotationError),
    /// A source span is not valid in the supplied base text.
    InvalidSpan(SpanError),
    /// A mapped endpoint is not a target UTF-8 boundary.
    InvalidOffset(Utf8Error),
    /// The map rejected an endpoint.
    PositionMap(PositionMapError),
}

impl fmt::Display for AnnotationMappingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "annotation mapping error: {self:?}")
    }
}
impl std::error::Error for AnnotationMappingError {}

/// Map geometry paired with its caller-declared revision boundaries.
/// The caller must associate these revisions with the actual edit history.
pub struct RevisionedMap<'a> {
    /// Position map for the edit history.
    pub map: &'a PositionMap,
    /// Source revision of the edit history.
    pub source: Revision,
    /// Target revision of the edit history.
    pub target: Revision,
}

/// An owned map produced by revision-checked composition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposedRevisionedMap {
    map: PositionMap,
    source: Revision,
    target: Revision,
}

impl ComposedRevisionedMap {
    /// Borrows the composed geometry together with its revision boundaries.
    pub fn as_revisioned(&self) -> RevisionedMap<'_> {
        RevisionedMap {
            map: &self.map,
            source: self.source,
            target: self.target,
        }
    }
}

impl RevisionedMap<'_> {
    /// Composes consecutive maps after checking revision and length continuity.
    pub fn compose(
        &self,
        next: &RevisionedMap<'_>,
    ) -> Result<ComposedRevisionedMap, AnnotationMappingError> {
        if self.target != next.source {
            return Err(AnnotationMappingError::RevisionMismatch {
                expected: next.source,
                actual: self.target,
            });
        }
        let map = self
            .map
            .compose(next.map)
            .map_err(AnnotationMappingError::PositionMap)?;
        Ok(ComposedRevisionedMap {
            map,
            source: self.source,
            target: next.target,
        })
    }
}

/// Revisions of the supplied base and destination texts.
#[derive(Clone, Copy)]
pub struct MappingRevisions {
    /// Revision of the base text and annotations.
    pub base: Revision,
    /// Revision of the target text.
    pub target: Revision,
}

fn check_revisions(
    map: &RevisionedMap<'_>,
    revisions: MappingRevisions,
) -> Result<(), AnnotationMappingError> {
    for (expected, actual) in [(map.source, revisions.base), (map.target, revisions.target)] {
        if expected != actual {
            return Err(AnnotationMappingError::RevisionMismatch { expected, actual });
        }
    }
    Ok(())
}

fn validate_lengths(
    base: &Text,
    target: &Text,
    map: &PositionMap,
) -> Result<(), AnnotationMappingError> {
    if base.len_bytes() != map.source_len() {
        return Err(AnnotationMappingError::SourceLengthMismatch {
            expected: map.source_len(),
            actual: base.len_bytes(),
        });
    }
    if target.len_bytes() != map.target_len() {
        return Err(AnnotationMappingError::TargetLengthMismatch {
            expected: map.target_len(),
            actual: target.len_bytes(),
        });
    }
    Ok(())
}

/// Maps both independently affined endpoints, retaining all other fields.
/// Collapsed or inverted endpoints yield a point at the mapped start, regardless
/// of the annotation's transport collapse policy.
pub fn map_annotation(
    annotation: &Annotation,
    base: &Text,
    target: &Text,
    versioned: &RevisionedMap<'_>,
    revisions: MappingRevisions,
) -> Result<Annotation, AnnotationMappingError> {
    check_revisions(versioned, revisions)?;
    let map = versioned.map;
    validate_lengths(base, target, map)?;
    annotation
        .span
        .slice_in(base)
        .map_err(AnnotationMappingError::InvalidSpan)?;
    let start = map
        .map_offset(
            annotation.span.start().as_usize(),
            annotation.start_affinity,
        )
        .map_err(AnnotationMappingError::PositionMap)?;
    let end = map
        .map_offset(annotation.span.end().as_usize(), annotation.end_affinity)
        .map_err(AnnotationMappingError::PositionMap)?;
    let start = target
        .validate_offset(start)
        .map_err(AnnotationMappingError::InvalidOffset)?;
    let end = if start.as_usize() > end {
        start
    } else {
        target
            .validate_offset(end)
            .map_err(AnnotationMappingError::InvalidOffset)?
    };
    let mut result = annotation.clone();
    result.span = if start > end {
        ByteSpan::empty_at(start)
    } else {
        ByteSpan::new(start, end).expect("ordered endpoints")
    };
    Ok(result)
}

/// Returns a new store with one buffer mapped, leaving other buffers unchanged.
/// The target catalog must contain the buffer and its destination text. No
/// changes are made to the original store if any validation fails.
pub fn map_store(
    store: &AnnotationStore,
    buffer: BufferId,
    base: &Text,
    target_catalog: &BufferCatalog,
    versioned: &RevisionedMap<'_>,
    revisions: MappingRevisions,
) -> Result<AnnotationStore, AnnotationMappingError> {
    check_revisions(versioned, revisions)?;
    let map = versioned.map;
    let target = target_catalog.get(buffer).ok_or_else(|| {
        AnnotationMappingError::Annotation(if target_catalog.is_tombstoned(buffer) {
            AnnotationError::BufferClosed(buffer)
        } else {
            AnnotationError::UnknownBuffer(buffer)
        })
    })?;
    validate_lengths(base, target, map)?;
    let mut result = store.clone();
    for annotation in store.iter().filter(|a| a.key.buffer == buffer) {
        let mapped = map_annotation(annotation, base, target, versioned, revisions)?;
        result
            .set(target_catalog, mapped)
            .map_err(AnnotationMappingError::Annotation)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::ids::ActorId;
    use crate::editor::test_support::{assert_law, Rng};
    use crate::editor::text::anchors::Affinity;
    use crate::editor::text::position_map::ByteReplacement;
    use crate::editor::workspace::annotations::{
        AnnotationId, AnnotationKey, CollapsePolicy, NamespaceId,
    };
    use crate::editor::workspace::properties::{
        project_properties, properties_at, AnnotationProperties,
    };

    fn map_annotation(
        a: &Annotation,
        base: &Text,
        target: &Text,
        map: &PositionMap,
    ) -> Result<Annotation, AnnotationMappingError> {
        let rev = Revision::new(1);
        super::map_annotation(
            a,
            base,
            target,
            &RevisionedMap {
                map,
                source: rev,
                target: rev,
            },
            MappingRevisions {
                base: rev,
                target: rev,
            },
        )
    }

    fn map_store(
        store: &AnnotationStore,
        buffer: BufferId,
        base: &Text,
        target: &BufferCatalog,
        map: &PositionMap,
    ) -> Result<AnnotationStore, AnnotationMappingError> {
        let rev = Revision::new(1);
        super::map_store(
            store,
            buffer,
            base,
            target,
            &RevisionedMap {
                map,
                source: rev,
                target: rev,
            },
            MappingRevisions {
                base: rev,
                target: rev,
            },
        )
    }

    #[test]
    fn composition_rejects_revision_gaps_and_documents_intermediate_utf8() {
        let base = Text::from_str("é");
        let middle = Text::from_str("è");
        let target = Text::empty();
        let first = PositionMap::from_replacement(
            2,
            ByteReplacement {
                start: 1,
                end: 2,
                inserted_len: 1,
            },
        )
        .unwrap();
        let second = PositionMap::from_replacement(
            2,
            ByteReplacement {
                start: 0,
                end: 2,
                inserted_len: 0,
            },
        )
        .unwrap();
        let one = RevisionedMap {
            map: &first,
            source: Revision::new(10),
            target: Revision::new(11),
        };
        let gap = RevisionedMap {
            map: &second,
            source: Revision::new(12),
            target: Revision::new(13),
        };
        assert!(
            matches!(one.compose(&gap), Err(AnnotationMappingError::RevisionMismatch { expected, actual }) if expected == Revision::new(12) && actual == Revision::new(11))
        );
        let two = RevisionedMap {
            map: &second,
            source: Revision::new(11),
            target: Revision::new(13),
        };
        let composed = one.compose(&two).unwrap();
        let annotation = item(&base, 2, 2, Affinity::Left, Affinity::Left, 0);
        assert!(matches!(
            super::map_annotation(
                &annotation,
                &base,
                &middle,
                &one,
                MappingRevisions {
                    base: Revision::new(10),
                    target: Revision::new(11)
                }
            ),
            Err(AnnotationMappingError::InvalidOffset(
                Utf8Error::NotCharBoundary { offset: 1 }
            ))
        ));
        let mapped = super::map_annotation(
            &annotation,
            &base,
            &target,
            &composed.as_revisioned(),
            MappingRevisions {
                base: Revision::new(10),
                target: Revision::new(13),
            },
        )
        .unwrap();
        assert_eq!(mapped.span, ByteSpan::full_of(&target));
    }

    #[test]
    fn independent_affinities_and_revisions() {
        let base = Text::empty();
        let (target, map) = edit("", 0, 0, "xy");
        for (sa, ea, expected) in [
            (Affinity::Left, Affinity::Left, (0, 0)),
            (Affinity::Left, Affinity::Right, (0, 2)),
            (Affinity::Right, Affinity::Left, (2, 2)),
            (Affinity::Right, Affinity::Right, (2, 2)),
        ] {
            let a = item(&base, 0, 0, sa, ea, 0);
            assert_eq!(
                map_annotation(&a, &base, &target, &map).unwrap().span,
                ByteSpan::validate_in(&target, expected.0, expected.1).unwrap()
            );
        }
        let versioned = RevisionedMap {
            map: &map,
            source: Revision::new(1),
            target: Revision::new(2),
        };
        let revisions = MappingRevisions {
            base: Revision::new(0),
            target: Revision::new(2),
        };
        let a = item(&base, 0, 0, Affinity::Left, Affinity::Right, 0);
        assert!(matches!(
            super::map_annotation(&a, &base, &target, &versioned, revisions),
            Err(AnnotationMappingError::RevisionMismatch { .. })
        ));
        assert!(matches!(
            super::map_store(
                &AnnotationStore::new(),
                BufferId::new(1),
                &base,
                &catalog(&target),
                &versioned,
                revisions
            ),
            Err(AnnotationMappingError::RevisionMismatch { .. })
        ));
    }

    fn catalog(text: &Text) -> BufferCatalog {
        let mut catalog = BufferCatalog::new();
        catalog.register(BufferId::new(1), text.clone()).unwrap();
        catalog
    }
    fn item(
        text: &Text,
        start: usize,
        end: usize,
        sa: Affinity,
        ea: Affinity,
        id: u64,
    ) -> Annotation {
        Annotation {
            key: AnnotationKey {
                buffer: BufferId::new(1),
                namespace: NamespaceId::new(2),
                id: AnnotationId::new(id),
            },
            owner: ActorId::new(3),
            span: ByteSpan::validate_in(text, start, end).unwrap(),
            start_affinity: sa,
            end_affinity: ea,
            priority: id as u32,
            collapse: CollapsePolicy::Evict,
            label: format!("item{id}"),
        }
    }
    fn edit(text: &str, start: usize, end: usize, inserted: &str) -> (Text, PositionMap) {
        let target = Text::from_str(&format!("{}{}{}", &text[..start], inserted, &text[end..]));
        let map = PositionMap::from_replacement(
            text.len(),
            ByteReplacement {
                start,
                end,
                inserted_len: inserted.len(),
            },
        )
        .unwrap();
        (target, map)
    }
    #[test]
    fn affinities_collapse_and_preserve_metadata() {
        let base = Text::from_str("abc");
        let (target, deletion) = edit("abc", 0, 3, "");
        for sa in [Affinity::Left, Affinity::Right] {
            for ea in [Affinity::Left, Affinity::Right] {
                let a = item(&base, 1, 2, sa, ea, 7);
                let mapped = map_annotation(&a, &base, &target, &deletion).unwrap();
                let mut expected = a.clone();
                expected.span = ByteSpan::full_of(&target);
                assert_eq!(mapped, expected);
            }
        }
        let (target, replace) = edit("abc", 0, 3, "xy");
        let a = item(&base, 1, 2, Affinity::Right, Affinity::Left, 1);
        assert_eq!(
            map_annotation(&a, &base, &target, &replace).unwrap().span,
            ByteSpan::validate_in(&target, 2, 2).unwrap()
        );
    }
    #[test]
    fn inverted_discarded_endpoint_may_not_be_a_target_boundary() {
        let base = Text::from_str("é");
        let (middle, first) = edit("é", 2, 2, "x");
        let target = Text::from_str("èx");
        let second = PositionMap::from_replacement(
            3,
            ByteReplacement {
                start: 1,
                end: 2,
                inserted_len: 1,
            },
        )
        .unwrap();
        let composed = first.compose(&second).unwrap();
        let annotation = item(&base, 2, 2, Affinity::Right, Affinity::Left, 1);
        let step = map_annotation(&annotation, &base, &middle, &first).unwrap();
        let sequential = map_annotation(&step, &middle, &target, &second).unwrap();
        let direct = map_annotation(&annotation, &base, &target, &composed).unwrap();
        assert_eq!(direct, sequential);
        assert_eq!(direct.span, ByteSpan::validate_in(&target, 3, 3).unwrap());
    }

    #[test]
    fn deleted_annotation_remains_and_resolves_properties() {
        let base = Text::from_str("abc");
        let (target, map) = edit("abc", 0, 3, "");
        let annotation = item(&base, 1, 2, Affinity::Left, Affinity::Right, 7);
        let mut store = AnnotationStore::new();
        store.set(&catalog(&base), annotation.clone()).unwrap();
        let mapped = map_store(&store, BufferId::new(1), &base, &catalog(&target), &map).unwrap();
        let mut expected = annotation.clone();
        expected.span = ByteSpan::full_of(&target);
        assert_eq!(mapped.get(annotation.key), Some(&expected));
        let values = [("color".to_owned(), "red".to_owned())]
            .into_iter()
            .collect();
        let properties = [(annotation.key.id, values)].into_iter().collect();
        assert_eq!(
            properties_at(
                &target,
                &mapped,
                annotation.key.buffer,
                annotation.key.namespace,
                &properties,
                target.validate_offset(0).unwrap()
            )
            .unwrap()
            .get("color")
            .map(String::as_str),
            Some("red")
        );
    }

    #[test]
    fn composition_and_property_projection() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| {
                (0..200)
                    .map(|_| {
                        (
                            rng.gen_range(0..4),
                            rng.gen_range(0..4),
                            rng.gen_range(0..4),
                            rng.gen_range(0..4),
                            rng.gen_range(0..3),
                            rng.gen_range(0..3),
                        )
                    })
                    .collect::<Vec<_>>()
            },
            |cases| {
                let base = Text::from_str("abc");
                for &(a, b, c, d, x, y) in cases {
                    let (middle, first) = edit("abc", a.min(b), a.max(b), &"x".repeat(x));
                    let s = c.min(middle.len_bytes());
                    let e = d.min(middle.len_bytes());
                    let (target, second) =
                        edit(middle.as_str(), s.min(e), s.max(e), &"y".repeat(y));
                    let composed = first.compose(&second).unwrap();
                    let mut store = AnnotationStore::new();
                    let mut props = AnnotationProperties::new();
                    let mut id = 0;
                    for start in 0..=3 {
                        for end in start..=3 {
                            for sa in [Affinity::Left, Affinity::Right] {
                                for ea in [Affinity::Left, Affinity::Right] {
                                    let annotation = item(&base, start, end, sa, ea, id);
                                    store.set(&catalog(&base), annotation).unwrap();
                                    props.insert(
                                        AnnotationId::new(id),
                                        [("color".into(), id.to_string())].into_iter().collect(),
                                    );
                                    id += 1;
                                }
                            }
                        }
                    }
                    let direct = map_store(
                        &store,
                        BufferId::new(1),
                        &base,
                        &catalog(&target),
                        &composed,
                    )
                    .unwrap();
                    let intermediate =
                        map_store(&store, BufferId::new(1), &base, &catalog(&middle), &first)
                            .unwrap();
                    let sequential = map_store(
                        &intermediate,
                        BufferId::new(1),
                        &middle,
                        &catalog(&target),
                        &second,
                    )
                    .unwrap();
                    assert_eq!(direct, sequential);
                    let project = |store: &AnnotationStore| {
                        project_properties(
                            &target,
                            store,
                            BufferId::new(1),
                            NamespaceId::new(2),
                            &props,
                            ByteSpan::full_of(&target),
                        )
                        .unwrap()
                    };
                    assert_eq!(project(&direct), project(&sequential));
                    for point in target.char_boundaries() {
                        assert_eq!(
                            properties_at(
                                &target,
                                &direct,
                                BufferId::new(1),
                                NamespaceId::new(2),
                                &props,
                                point
                            ),
                            properties_at(
                                &target,
                                &sequential,
                                BufferId::new(1),
                                NamespaceId::new(2),
                                &props,
                                point
                            )
                        );
                    }
                }
            },
        );
    }
    #[test]
    fn rejects_bad_lengths_and_foreign_boundaries_atomically() {
        let base = Text::from_str("界");
        let ascii = Text::from_str("abc");
        let identity = PositionMap::identity(3);
        let a = item(&ascii, 1, 2, Affinity::Left, Affinity::Right, 1);
        assert!(matches!(
            map_annotation(&a, &base, &ascii, &identity),
            Err(AnnotationMappingError::InvalidSpan(_))
        ));
        assert!(matches!(
            map_annotation(&a, &ascii, &base, &identity),
            Err(AnnotationMappingError::InvalidOffset(_))
        ));
        assert!(matches!(
            map_annotation(&a, &Text::empty(), &ascii, &identity),
            Err(AnnotationMappingError::SourceLengthMismatch { .. })
        ));
        assert!(matches!(
            map_annotation(&a, &ascii, &Text::empty(), &identity),
            Err(AnnotationMappingError::TargetLengthMismatch { .. })
        ));
        let mut store = AnnotationStore::new();
        store.set(&catalog(&ascii), a).unwrap();
        let original = store.clone();
        assert!(map_store(&store, BufferId::new(1), &base, &catalog(&ascii), &identity).is_err());
        assert_eq!(store, original);
        assert!(map_store(
            &AnnotationStore::new(),
            BufferId::new(1),
            &base,
            &catalog(&ascii),
            &PositionMap::identity(4)
        )
        .is_err());
    }
}
