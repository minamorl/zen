//! Frontend annotation coordinates with explicit metadata sidecars.
//! Mapping uses workspace mapping semantics, retaining collapsed ranges as points.
//! After edits, both Emacs and Neovim frontends must re-apply the mapped
//! descriptor projected from zen; native overlay or extmark gravity updates alone
//! do not preserve these semantics. Persist the sidecar as well.
//!
//! For example, replacing 😀 with 界 in `a😀z` with right start affinity and
//! left end affinity (`front_advance=true`, `rear_advance=false`, or
//! `right_gravity=true`, `end_right_gravity=false`) can yield native Emacs
//! endpoints (2, 2) and inverted native Neovim endpoints ((0, 4), (0, 1)).
//! Zen maps them to (3, 3) and ((0, 4), (0, 4)), respectively; unprojecting
//! inverted endpoints is rejected with [`ErrorCode::InvalidSpan`].

use super::agent_position::{self, AgentPosition};
use super::coordinates::{CoordKind, EmacsCharPos, LineColByte};
use super::error::{BoundaryError, ErrorCode};
use super::{emacs_position, neovim_position};
use crate::editor::edit::ids::ActorId;
use crate::editor::text::anchors::Affinity;
use crate::editor::text::span::ByteSpan;
use crate::editor::text::utf8::{ByteOffset, Text};
use crate::editor::workspace::annotation_mapping::{
    self, AnnotationMappingError, MappingRevisions, RevisionedMap,
};
use crate::editor::workspace::annotations::{Annotation, AnnotationKey, CollapsePolicy};

