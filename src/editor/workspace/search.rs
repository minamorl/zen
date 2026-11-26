//! Permission-scoped literal search of immutable snapshots.
//!
//! Both entry points omit unknown, closed, and unreadable buffers without
//! distinguishing them; explicit IDs are deduplicated. Empty patterns take
//! precedence over inactive actors. Results are sorted by ID and byte start.
//! Revision identifies the whole snapshot and can change on unreadable edits.
//! The results support multi-occur, cross-buffer grep, and agent search.

use std::{collections::BTreeSet, fmt};

use crate::editor::{
    collab::{
        access::{AccessError, AccessPolicy, Capability},
        registry::ActorRegistry,
    },
    edit::ids::{ActorId, BufferId},
    text::{
        search::{self, SearchError},
        span::ByteSpan,
    },
    workspace::{snapshots::WorkspaceSnapshot, version::Revision},
};

/// A match in the specified snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceMatch {
    /// Snapshot revision.
    pub revision: Revision,
    /// Buffer containing the match.
    pub buffer: BufferId,
    /// Validated byte range.
    pub span: ByteSpan,
}

/// Search or actor authorization failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceSearchError {
    /// Literal search failed.
    Search(SearchError),
    /// Actor authorization failed.
    Access(AccessError),
}

impl fmt::Display for WorkspaceSearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Search(error) => error.fmt(f),
            Self::Access(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for WorkspaceSearchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Search(error) => Some(error),
            Self::Access(error) => Some(error),
        }
    }
}

/// Searches all live buffers readable by the actor.
pub fn search_workspace(
    snapshot: &WorkspaceSnapshot,
    registry: &ActorRegistry,
    policy: &AccessPolicy,
    actor: ActorId,
    pattern: &str,
) -> Result<Vec<WorkspaceMatch>, WorkspaceSearchError> {
    search_buffers(
        snapshot,
        registry,
        policy,
        actor,
        snapshot.catalog().live_ids(),
        pattern,
    )
}

