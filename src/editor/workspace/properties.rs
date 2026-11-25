//! Deterministic property resolution for a buffer and annotation namespace.
//!
//! Property maps are supplied separately, indexed by annotation ID. An absent
//! map contributes nothing. Each key is won independently by the covering
//! annotation with greatest `(priority, id)`. Restricting resolution to one
//! buffer and namespace makes IDs unique even when a store contains others.
//!
//! Query offsets, projection endpoints, and every selected annotation span are
//! revalidated against the supplied target text, even if originally validated
//! elsewhere. Non-empty annotations cover `[start, end)`; empty annotations
//! cover exactly their point, including the end of the text. Projection covers
//! `[start, end)` and an empty projection has no segments. Because only UTF-8
//! character boundaries are valid query offsets, a point annotation's effect
//! in a projection extends to the next character boundary in its representation.
//! Thus each valid offset in a segment has exactly its displayed map. Adjacent
//! segments with equal maps are merged, irrespective of their winning annotations.

use std::collections::BTreeMap;

use crate::editor::edit::ids::BufferId;
use crate::editor::text::span::{ByteSpan, SpanError};
use crate::editor::text::utf8::{ByteOffset, Text};
use crate::editor::workspace::annotations::{
    Annotation, AnnotationId, AnnotationStore, NamespaceId,
};

/// A property map; absent keys have no effective value.
pub type PropertyMap = BTreeMap<String, String>;
/// Properties assigned to annotations in the selected buffer and namespace.
pub type AnnotationProperties = BTreeMap<AnnotationId, PropertyMap>;

/// A non-empty half-open portion of a projection and its effective properties.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PropertySegment {
    /// The portion of the requested span.
    pub span: ByteSpan,
    /// Effective values at every valid offset in this portion.
    pub properties: PropertyMap,
}

fn selected<'a>(
    text: &Text,
    store: &'a AnnotationStore,
    buffer: BufferId,
    namespace: NamespaceId,
) -> Result<Vec<&'a Annotation>, SpanError> {
    let annotations: Vec<_> = store
        .iter()
        .filter(|a| a.key.buffer == buffer && a.key.namespace == namespace)
        .collect();
    for annotation in &annotations {
        annotation.span.slice_in(text)?;
    }
    Ok(annotations)
}

fn resolve(
    annotations: &[&Annotation],
    properties: &AnnotationProperties,
    offset: ByteOffset,
) -> PropertyMap {
    let mut winners = BTreeMap::new();
    for annotation in annotations {
        let span = annotation.span;
        let covers = if span.is_empty() {
            span.start() == offset
        } else {
            span.start() <= offset && offset < span.end()
        };
        if covers {
            if let Some(values) = properties.get(&annotation.key.id) {
                for (key, value) in values {
                    let rank = (annotation.priority, annotation.key.id);
                    let entry = winners.entry(key.clone()).or_insert((rank, value.clone()));
                    if rank > entry.0 {
                        *entry = (rank, value.clone());
                    }
                }
            }
        }
    }
    winners
        .into_iter()
        .map(|(key, (_, value))| (key, value))
        .collect()
}

/// Resolves effective values at a valid character boundary, including text end.
pub fn properties_at(
    text: &Text,
    store: &AnnotationStore,
    buffer: BufferId,
    namespace: NamespaceId,
    properties: &AnnotationProperties,
    offset: ByteOffset,
) -> Result<PropertyMap, SpanError> {
    ByteSpan::empty_at(offset).slice_in(text)?;
    let annotations = selected(text, store, buffer, namespace)?;
    Ok(resolve(&annotations, properties, offset))
}

