//! Position-attached annotations keyed by buffer, namespace, and identifier.
//!
//! Setting the same value twice is idempotent: map insertion replaces a value
//! with itself. Operations on distinct keys commute because each changes only
//! its own entry; the full tuple prevents collisions across buffers or
//! namespaces. Validation depends only on the unchanged catalog and value.
//!
//! Transport applies each stored endpoint affinity independently. A range is
//! kept as returned; a collapsed or inverted result is evicted under `Evict`.
//! Under `RetainEmpty`, a collapse stays at its common boundary and an
//! inversion becomes empty at the transported start boundary. In particular,
//! inverted endpoints are never sorted into a range containing inserted text.
//! Transport is atomic: an invalid replacement or endpoint leaves the store
//! unchanged, including when the selected buffer has no annotations.

use std::collections::BTreeMap;
use std::fmt;

use crate::editor::edit::ids::{ActorId, BufferId};
use crate::editor::text::anchors::{
    Affinity, Anchor, AnchorError, ReplacementEffect, TransportedSpan,
};
use crate::editor::text::span::{ByteSpan, SpanError};
use crate::editor::text::utf8::Text;
use crate::editor::workspace::catalog::BufferCatalog;

/// Identifies an annotation namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct NamespaceId(u64);
impl NamespaceId {
    /// Creates an identifier.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    /// Returns its numeric value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Identifies an annotation within a buffer and namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct AnnotationId(u64);
impl AnnotationId {
    /// Creates an identifier.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    /// Returns its numeric value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// The complete identity of an annotation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct AnnotationKey {
    /// Buffer containing the annotation.
    pub buffer: BufferId,
    /// Namespace containing the annotation.
    pub namespace: NamespaceId,
    /// Identifier within the buffer and namespace.
    pub id: AnnotationId,
}

/// Behavior when transported endpoints collapse or invert.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollapsePolicy {
    /// Remove the annotation.
    Evict,
    /// Keep an empty span at the transported start boundary.
    RetainEmpty,
}

/// A labeled, independently affined range owned by an actor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Annotation {
    /// Complete identity.
    pub key: AnnotationKey,
    /// Owning actor.
    pub owner: ActorId,
    /// Half-open range in the buffer text.
    pub span: ByteSpan,
    /// Affinity of the start endpoint.
    pub start_affinity: Affinity,
    /// Affinity of the end endpoint.
    pub end_affinity: Affinity,
    /// Rendering priority.
    pub priority: u32,
    /// Behavior after collapse or inversion.
    pub collapse: CollapsePolicy,
    /// Caller-defined label.
    pub label: String,
}

/// Annotation validation or transport failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnnotationError {
    /// The buffer identifier has never been registered.
    UnknownBuffer(BufferId),
    /// The buffer was removed from the catalog.
    BufferClosed(BufferId),
    /// The span does not address the live text.
    InvalidSpan(SpanError),
    /// The replacement or an endpoint cannot be transported.
    Transport(AnchorError),
}
impl fmt::Display for AnnotationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownBuffer(id) => write!(f, "unknown buffer {id}"),
            Self::BufferClosed(id) => write!(f, "buffer {id} is closed"),
            Self::InvalidSpan(error) => write!(f, "invalid annotation span: {error}"),
            Self::Transport(error) => write!(f, "annotation transport failed: {error}"),
        }
    }
}
impl std::error::Error for AnnotationError {}