/// Searches readable live IDs in the supplied set, silently omitting others.
pub fn search_buffers(
    snapshot: &WorkspaceSnapshot,
    registry: &ActorRegistry,
    policy: &AccessPolicy,
    actor: ActorId,
    buffers: impl IntoIterator<Item = BufferId>,
    pattern: &str,
) -> Result<Vec<WorkspaceMatch>, WorkspaceSearchError> {
    if pattern.is_empty() {
        return Err(WorkspaceSearchError::Search(SearchError::EmptyPattern));
    }
    policy
        .authorize(registry, snapshot.catalog(), actor, Capability::Read, [])
        .map_err(WorkspaceSearchError::Access)?;
    let mut matches = Vec::new();
    for buffer in buffers.into_iter().collect::<BTreeSet<_>>() {
        if let Ok(text) = policy.read(registry, snapshot.catalog(), actor, buffer) {
            for span in search::find_all(text, pattern).map_err(WorkspaceSearchError::Search)? {
                matches.push(WorkspaceMatch {
                    revision: snapshot.revision(),
                    buffer,
                    span,
                });
            }
        }
    }
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::role::ActorRole,
        test_support::{assert_law, Rng},
        text::utf8::Text,
        workspace::{catalog::BufferCatalog, snapshots::VersionedWorkspace},
    };

    #[test]
    fn search_laws() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| {
                let mut order: Vec<_> = (0..8).collect();
                rng.shuffle(&mut order);
                let texts = (0..8)
                    .map(|_| {
                        (0..8)
                            .map(|_| *rng.choose(&["a", "界", "😀", "b"]).unwrap())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>();
                (order, texts, rng.next_u64())
            },
            |(order, texts, grants)| {
                let actor = ActorId::new(1);
                let mut registry = ActorRegistry::new();
                registry.register(actor, ActorRole::Agent).unwrap();
                let mut policy = AccessPolicy::new();
                let mut catalog = BufferCatalog::new();
                for &n in order {
                    let id = BufferId::new(n as u64);
                    catalog.register(id, Text::from_str(&texts[n])).unwrap();
                    if grants & (1 << n) != 0 {
                        policy.grant_buffer(actor, id, Capability::Read);
                    }
                }
                let snapshot = VersionedWorkspace::new(catalog.clone()).capture();
                let mut reversed = BufferCatalog::new();
                for &n in order.iter().rev() {
                    reversed
                        .register(BufferId::new(n as u64), Text::from_str(&texts[n]))
                        .unwrap();
                }
                let reversed = VersionedWorkspace::new(reversed).capture();
                for pattern in ["a", "aa", "界", "界界", "😀", "😀😀", "x"] {
                    let actual =
                        search_workspace(&snapshot, &registry, &policy, actor, pattern).unwrap();
                    let mut expected = Vec::new();
                    for id in catalog.live_ids() {
                        if policy.is_granted(actor, id, Capability::Read) {
                            for span in search::find_all(catalog.get(id).unwrap(), pattern).unwrap()
                            {
                                expected.push(WorkspaceMatch {
                                    revision: snapshot.revision(),
                                    buffer: id,
                                    span,
                                });
                            }
                        }
                    }
                    assert_eq!(actual, expected);
                    assert!(actual
                        .windows(2)
                        .all(|pair| (pair[0].buffer, pair[0].span.start())
                            < (pair[1].buffer, pair[1].span.start())));
                    for found in &actual {
                        assert_eq!(
                            found.span.slice_in(catalog.get(found.buffer).unwrap()),
                            Ok(pattern)
                        );
                    }
                    assert_eq!(
                        actual,
                        search_workspace(&reversed, &registry, &policy, actor, pattern).unwrap()
                    );
                    let subset: Vec<_> = catalog
                        .live_ids()
                        .into_iter()
                        .filter(|id| id.raw() % 2 == 0)
                        .collect();
                    let expected_subset: Vec<_> = actual
                        .iter()
                        .copied()
                        .filter(|found| subset.contains(&found.buffer))
                        .collect();
                    let mut permutation = subset.clone();
                    permutation.reverse();
                    permutation.extend(subset);
                    permutation.push(BufferId::new(99));
                    assert_eq!(
                        expected_subset,
                        search_buffers(&snapshot, &registry, &policy, actor, permutation, pattern)
                            .unwrap()
                    );
                    let mut altered = catalog.clone();
                    for id in catalog.live_ids() {
                        if !policy.is_granted(actor, id, Capability::Read) {
                            altered.update(id, Text::from_str("界😀aaaa")).unwrap();
                            altered.remove(id).unwrap();
                        }
                    }
                    altered
                        .register(BufferId::new(99), Text::from_str("aaaa界😀"))
                        .unwrap();
                    let changed = VersionedWorkspace::new(altered).capture();
                    assert_eq!(
                        actual,
                        search_workspace(&changed, &registry, &policy, actor, pattern).unwrap()
                    );
                }
            },
        );
    }

    #[test]
    fn errors_and_overlaps() {
        let actor = ActorId::new(1);
        let mut registry = ActorRegistry::new();
        registry.register(actor, ActorRole::Human).unwrap();
        let mut catalog = BufferCatalog::new();
        let live = BufferId::new(1);
        let closed = BufferId::new(2);
        catalog
            .register(live, Text::from_str("aaa界界界😀😀😀"))
            .unwrap();
        catalog.register(closed, Text::from_str("aaa")).unwrap();
        catalog.remove(closed).unwrap();
        let snapshot = VersionedWorkspace::new(catalog).capture();
        let mut policy = AccessPolicy::new();
        policy.grant_buffer(actor, live, Capability::Read);
        for pattern in ["aa", "界界", "😀😀"] {
            let found = search_buffers(
                &snapshot,
                &registry,
                &policy,
                actor,
                [closed, live, BufferId::new(3), live],
                pattern,
            )
            .unwrap();
            assert_eq!(found.len(), 2);
            assert!(found
                .iter()
                .all(|m| m.span.slice_in(snapshot.catalog().get(live).unwrap()) == Ok(pattern)));
        }
        let inactive = ActorId::new(9);
        assert_eq!(
            search_buffers(&snapshot, &registry, &policy, inactive, [], ""),
            Err(WorkspaceSearchError::Search(SearchError::EmptyPattern))
        );
        assert_eq!(
            search_buffers(&snapshot, &registry, &policy, inactive, [], "a"),
            Err(WorkspaceSearchError::Access(AccessError::InactiveActor(
                inactive
            )))
        );
    }

    #[test]
    fn unreadable_mutations_preserve_locations_and_update_revision() {
        let actor = ActorId::new(1);
        let visible = BufferId::new(1);
        let hidden = BufferId::new(2);
        let added = BufferId::new(3);
        let mut registry = ActorRegistry::new();
        registry.register(actor, ActorRole::Agent).unwrap();
        let mut policy = AccessPolicy::new();
        policy.grant_buffer(actor, visible, Capability::Read);
        let mut catalog = BufferCatalog::new();
        catalog.register(visible, Text::from_str("aaa")).unwrap();
        catalog.register(hidden, Text::from_str("a")).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog);
        let initial = workspace.capture();
        let expected = search_workspace(&initial, &registry, &policy, actor, "aa").unwrap();
        let check = |workspace: &VersionedWorkspace| {
            let snapshot = workspace.capture();
            let found = search_workspace(&snapshot, &registry, &policy, actor, "aa").unwrap();
            assert_eq!(found.len(), expected.len());
            for (actual, original) in found.iter().zip(&expected) {
                assert_eq!(
                    (actual.buffer, actual.span),
                    (original.buffer, original.span)
                );
                assert_eq!(actual.revision, snapshot.revision());
            }
            assert_eq!(
                found,
                search_buffers(
                    &snapshot,
                    &registry,
                    &policy,
                    actor,
                    [visible, hidden, added],
                    "aa"
                )
                .unwrap()
            );
        };
        workspace.update(hidden, Text::from_str("aaaa")).unwrap();
        check(&workspace);
        workspace.register(added, Text::from_str("aaaa")).unwrap();
        check(&workspace);
        workspace.remove(hidden).unwrap();
        check(&workspace);
    }
}