/// Projects effective values over a validated half-open span in maximal runs.
pub fn project_properties(
    text: &Text,
    store: &AnnotationStore,
    buffer: BufferId,
    namespace: NamespaceId,
    properties: &AnnotationProperties,
    span: ByteSpan,
) -> Result<Vec<PropertySegment>, SpanError> {
    span.slice_in(text)?;
    let annotations = selected(text, store, buffer, namespace)?;
    let boundaries: Vec<_> = text
        .char_boundaries()
        .into_iter()
        .filter(|&b| span.start() <= b && b <= span.end())
        .collect();
    let mut segments: Vec<PropertySegment> = Vec::new();
    for pair in boundaries.windows(2) {
        let values = resolve(&annotations, properties, pair[0]);
        if let Some(last) = segments.last_mut().filter(|last| last.properties == values) {
            last.span = ByteSpan::new(last.span.start(), pair[1]).expect("ordered boundaries");
        } else {
            segments.push(PropertySegment {
                span: ByteSpan::new(pair[0], pair[1]).expect("ordered boundaries"),
                properties: values,
            });
        }
    }
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::ids::ActorId;
    use crate::editor::test_support::{assert_law, texts, Rng};
    use crate::editor::text::anchors::Affinity;
    use crate::editor::workspace::annotations::{AnnotationKey, CollapsePolicy};
    use crate::editor::workspace::catalog::BufferCatalog;

    fn item(id: u64, priority: u32, span: ByteSpan) -> Annotation {
        Annotation {
            key: AnnotationKey {
                buffer: BufferId::new(1),
                namespace: NamespaceId::new(2),
                id: AnnotationId::new(id),
            },
            owner: ActorId::new(0),
            span,
            start_affinity: Affinity::Left,
            end_affinity: Affinity::Right,
            priority,
            collapse: CollapsePolicy::RetainEmpty,
            label: String::new(),
        }
    }
    fn map(values: &[(&str, &str)]) -> PropertyMap {
        values
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }
    fn setup(text: &Text, items: &[Annotation]) -> AnnotationStore {
        let mut catalog = BufferCatalog::new();
        catalog.register(BufferId::new(1), text.clone()).unwrap();
        let mut store = AnnotationStore::new();
        for a in items {
            store.set(&catalog, a.clone()).unwrap();
        }
        store
    }
    fn at(
        text: &Text,
        store: &AnnotationStore,
        props: &AnnotationProperties,
        pos: ByteOffset,
    ) -> PropertyMap {
        properties_at(
            text,
            store,
            BufferId::new(1),
            NamespaceId::new(2),
            props,
            pos,
        )
        .unwrap()
    }
    fn project(
        text: &Text,
        store: &AnnotationStore,
        props: &AnnotationProperties,
        span: ByteSpan,
    ) -> Vec<PropertySegment> {
        project_properties(
            text,
            store,
            BufferId::new(1),
            NamespaceId::new(2),
            props,
            span,
        )
        .unwrap()
    }

    #[test]
    fn precedence_points_and_coalescing() {
        let text = Text::from_str("a界bc");
        let b = text.char_boundaries();
        let items = vec![
            item(0, 1, ByteSpan::new(b[0], b[4]).unwrap()),
            item(1, 2, ByteSpan::new(b[1], b[3]).unwrap()),
            item(2, 2, ByteSpan::empty_at(b[2])),
            item(3, 1, ByteSpan::empty_at(b[4])),
        ];
        let props = [
            (0, map(&[("a", "base"), ("b", "base")])),
            (1, map(&[("a", "high")])),
            (2, map(&[("a", "point")])),
            (3, map(&[("a", "end")])),
        ]
        .into_iter()
        .map(|(id, m)| (AnnotationId::new(id), m))
        .collect();
        let store = setup(&text, &items);
        assert_eq!(
            at(&text, &store, &props, b[1]),
            map(&[("a", "high"), ("b", "base")])
        );
        assert_eq!(
            at(&text, &store, &props, b[2]),
            map(&[("a", "point"), ("b", "base")])
        );
        assert_eq!(at(&text, &store, &props, b[4]), map(&[("a", "end")]));
        let segments = project(&text, &store, &props, ByteSpan::full_of(&text));
        assert_eq!(segments.len(), 4);
        assert!(project(&text, &store, &props, ByteSpan::empty_at(b[2])).is_empty());
        let same = [
            (AnnotationId::new(0), map(&[("x", "same")])),
            (AnnotationId::new(1), map(&[("x", "same")])),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            project(&text, &store, &same, ByteSpan::full_of(&text)).len(),
            1
        );
    }

    #[test]
    fn scopes_isolate_reused_annotation_ids() {
        let text = Text::from_str("abc");
        let boundaries = text.char_boundaries();
        let span = |i| ByteSpan::new(boundaries[i], boundaries[i + 1]).unwrap();
        let target = item(7, 1, span(0));
        let mut other_buffer = item(7, 9, span(1));
        other_buffer.key.buffer = BufferId::new(3);
        let mut other_namespace = item(7, 9, span(2));
        other_namespace.key.namespace = NamespaceId::new(4);
        let mut catalog = BufferCatalog::new();
        for id in [1, 3] {
            catalog.register(BufferId::new(id), text.clone()).unwrap();
        }
        let mut store = AnnotationStore::new();
        for annotation in [target, other_buffer, other_namespace] {
            store.set(&catalog, annotation).unwrap();
        }
        let values = map(&[("key", "value")]);
        let props = [(AnnotationId::new(7), values.clone())]
            .into_iter()
            .collect();
        assert_eq!(at(&text, &store, &props, boundaries[0]), values);
        for &offset in &boundaries[1..3] {
            assert_eq!(at(&text, &store, &props, offset), PropertyMap::new());
        }
        assert_eq!(
            project(&text, &store, &props, ByteSpan::full_of(&text)),
            vec![
                PropertySegment {
                    span: span(0),
                    properties: values
                },
                PropertySegment {
                    span: ByteSpan::new(boundaries[1], boundaries[3]).unwrap(),
                    properties: PropertyMap::new(),
                },
            ]
        );
    }

    #[test]
    fn resolution_is_independent_of_enumeration_order() {
        let text = Text::from_str("a");
        let span = ByteSpan::full_of(&text);
        let low = item(1, 7, span);
        let high = item(2, 7, span);
        let properties = [
            (low.key.id, map(&[("key", "low")])),
            (high.key.id, map(&[("key", "high")])),
        ]
        .into_iter()
        .collect();
        let expected = map(&[("key", "high")]);
        assert_eq!(resolve(&[&low, &high], &properties, span.start()), expected);
        assert_eq!(resolve(&[&high, &low], &properties, span.start()), expected);
    }

    #[test]
    fn foreign_offsets_and_stale_annotations_are_rejected() {
        let text = Text::from_str("界");
        let foreign = Text::from_str("abcd");
        let b = foreign.char_boundaries();
        let empty = AnnotationStore::new();
        for (n, error) in [
            (1, SpanError::NotCharBoundary { offset: 1 }),
            (4, SpanError::OutOfBounds { offset: 4, len: 3 }),
        ] {
            assert_eq!(
                properties_at(
                    &text,
                    &empty,
                    BufferId::new(1),
                    NamespaceId::new(2),
                    &AnnotationProperties::new(),
                    b[n]
                ),
                Err(error.clone())
            );
            assert_eq!(
                project_properties(
                    &text,
                    &empty,
                    BufferId::new(1),
                    NamespaceId::new(2),
                    &AnnotationProperties::new(),
                    ByteSpan::new(b[0], b[n]).unwrap()
                ),
                Err(error.clone())
            );
            let stale = setup(&foreign, &[item(0, 0, ByteSpan::empty_at(b[n]))]);
            assert_eq!(
                properties_at(
                    &text,
                    &stale,
                    BufferId::new(1),
                    NamespaceId::new(2),
                    &AnnotationProperties::new(),
                    text.validate_offset(0).unwrap()
                ),
                Err(error.clone())
            );
            assert_eq!(
                project_properties(
                    &text,
                    &stale,
                    BufferId::new(1),
                    NamespaceId::new(2),
                    &AnnotationProperties::new(),
                    ByteSpan::full_of(&text)
                ),
                Err(error)
            );
        }
    }

    #[test]
    fn generated_projection_matches_independent_point_oracle() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            let mut rng = Rng::new(113);
            for source in corpus {
                let text = Text::from_str(source);
                let b = text.char_boundaries();
                let mut items = Vec::new();
                let mut props = AnnotationProperties::new();
                for id in 0..12 {
                    let x = rng.gen_range(0..b.len());
                    let y = rng.gen_range(0..b.len());
                    items.push(item(
                        id,
                        rng.gen_range(0..3) as u32,
                        ByteSpan::new(b[x.min(y)], b[x.max(y)]).unwrap(),
                    ));
                    props.insert(
                        AnnotationId::new(id),
                        map(&[(
                            if id % 2 == 0 { "a" } else { "b" },
                            if id % 3 == 0 { "x" } else { "y" },
                        )]),
                    );
                }
                let store = setup(&text, &items);
                let mut shuffled = items.clone();
                rng.shuffle(&mut shuffled);
                let other = setup(&text, &shuffled);
                for (i, &start) in b.iter().enumerate() {
                    for &end in &b[i..] {
                        let span = ByteSpan::new(start, end).unwrap();
                        let segments = project(&text, &store, &props, span);
                        assert_eq!(segments, project(&text, &other, &props, span));
                        if start == end {
                            assert!(segments.is_empty());
                            continue;
                        }
                        assert_eq!(segments.first().unwrap().span.start(), start);
                        assert_eq!(segments.last().unwrap().span.end(), end);
                        for pair in segments.windows(2) {
                            assert_eq!(pair[0].span.end(), pair[1].span.start());
                            assert_ne!(pair[0].properties, pair[1].properties);
                        }
                        for &pos in b.iter().filter(|&&p| start <= p && p < end) {
                            let mut expected = PropertyMap::new();
                            for key in ["a", "b"] {
                                if let Some(winner) = items
                                    .iter()
                                    .filter(|a| {
                                        (if a.span.is_empty() {
                                            a.span.start() == pos
                                        } else {
                                            a.span.start() <= pos && pos < a.span.end()
                                        }) && props[&a.key.id].contains_key(key)
                                    })
                                    .max_by_key(|a| (a.priority, a.key.id))
                                {
                                    expected.insert(key.into(), props[&winner.key.id][key].clone());
                                }
                            }
                            assert_eq!(at(&text, &store, &props, pos), expected);
                            let segment = segments
                                .iter()
                                .find(|s| s.span.start() <= pos && pos < s.span.end())
                                .unwrap();
                            assert_eq!(segment.properties, expected);
                        }
                    }
                }
            }
        });
    }
}