/// A deterministic collection of annotations.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AnnotationStore {
    entries: BTreeMap<AnnotationKey, Annotation>,
}
impl AnnotationStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a value after validating its buffer and span.
    ///
    /// Tombstoned buffers are distinguished from unknown identifiers. Both
    /// endpoints are revalidated against the current live text before the map
    /// is changed, so validation errors leave existing entries untouched.
    pub fn set(
        &mut self,
        catalog: &BufferCatalog,
        annotation: Annotation,
    ) -> Result<(), AnnotationError> {
        let id = annotation.key.buffer;
        let text = catalog.get(id).ok_or_else(|| {
            if catalog.is_tombstoned(id) {
                AnnotationError::BufferClosed(id)
            } else {
                AnnotationError::UnknownBuffer(id)
            }
        })?;
        annotation
            .span
            .slice_in(text)
            .map_err(AnnotationError::InvalidSpan)?;
        self.entries.insert(annotation.key, annotation);
        Ok(())
    }

    /// Removes an entry if present.
    pub fn remove(&mut self, key: AnnotationKey) -> Option<Annotation> {
        self.entries.remove(&key)
    }
    /// Looks up an entry by its complete key.
    pub fn get(&self, key: AnnotationKey) -> Option<&Annotation> {
        self.entries.get(&key)
    }
    /// Iterates in key order.
    pub fn iter(&self) -> impl Iterator<Item = &Annotation> {
        self.entries.values()
    }
    /// Returns the number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    /// Returns whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Transports one buffer through a replacement, committing only on success.
    ///
    /// `before` and `after` must describe the supplied effect. The caller is
    /// responsible for coordinating the catalog text update with this operation.
    pub fn transport(
        &mut self,
        buffer: BufferId,
        effect: ReplacementEffect,
        before: &Text,
        after: &Text,
    ) -> Result<(), AnnotationError> {
        let origin = before.validate_offset(0).expect("zero is a text boundary");
        effect
            .transport(
                Anchor {
                    offset: origin,
                    affinity: Affinity::Left,
                },
                before,
                after,
            )
            .map_err(AnnotationError::Transport)?;
        let mut updated = self.entries.clone();
        for (key, annotation) in self.entries.iter().filter(|(key, _)| key.buffer == buffer) {
            let start = Anchor {
                offset: annotation.span.start(),
                affinity: annotation.start_affinity,
            };
            let end = Anchor {
                offset: annotation.span.end(),
                affinity: annotation.end_affinity,
            };
            let result = effect
                .transport_endpoints(start, end, before, after)
                .map_err(AnnotationError::Transport)?;
            let span = match result {
                TransportedSpan::Range { start, end } => Some(
                    ByteSpan::new(start.offset, end.offset)
                        .expect("transport returned ordered endpoints"),
                ),
                TransportedSpan::Collapsed { start, .. }
                | TransportedSpan::Inverted { start, .. } => match annotation.collapse {
                    CollapsePolicy::Evict => None,
                    CollapsePolicy::RetainEmpty => Some(ByteSpan::empty_at(start.offset)),
                },
            };
            if let Some(span) = span {
                updated.get_mut(key).expect("cloned entry exists").span = span;
            } else {
                updated.remove(key);
            }
        }
        self.entries = updated;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(buffer: u64, namespace: u64, id: u64) -> AnnotationKey {
        AnnotationKey {
            buffer: BufferId::new(buffer),
            namespace: NamespaceId::new(namespace),
            id: AnnotationId::new(id),
        }
    }
    fn annotation(
        key: AnnotationKey,
        span: ByteSpan,
        start_affinity: Affinity,
        end_affinity: Affinity,
        collapse: CollapsePolicy,
    ) -> Annotation {
        Annotation {
            key,
            owner: ActorId::new(7),
            span,
            start_affinity,
            end_affinity,
            priority: 9,
            collapse,
            label: "mark".into(),
        }
    }

    #[test]
    fn set_idempotence_and_distinct_operations_commute() {
        let text = Text::from_str("a界");
        let mut catalog = BufferCatalog::new();
        for b in 0..2 {
            catalog.register(BufferId::new(b), text.clone()).unwrap();
        }
        let keys: Vec<_> = (0..2)
            .flat_map(|b| (0..2).flat_map(move |n| (0..2).map(move |i| key(b, n, i))))
            .collect();
        for &a in &keys {
            for &b in &keys {
                if a == b {
                    continue;
                }
                let x = annotation(
                    a,
                    ByteSpan::full_of(&text),
                    Affinity::Left,
                    Affinity::Right,
                    CollapsePolicy::RetainEmpty,
                );
                let y = annotation(
                    b,
                    ByteSpan::empty_at(text.validate_offset(1).unwrap()),
                    Affinity::Right,
                    Affinity::Left,
                    CollapsePolicy::Evict,
                );
                for initial in 0..4 {
                    for operation_a in 0..2 {
                        for operation_b in 0..2 {
                            let mut store = AnnotationStore::new();
                            if initial & 1 != 0 {
                                store.set(&catalog, x.clone()).unwrap();
                            }
                            if initial & 2 != 0 {
                                store.set(&catalog, y.clone()).unwrap();
                            }
                            let apply =
                                |store: &mut AnnotationStore, value: &Annotation, operation| {
                                    if operation == 0 {
                                        store.set(&catalog, value.clone()).unwrap();
                                    } else {
                                        store.remove(value.key);
                                    }
                                };
                            let mut reverse = store.clone();
                            apply(&mut store, &x, operation_a);
                            apply(&mut store, &y, operation_b);
                            apply(&mut reverse, &y, operation_b);
                            apply(&mut reverse, &x, operation_a);
                            assert_eq!(store, reverse);
                            assert_eq!(
                                store.get(a),
                                if operation_a == 0 { Some(&x) } else { None }
                            );
                            assert_eq!(
                                store.get(b),
                                if operation_b == 0 { Some(&y) } else { None }
                            );
                            store.set(&catalog, x.clone()).unwrap();
                            let once = store.clone();
                            store.set(&catalog, x.clone()).unwrap();
                            assert_eq!(store, once);
                            assert_eq!(store.get(a), Some(&x));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn catalog_and_span_validation_is_atomic() {
        let mut catalog = BufferCatalog::new();
        let text = Text::from_str("界");
        catalog.register(BufferId::new(0), text.clone()).unwrap();
        catalog.register(BufferId::new(1), text.clone()).unwrap();
        catalog.remove(BufferId::new(1)).unwrap();
        let foreign = Text::from_str("abcd");
        let mut item = annotation(
            key(0, 0, 0),
            ByteSpan::full_of(&text),
            Affinity::Left,
            Affinity::Right,
            CollapsePolicy::Evict,
        );
        let mut store = AnnotationStore::new();
        store.set(&catalog, item.clone()).unwrap();
        let before = store.clone();
        for (buffer, expected) in [
            (1, AnnotationError::BufferClosed(BufferId::new(1))),
            (2, AnnotationError::UnknownBuffer(BufferId::new(2))),
        ] {
            item.key.buffer = BufferId::new(buffer);
            assert_eq!(store.set(&catalog, item.clone()), Err(expected));
            assert_eq!(store, before);
        }
        item.key.buffer = BufferId::new(0);
        for n in [1, 4] {
            item.span = ByteSpan::empty_at(foreign.validate_offset(n).unwrap());
            assert!(matches!(
                store.set(&catalog, item.clone()),
                Err(AnnotationError::InvalidSpan(_))
            ));
            assert_eq!(store, before);
        }
        item.span = ByteSpan::validate_in(&foreign, 0, 1).unwrap();
        assert_eq!(
            store.set(&catalog, item),
            Err(AnnotationError::InvalidSpan(SpanError::NotCharBoundary {
                offset: 1
            }))
        );
        assert_eq!(store, before);
    }

    #[test]
    fn generated_catalog_and_endpoint_validation() {
        for source in ["", "a", "界", "a界"] {
            let text = Text::from_str(source);
            let foreign = Text::from_str(&"x".repeat(text.len_bytes() + 1));
            let mut catalog = BufferCatalog::new();
            catalog.register(BufferId::new(0), text.clone()).unwrap();
            catalog.register(BufferId::new(1), text.clone()).unwrap();
            catalog.remove(BufferId::new(1)).unwrap();
            let seed = annotation(
                key(0, 0, 0),
                ByteSpan::full_of(&text),
                Affinity::Left,
                Affinity::Right,
                CollapsePolicy::Evict,
            );
            let mut initial = AnnotationStore::new();
            initial.set(&catalog, seed).unwrap();
            for buffer in 0..3 {
                for start in 0..=foreign.len_bytes() {
                    for end in start..=foreign.len_bytes() {
                        let span = ByteSpan::validate_in(&foreign, start, end).unwrap();
                        let item = annotation(
                            key(buffer, 0, 0),
                            span,
                            Affinity::Right,
                            Affinity::Left,
                            CollapsePolicy::RetainEmpty,
                        );
                        let invalid = [start, end].into_iter().find_map(|offset| {
                            if offset > source.len() {
                                Some(SpanError::OutOfBounds {
                                    offset,
                                    len: source.len(),
                                })
                            } else if !source.is_char_boundary(offset) {
                                Some(SpanError::NotCharBoundary { offset })
                            } else {
                                None
                            }
                        });
                        let expected = match buffer {
                            1 => Err(AnnotationError::BufferClosed(BufferId::new(buffer))),
                            2 => Err(AnnotationError::UnknownBuffer(BufferId::new(buffer))),
                            _ => invalid
                                .map_or(Ok(()), |error| Err(AnnotationError::InvalidSpan(error))),
                        };
                        let mut store = initial.clone();
                        assert_eq!(store.set(&catalog, item.clone()), expected);
                        if expected.is_err() {
                            assert_eq!(store, initial);
                        } else {
                            assert_eq!(store.get(item.key), Some(&item));
                            assert_eq!(store.len(), 1);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn exhaustive_transport_and_lifecycle() {
        for source in ["", "ab", "a界"] {
            let before = Text::from_str(source);
            let bounds = before.char_boundaries();
            for (si, &s) in bounds.iter().enumerate() {
                for &e in &bounds[si..] {
                    for inserted in ["", "x", "界"] {
                        let after = Text::from_str(&format!(
                            "{}{}{}",
                            &source[..s.as_usize()],
                            inserted,
                            &source[e.as_usize()..]
                        ));
                        let effect = ReplacementEffect {
                            span: ByteSpan::new(s, e).unwrap(),
                            inserted_len: inserted.len(),
                        };
                        for (i, &start) in bounds.iter().enumerate() {
                            for &end in &bounds[i..] {
                                for sa in [Affinity::Left, Affinity::Right] {
                                    for ea in [Affinity::Left, Affinity::Right] {
                                        for policy in
                                            [CollapsePolicy::Evict, CollapsePolicy::RetainEmpty]
                                        {
                                            let mut catalog = BufferCatalog::new();
                                            catalog
                                                .register(BufferId::new(0), before.clone())
                                                .unwrap();
                                            let item = annotation(
                                                key(0, 0, 0),
                                                ByteSpan::new(start, end).unwrap(),
                                                sa,
                                                ea,
                                                policy,
                                            );
                                            let mut store = AnnotationStore::new();
                                            store.set(&catalog, item.clone()).unwrap();
                                            let result = effect
                                                .transport_endpoints(
                                                    Anchor {
                                                        offset: start,
                                                        affinity: sa,
                                                    },
                                                    Anchor {
                                                        offset: end,
                                                        affinity: ea,
                                                    },
                                                    &before,
                                                    &after,
                                                )
                                                .unwrap();
                                            store
                                                .transport(
                                                    BufferId::new(0),
                                                    effect,
                                                    &before,
                                                    &after,
                                                )
                                                .unwrap();
                                            let expected = match result {
                                                TransportedSpan::Range { start, end } => Some(
                                                    ByteSpan::new(start.offset, end.offset)
                                                        .unwrap(),
                                                ),
                                                TransportedSpan::Collapsed { start, .. }
                                                | TransportedSpan::Inverted { start, .. } => {
                                                    if policy == CollapsePolicy::Evict {
                                                        None
                                                    } else {
                                                        Some(ByteSpan::empty_at(start.offset))
                                                    }
                                                }
                                            };
                                            assert_eq!(
                                                store.get(item.key).map(|a| a.span),
                                                expected
                                            );
                                            if let Some(actual) = store.get(item.key) {
                                                let mut expected_item = item.clone();
                                                expected_item.span = expected.unwrap();
                                                assert_eq!(actual, &expected_item);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn inversion_and_failed_transport_do_not_expand_or_mutate() {
        let before = Text::from_str("abc");
        let after = Text::from_str("aXc");
        let effect = ReplacementEffect {
            span: ByteSpan::validate_in(&before, 1, 2).unwrap(),
            inserted_len: 1,
        };
        let mut catalog = BufferCatalog::new();
        catalog.register(BufferId::new(0), before.clone()).unwrap();
        let mut store = AnnotationStore::new();
        let item = annotation(
            key(0, 0, 0),
            effect.span,
            Affinity::Right,
            Affinity::Left,
            CollapsePolicy::RetainEmpty,
        );
        store.set(&catalog, item.clone()).unwrap();
        let original = store.clone();
        assert!(store
            .transport(BufferId::new(0), effect, &before, &Text::from_str("zXc"))
            .is_err());
        assert_eq!(store, original);
        store
            .transport(BufferId::new(0), effect, &before, &after)
            .unwrap();
        assert_eq!(
            store.get(item.key).unwrap().span,
            ByteSpan::empty_at(after.validate_offset(2).unwrap())
        );
        let mut empty = AnnotationStore::new();
        assert!(empty
            .transport(BufferId::new(0), effect, &before, &Text::from_str("zXc"))
            .is_err());

        let longer = Text::from_str("abcd");
        catalog.update(BufferId::new(0), longer.clone()).unwrap();
        let mut stale = AnnotationStore::new();
        let first = annotation(
            key(0, 0, 0),
            ByteSpan::empty_at(longer.validate_offset(1).unwrap()),
            Affinity::Right,
            Affinity::Right,
            CollapsePolicy::RetainEmpty,
        );
        let second = annotation(
            key(0, 0, 1),
            ByteSpan::empty_at(longer.validate_offset(4).unwrap()),
            Affinity::Left,
            Affinity::Left,
            CollapsePolicy::RetainEmpty,
        );
        stale.set(&catalog, first).unwrap();
        stale.set(&catalog, second).unwrap();
        let original = stale.clone();
        assert_eq!(
            stale.transport(BufferId::new(0), effect, &before, &after),
            Err(AnnotationError::Transport(AnchorError::InvalidBounds))
        );
        assert_eq!(stale, original);
    }
}