/// Workspace fields that frontend marks do not all carry natively.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnnotationMetadata {
    pub key: AnnotationKey,
    pub owner: ActorId,
    pub priority: u32,
    pub collapse: CollapsePolicy,
    pub label: String,
}
impl From<&Annotation> for AnnotationMetadata {
    fn from(a: &Annotation) -> Self {
        Self {
            key: a.key,
            owner: a.owner,
            priority: a.priority,
            collapse: a.collapse,
            label: a.label.clone(),
        }
    }
}
impl AnnotationMetadata {
    fn annotation(&self, span: ByteSpan, start: Affinity, end: Affinity) -> Annotation {
        Annotation {
            key: self.key,
            owner: self.owner,
            span,
            start_affinity: start,
            end_affinity: end,
            priority: self.priority,
            collapse: self.collapse,
            label: self.label.clone(),
        }
    }
}
/// Overlay coordinates, advance flags and required sidecar.
/// After edits, re-apply the mapped descriptor projected from zen instead of
/// relying on native overlay gravity updates alone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmacsOverlay {
    pub start: EmacsCharPos,
    pub end: EmacsCharPos,
    pub front_advance: bool,
    pub rear_advance: bool,
    pub metadata: AnnotationMetadata,
}
/// Extmark coordinates, gravity flags and required sidecar.
/// After edits, re-apply the mapped descriptor projected from zen instead of
/// relying on native extmark gravity updates alone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NeovimExtmark {
    pub start: LineColByte,
    pub end: LineColByte,
    pub right_gravity: bool,
    pub end_right_gravity: bool,
    pub metadata: AnnotationMetadata,
}
/// Agent comment or diagnostic range and required sidecar.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentAnnotation {
    pub start: AgentPosition,
    pub end: AgentPosition,
    pub start_affinity: Affinity,
    pub end_affinity: Affinity,
    pub metadata: AnnotationMetadata,
}
fn affinity(right: bool) -> Affinity {
    if right {
        Affinity::Right
    } else {
        Affinity::Left
    }
}
fn span(start: ByteOffset, end: ByteOffset) -> Result<ByteSpan, BoundaryError> {
    ByteSpan::new(start, end).map_err(|e| BoundaryError::new(ErrorCode::InvalidSpan, e.to_string()))
}
/// Project an annotation to an overlay.
/// After edits, re-apply the mapped descriptor projected from zen instead of
/// relying on native overlay gravity updates alone.
pub fn project_emacs(text: &Text, a: &Annotation) -> Result<EmacsOverlay, BoundaryError> {
    Ok(EmacsOverlay {
        start: emacs_position::byte_to_emacs(text, a.span.start())?,
        end: emacs_position::byte_to_emacs(text, a.span.end())?,
        front_advance: a.start_affinity == Affinity::Right,
        rear_advance: a.end_affinity == Affinity::Right,
        metadata: a.into(),
    })
}
/// Recover an annotation from an overlay and its sidecar.
pub fn unproject_emacs(text: &Text, a: &EmacsOverlay) -> Result<Annotation, BoundaryError> {
    Ok(a.metadata.annotation(
        span(
            emacs_position::emacs_to_byte(text, a.start)?,
            emacs_position::emacs_to_byte(text, a.end)?,
        )?,
        affinity(a.front_advance),
        affinity(a.rear_advance),
    ))
}
fn validate_extmark(priority: u32) -> Result<(), BoundaryError> {
    if priority > u16::MAX.into() {
        return Err(BoundaryError::new(
            ErrorCode::UnsupportedOperation,
            "extmark priority exceeds 65535",
        ));
    }
    Ok(())
}
/// Project an annotation to an extmark.
/// After edits, re-apply the mapped descriptor projected from zen instead of
/// relying on native extmark gravity updates alone.
pub fn project_neovim(text: &Text, a: &Annotation) -> Result<NeovimExtmark, BoundaryError> {
    validate_extmark(a.priority)?;
    Ok(NeovimExtmark {
        start: neovim_position::byte_to_line_col(text, a.span.start())?,
        end: neovim_position::byte_to_line_col(text, a.span.end())?,
        right_gravity: a.start_affinity == Affinity::Right,
        end_right_gravity: a.end_affinity == Affinity::Right,
        metadata: a.into(),
    })
}
/// Recover an annotation from an extmark and its sidecar.
pub fn unproject_neovim(text: &Text, a: &NeovimExtmark) -> Result<Annotation, BoundaryError> {
    validate_extmark(a.metadata.priority)?;
    Ok(a.metadata.annotation(
        span(
            neovim_position::line_col_to_byte(text, a.start)?,
            neovim_position::line_col_to_byte(text, a.end)?,
        )?,
        affinity(a.right_gravity),
        affinity(a.end_right_gravity),
    ))
}
/// Project an annotation to byte or UTF-16 agent coordinates.
pub fn project_agent(
    text: &Text,
    a: &Annotation,
    kind: CoordKind,
) -> Result<AgentAnnotation, BoundaryError> {
    Ok(AgentAnnotation {
        start: agent_position::byte_to_agent_position(text, a.span.start(), kind)?,
        end: agent_position::byte_to_agent_position(text, a.span.end(), kind)?,
        start_affinity: a.start_affinity,
        end_affinity: a.end_affinity,
        metadata: a.into(),
    })
}
/// Recover an agent annotation, rejecting mixed coordinate kinds.
pub fn unproject_agent(text: &Text, a: &AgentAnnotation) -> Result<Annotation, BoundaryError> {
    if std::mem::discriminant(&a.start) != std::mem::discriminant(&a.end) {
        return Err(BoundaryError::new(
            ErrorCode::InvalidCoordinate,
            "mixed agent coordinate kinds",
        ));
    }
    Ok(a.metadata.annotation(
        agent_position::span_from_positions(text, a.start, a.end)?,
        a.start_affinity,
        a.end_affinity,
    ))
}
/// Reject ties that frontend priority alone cannot order like workspace properties.
pub fn validate_priority_order(annotations: &[Annotation]) -> Result<(), BoundaryError> {
    for (i, a) in annotations.iter().enumerate() {
        for b in &annotations[i + 1..] {
            if a.key.buffer != b.key.buffer || a.key.namespace != b.key.namespace {
                return Err(BoundaryError::new(
                    ErrorCode::InvalidCoordinate,
                    "priority comparison requires one buffer and namespace",
                ));
            }
            if a.key.id == b.key.id || a.priority == b.priority {
                return Err(BoundaryError::new(
                    ErrorCode::UnsupportedOperation,
                    "duplicate id or priority cannot preserve ordering",
                ));
            }
        }
    }
    Ok(())
}
/// Coordinate or revisioned mapping failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectionError {
    Coordinate(BoundaryError),
    Mapping(AnnotationMappingError),
}
impl std::fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Coordinate(e) => e.fmt(f),
            Self::Mapping(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for ProjectionError {}
impl From<BoundaryError> for ProjectionError {
    fn from(e: BoundaryError) -> Self {
        Self::Coordinate(e)
    }
}
impl From<AnnotationMappingError> for ProjectionError {
    fn from(e: AnnotationMappingError) -> Self {
        Self::Mapping(e)
    }
}
/// Map an overlay and reproject in the target text.
pub fn map_emacs(
    a: &EmacsOverlay,
    before: &Text,
    after: &Text,
    map: &RevisionedMap<'_>,
    revisions: MappingRevisions,
) -> Result<EmacsOverlay, ProjectionError> {
    Ok(project_emacs(
        after,
        &annotation_mapping::map_annotation(
            &unproject_emacs(before, a)?,
            before,
            after,
            map,
            revisions,
        )?,
    )?)
}
/// Map an extmark and reproject in the target text.
pub fn map_neovim(
    a: &NeovimExtmark,
    before: &Text,
    after: &Text,
    map: &RevisionedMap<'_>,
    revisions: MappingRevisions,
) -> Result<NeovimExtmark, ProjectionError> {
    Ok(project_neovim(
        after,
        &annotation_mapping::map_annotation(
            &unproject_neovim(before, a)?,
            before,
            after,
            map,
            revisions,
        )?,
    )?)
}
/// Map an agent annotation while retaining its coordinate kind.
pub fn map_agent(
    a: &AgentAnnotation,
    before: &Text,
    after: &Text,
    map: &RevisionedMap<'_>,
    revisions: MappingRevisions,
) -> Result<AgentAnnotation, ProjectionError> {
    let core = unproject_agent(before, a)?;
    let kind = match a.start {
        AgentPosition::Byte(_) => CoordKind::Byte,
        AgentPosition::LineColUtf16(_) => CoordKind::LineColUtf16,
    };
    Ok(project_agent(
        after,
        &annotation_mapping::map_annotation(&core, before, after, map, revisions)?,
        kind,
    )?)
}

#[cfg(test)]
mod tests {
    use super::super::coordinates::{BytePos, LineColUtf16};
    use super::*;
    use crate::editor::edit::ids::BufferId;
    use crate::editor::test_support::{assert_law, texts, Rng};
    use crate::editor::text::position_map::{ByteReplacement, PositionMap};
    use crate::editor::workspace::annotations::{AnnotationId, NamespaceId};
    use crate::editor::workspace::version::Revision;

    fn item(text: &Text, start: usize, end: usize, sa: Affinity, ea: Affinity) -> Annotation {
        Annotation {
            key: AnnotationKey {
                buffer: BufferId::new(1),
                namespace: NamespaceId::new(2),
                id: AnnotationId::new(3),
            },
            owner: ActorId::new(4),
            span: ByteSpan::validate_in(text, start, end).unwrap(),
            start_affinity: sa,
            end_affinity: ea,
            priority: 17,
            collapse: CollapsePolicy::Evict,
            label: "label".into(),
        }
    }

    #[test]
    fn generated_roundtrip_and_edit_commutation() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            let mut rng = Rng::new(73);
            for source in corpus {
                let before = Text::from_str(source);
                let boundaries = before.char_boundaries();
                for _ in 0..12 {
                    let mut p = [
                        rng.gen_range(0..boundaries.len()),
                        rng.gen_range(0..boundaries.len()),
                    ];
                    p.sort();
                    let mut e = [
                        rng.gen_range(0..boundaries.len()),
                        rng.gen_range(0..boundaries.len()),
                    ];
                    e.sort();
                    let (s, t) = (boundaries[e[0]].as_usize(), boundaries[e[1]].as_usize());
                    let inserted = *rng.choose(&["", "界", "😀\n"]).unwrap();
                    let after =
                        Text::from_str(&format!("{}{}{}", &source[..s], inserted, &source[t..]));
                    let geometry = PositionMap::from_replacement(
                        source.len(),
                        ByteReplacement {
                            start: s,
                            end: t,
                            inserted_len: inserted.len(),
                        },
                    )
                    .unwrap();
                    let map = RevisionedMap {
                        map: &geometry,
                        source: Revision::ZERO,
                        target: Revision::new(1),
                    };
                    let revisions = MappingRevisions {
                        base: Revision::ZERO,
                        target: Revision::new(1),
                    };
                    for sa in [Affinity::Left, Affinity::Right] {
                        for ea in [Affinity::Left, Affinity::Right] {
                            let mut a = item(
                                &before,
                                boundaries[p[0]].as_usize(),
                                boundaries[p[1]].as_usize(),
                                sa,
                                ea,
                            );
                            a.priority = rng.gen_range(0..65536) as u32;
                            let mapped = annotation_mapping::map_annotation(
                                &a, &before, &after, &map, revisions,
                            )
                            .unwrap();
                            let emacs = project_emacs(&before, &a).unwrap();
                            assert_eq!(unproject_emacs(&before, &emacs).unwrap(), a);
                            assert_eq!(
                                map_emacs(&emacs, &before, &after, &map, revisions).unwrap(),
                                project_emacs(&after, &mapped).unwrap()
                            );
                            let nvim = project_neovim(&before, &a).unwrap();
                            assert_eq!(unproject_neovim(&before, &nvim).unwrap(), a);
                            assert_eq!(
                                map_neovim(&nvim, &before, &after, &map, revisions).unwrap(),
                                project_neovim(&after, &mapped).unwrap()
                            );
                            let mut higher = a.clone();
                            if a.priority < 65535 {
                                higher.priority += 1;
                                assert!(
                                    emacs.metadata.priority
                                        < project_emacs(&before, &higher)
                                            .unwrap()
                                            .metadata
                                            .priority
                                );
                                assert!(
                                    nvim.metadata.priority
                                        < project_neovim(&before, &higher)
                                            .unwrap()
                                            .metadata
                                            .priority
                                );
                                for kind in [CoordKind::Byte, CoordKind::LineColUtf16] {
                                    assert!(
                                        project_agent(&before, &a, kind).unwrap().metadata.priority
                                            < project_agent(&before, &higher, kind)
                                                .unwrap()
                                                .metadata
                                                .priority
                                    );
                                }
                            }
                            for kind in [CoordKind::Byte, CoordKind::LineColUtf16] {
                                let agent = project_agent(&before, &a, kind).unwrap();
                                assert_eq!(unproject_agent(&before, &agent).unwrap(), a);
                                assert_eq!(
                                    map_agent(&agent, &before, &after, &map, revisions).unwrap(),
                                    project_agent(&after, &mapped, kind).unwrap()
                                );
                            }
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn rejects_invalid_coordinates_and_ambiguous_priority() {
        let text = Text::from_str("😀\nx");
        let a = item(&text, 0, 4, Affinity::Left, Affinity::Right);
        let mut agent = project_agent(&text, &a, CoordKind::Byte).unwrap();
        agent.start = AgentPosition::Byte(BytePos::new(1));
        assert_eq!(
            unproject_agent(&text, &agent).unwrap_err().code(),
            ErrorCode::InvalidBoundary
        );
        agent.start = AgentPosition::Byte(BytePos::new(99));
        assert_eq!(
            unproject_agent(&text, &agent).unwrap_err().code(),
            ErrorCode::OutOfRange
        );
        agent.start = AgentPosition::Byte(BytePos::new(4));
        agent.end = AgentPosition::Byte(BytePos::new(0));
        assert_eq!(
            unproject_agent(&text, &agent).unwrap_err().code(),
            ErrorCode::InvalidSpan
        );
        agent.start = AgentPosition::LineColUtf16(LineColUtf16::new(0, 1));
        agent.end = AgentPosition::LineColUtf16(LineColUtf16::new(0, 2));
        assert_eq!(
            unproject_agent(&text, &agent).unwrap_err().code(),
            ErrorCode::InvalidUtf16Boundary
        );
        let mut overlay = project_emacs(&text, &a).unwrap();
        overlay.start = EmacsCharPos::new(99).unwrap();
        assert_eq!(
            unproject_emacs(&text, &overlay).unwrap_err().code(),
            ErrorCode::OutOfRange
        );
        let mut mark = project_neovim(&text, &a).unwrap();
        mark.start = LineColByte::new(0, 1);
        assert_eq!(
            unproject_neovim(&text, &mark).unwrap_err().code(),
            ErrorCode::InvalidBoundary
        );
        let mut b = a.clone();
        b.priority = 65535;
        assert!(project_neovim(&text, &b).is_ok());
        b.priority = 65536;
        assert_eq!(
            project_neovim(&text, &b).unwrap_err().code(),
            ErrorCode::UnsupportedOperation
        );
        b.priority = a.priority;
        b.key.id = AnnotationId::new(4);
        assert_eq!(
            validate_priority_order(&[a.clone(), b.clone()])
                .unwrap_err()
                .code(),
            ErrorCode::UnsupportedOperation
        );
        b.priority += 1;
        assert!(validate_priority_order(&[a, b]).is_ok());
    }

    #[test]
    fn generated_invalid_neovim_coordinates() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let text = Text::from_str(source);
                let a = item(&text, 0, 0, Affinity::Left, Affinity::Left);
                let mut mark = project_neovim(&text, &a).unwrap();
                let lines: Vec<_> = source.split('\n').collect();
                for (line, content) in lines.iter().enumerate() {
                    mark.start = LineColByte::new(line, content.len() + 1);
                    assert_eq!(
                        unproject_neovim(&text, &mark).unwrap_err().code(),
                        ErrorCode::OutOfRange
                    );
                    for column in 0..content.len() {
                        if !content.is_char_boundary(column) {
                            mark.start = LineColByte::new(line, column);
                            assert_eq!(
                                unproject_neovim(&text, &mark).unwrap_err().code(),
                                ErrorCode::InvalidBoundary
                            );
                        }
                    }
                }
                mark.start = LineColByte::new(lines.len(), 0);
                assert_eq!(
                    unproject_neovim(&text, &mark).unwrap_err().code(),
                    ErrorCode::OutOfRange
                );
            }
        });
    }

    #[test]
    fn replacement_reapplies_mapped_descriptors() {
        let before = Text::from_str("a😀z");
        let after = Text::from_str("a界z");
        let geometry = PositionMap::from_replacement(
            before.len_bytes(),
            ByteReplacement {
                start: 1,
                end: 5,
                inserted_len: "界".len(),
            },
        )
        .unwrap();
        let map = RevisionedMap {
            map: &geometry,
            source: Revision::ZERO,
            target: Revision::new(1),
        };
        let revisions = MappingRevisions {
            base: Revision::ZERO,
            target: Revision::new(1),
        };
        let mut annotation = item(&before, 1, 5, Affinity::Right, Affinity::Left);
        annotation.collapse = CollapsePolicy::RetainEmpty;
        let expected =
            annotation_mapping::map_annotation(&annotation, &before, &after, &map, revisions)
                .unwrap();

        let overlay = project_emacs(&before, &annotation).unwrap();
        let mapped_overlay = map_emacs(&overlay, &before, &after, &map, revisions).unwrap();
        assert_eq!(mapped_overlay.start, EmacsCharPos::new(3).unwrap());
        assert_eq!(mapped_overlay.end, EmacsCharPos::new(3).unwrap());
        assert_eq!(unproject_emacs(&after, &mapped_overlay).unwrap(), expected);

        let mark = project_neovim(&before, &annotation).unwrap();
        let mapped_mark = map_neovim(&mark, &before, &after, &map, revisions).unwrap();
        assert_eq!(mapped_mark.start, LineColByte::new(0, 4));
        assert_eq!(mapped_mark.end, LineColByte::new(0, 4));
        assert_eq!(unproject_neovim(&after, &mapped_mark).unwrap(), expected);

        let mut native_mark = mapped_mark;
        native_mark.end = LineColByte::new(0, 1);
        assert_eq!(
            unproject_neovim(&after, &native_mark).unwrap_err().code(),
            ErrorCode::InvalidSpan
        );
    }

    #[test]
    fn concrete_edits_and_priority_order() {
        let before = Text::from_str("a😀z");
        let after = Text::from_str("az");
        let geometry = PositionMap::from_replacement(
            before.len_bytes(),
            ByteReplacement {
                start: 1,
                end: 5,
                inserted_len: 0,
            },
        )
        .unwrap();
        let map = RevisionedMap {
            map: &geometry,
            source: Revision::ZERO,
            target: Revision::new(1),
        };
        let revisions = MappingRevisions {
            base: Revision::ZERO,
            target: Revision::new(1),
        };
        let mut low = item(&before, 1, 5, Affinity::Left, Affinity::Right);
        low.priority = 0;
        let mut high = low.clone();
        high.priority = 65535;
        high.key.id = AnnotationId::new(4);
        let le = project_emacs(&before, &low).unwrap();
        let he = project_emacs(&before, &high).unwrap();
        let ln = project_neovim(&before, &low).unwrap();
        let hn = project_neovim(&before, &high).unwrap();
        assert_eq!((le.front_advance, le.rear_advance), (false, true));
        assert_eq!((ln.right_gravity, ln.end_right_gravity), (false, true));
        assert!(
            le.metadata.priority < he.metadata.priority
                && ln.metadata.priority < hn.metadata.priority
        );
        assert_eq!(unproject_emacs(&before, &he).unwrap(), high);
        assert_eq!(unproject_neovim(&before, &hn).unwrap(), high);
        let emacs = map_emacs(&he, &before, &after, &map, revisions).unwrap();
        assert_eq!((emacs.start.raw(), emacs.end.raw()), (2, 2));
        let nvim = map_neovim(&hn, &before, &after, &map, revisions).unwrap();
        assert_eq!(
            (nvim.start, nvim.end),
            (LineColByte::new(0, 1), LineColByte::new(0, 1))
        );
        let opposite = item(&before, 1, 5, Affinity::Right, Affinity::Left);
        let oe = project_emacs(&before, &opposite).unwrap();
        let on = project_neovim(&before, &opposite).unwrap();
        assert_eq!(unproject_emacs(&before, &oe).unwrap(), opposite);
        assert_eq!(unproject_neovim(&before, &on).unwrap(), opposite);
        let oe = map_emacs(&oe, &before, &after, &map, revisions).unwrap();
        let on = map_neovim(&on, &before, &after, &map, revisions).unwrap();
        assert_eq!((oe.start.raw(), oe.end.raw()), (2, 2));
        assert_eq!(
            (on.start, on.end),
            (LineColByte::new(0, 1), LineColByte::new(0, 1))
        );
        for kind in [CoordKind::Byte, CoordKind::LineColUtf16] {
            let la = project_agent(&before, &low, kind).unwrap();
            let ha = project_agent(&before, &high, kind).unwrap();
            assert!(la.metadata.priority < ha.metadata.priority);
            assert_eq!(unproject_agent(&before, &ha).unwrap(), high);
            let mapped = map_agent(&ha, &before, &after, &map, revisions).unwrap();
            let expected = match kind {
                CoordKind::Byte => AgentPosition::Byte(BytePos::new(1)),
                _ => AgentPosition::LineColUtf16(LineColUtf16::new(0, 1)),
            };
            assert_eq!((mapped.start, mapped.end), (expected, expected));
        }
        let inserted = Text::from_str("a界😀z");
        let insertion = PositionMap::from_replacement(
            before.len_bytes(),
            ByteReplacement {
                start: 1,
                end: 1,
                inserted_len: 3,
            },
        )
        .unwrap();
        let insertion_map = RevisionedMap {
            map: &insertion,
            source: Revision::ZERO,
            target: Revision::new(1),
        };
        let mut point = item(&before, 1, 1, Affinity::Right, Affinity::Right);
        let e = project_emacs(&before, &point).unwrap();
        let n = project_neovim(&before, &point).unwrap();
        assert_eq!((e.front_advance, e.rear_advance), (true, true));
        assert_eq!((n.right_gravity, n.end_right_gravity), (true, true));
        let e = map_emacs(&e, &before, &inserted, &insertion_map, revisions).unwrap();
        let n = map_neovim(&n, &before, &inserted, &insertion_map, revisions).unwrap();
        assert_eq!((e.start.raw(), e.end.raw()), (3, 3));
        assert_eq!(
            (n.start, n.end),
            (LineColByte::new(0, 4), LineColByte::new(0, 4))
        );
        point.start_affinity = Affinity::Left;
        point.end_affinity = Affinity::Left;
        let e = project_emacs(&before, &point).unwrap();
        let n = project_neovim(&before, &point).unwrap();
        assert_eq!((e.front_advance, e.rear_advance), (false, false));
        assert_eq!((n.right_gravity, n.end_right_gravity), (false, false));
        let e = map_emacs(&e, &before, &inserted, &insertion_map, revisions).unwrap();
        let n = map_neovim(&n, &before, &inserted, &insertion_map, revisions).unwrap();
        assert_eq!((e.start.raw(), e.end.raw()), (2, 2));
        assert_eq!(
            (n.start, n.end),
            (LineColByte::new(0, 1), LineColByte::new(0, 1))
        );
    }
}
