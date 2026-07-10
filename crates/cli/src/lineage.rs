//! Fork + subagent lineage tree: pure construction and `git log --graph`-style
//! layout, decoupled from `App` and ratatui so the same logic can back the
//! `C-x q` / `q` popup today and a future pinnable/dockable panel later
//! without a rewrite (see specs/0079-fork-and-subagent-lineage-view.md).
//!
//! A session has at most one incoming lineage edge — either it was forked
//! from a parent (`forked_from`, spec 0078) or it is a subagent parented to
//! one (`parent_session_id`, spec 0014); a session is never both. That means
//! the full lineage graph is a strict tree, never a general DAG, which is
//! what makes a plain recursive walk (no cycle-breaking beyond a defensive
//! guard) sufficient.

use std::collections::{HashMap, HashSet};

use agentd_protocol::{ForkMergeMode, SessionKind, SessionState, SessionSummary};

/// Levels rendered below the tree's root before a subtree collapses into a
/// "+N more" marker (spec: "depth/breadth cap").
pub const MAX_DEPTH: usize = 6;
/// Children rendered per node before the rest collapse into a "+N more"
/// marker.
pub const MAX_SIBLINGS: usize = 12;

/// What kind of edge connects a node to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineageEdge {
    /// The tree's root — no incoming edge.
    Root,
    /// Mergeable sibling via `forked_from` (spec 0078).
    Fork,
    /// True parent/child helper via `parent_session_id` (spec 0014).
    Subagent,
}

/// Fork-specific terminal state, derived from [`SessionSummary::merge`].
/// Meaningless for `LineageEdge::Subagent` / `LineageEdge::Root` nodes —
/// those are always `Open`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkStatus {
    /// Not a fork, or a fork with no merge outcome recorded yet — still
    /// mergeable.
    Open,
    /// `ForkMergeMode::Result`: closed back into the parent at the point the
    /// result was injected into its transcript.
    Merged,
    /// `ForkMergeMode::Discard`: dead-ended without a result.
    Discarded,
}

impl ForkStatus {
    pub fn of(summary: &SessionSummary) -> ForkStatus {
        match summary.merge.as_ref().map(|m| m.mode) {
            Some(ForkMergeMode::Result) => ForkStatus::Merged,
            Some(ForkMergeMode::Discard) => ForkStatus::Discarded,
            None => ForkStatus::Open,
        }
    }
}

/// One child slot in a node's children list: a real node, or a collapsed
/// run of nodes the depth/breadth cap dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineageChild {
    Node(LineageNode),
    /// `count` additional nodes exist here but were not materialized —
    /// either extra siblings beyond [`MAX_SIBLINGS`], or (when this marker
    /// is a node's only child) its direct children, dropped because
    /// [`MAX_DEPTH`] was reached.
    More(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineageNode {
    pub session_id: String,
    pub edge: LineageEdge,
    pub children: Vec<LineageChild>,
}

/// Whether `session_id` has any lineage relationship worth showing: it was
/// itself forked from a parent, or at least one other session in `sessions`
/// points back at it via `forked_from`/`parent_session_id`. Used to gate the
/// lineage preview trigger (the pane title bar's harness label) on ordinary
/// sessions that have nothing to show — cheaper than [`build_tree`] since it
/// doesn't walk to the root or materialize the full tree, just answers
/// yes/no for `session_id` itself.
pub fn has_lineage(session_id: &str, sessions: &[SessionSummary]) -> bool {
    sessions.iter().any(|s| {
        if s.id == session_id {
            s.forked_from.is_some()
        } else {
            (matches!(s.kind, SessionKind::Subagent)
                && s.parent_session_id.as_deref() == Some(session_id))
                || s.forked_from
                    .as_ref()
                    .is_some_and(|f| f.session_id == session_id)
        }
    })
}

/// Build the lineage tree containing `focus_id`: walk up through fork
/// (`forked_from`) and subagent (`parent_session_id`) parent links to the
/// topmost ancestor, then materialize the tree back down from there. `None`
/// when `focus_id` isn't among `sessions` (e.g. it was deleted while the
/// popup was open).
pub fn build_tree(focus_id: &str, sessions: &[SessionSummary]) -> Option<LineageNode> {
    let by_id: HashMap<&str, &SessionSummary> =
        sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    by_id.get(focus_id)?;
    let root_id = root_of(focus_id, &by_id);
    let mut visited = HashSet::new();
    build_subtree(&root_id, &by_id, LineageEdge::Root, 0, &mut visited)
}

fn parent_of(s: &SessionSummary) -> Option<&str> {
    s.forked_from
        .as_ref()
        .map(|f| f.session_id.as_str())
        .or(s.parent_session_id.as_deref())
}

fn root_of(focus_id: &str, by_id: &HashMap<&str, &SessionSummary>) -> String {
    let mut current = focus_id.to_string();
    let mut seen = HashSet::new();
    while seen.insert(current.clone()) {
        let Some(s) = by_id.get(current.as_str()) else {
            break;
        };
        match parent_of(s).filter(|p| by_id.contains_key(p)) {
            Some(p) => current = p.to_string(),
            None => break,
        }
    }
    current
}

fn build_subtree(
    id: &str,
    by_id: &HashMap<&str, &SessionSummary>,
    edge: LineageEdge,
    depth: usize,
    visited: &mut HashSet<String>,
) -> Option<LineageNode> {
    // Defensive cycle guard: a well-formed lineage graph is a tree (every
    // session has at most one parent edge), so this should never trip. It
    // exists so corrupted/adversarial data can't hang the render loop.
    if !visited.insert(id.to_string()) {
        return None;
    }
    by_id.get(id)?;

    let mut kids: Vec<(&SessionSummary, LineageEdge)> = Vec::new();
    for s in by_id.values() {
        if matches!(s.kind, SessionKind::Subagent) && s.parent_session_id.as_deref() == Some(id) {
            kids.push((s, LineageEdge::Subagent));
        } else if s.forked_from.as_ref().is_some_and(|f| f.session_id == id) {
            kids.push((s, LineageEdge::Fork));
        }
    }
    // Deterministic order: position/creation order within each edge type,
    // then subagents before forks (stable sort preserves the first pass).
    // The `by_id.values()` collection above iterates a `HashMap` in
    // unspecified order, so a final tiebreak on `id` is required — without
    // it, two sessions with equal `position` *and* `created_at` (both
    // plausible: default `position` is 0, and batch-created sessions can
    // share a millisecond) would render in a different order every time.
    kids.sort_by(|(a, _), (b, _)| {
        a.position
            .cmp(&b.position)
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    kids.sort_by_key(|(_, e)| matches!(e, LineageEdge::Fork));

    let total = kids.len();
    let children = if total == 0 {
        Vec::new()
    } else if depth + 1 >= MAX_DEPTH {
        // One more level would exceed the depth cap — collapse this node's
        // children (and everything below them) into a single marker rather
        // than silently truncating one branch and not another.
        vec![LineageChild::More(total)]
    } else {
        let mut out: Vec<LineageChild> = kids
            .iter()
            .take(MAX_SIBLINGS)
            .filter_map(|(s, e)| {
                build_subtree(&s.id, by_id, *e, depth + 1, visited).map(LineageChild::Node)
            })
            .collect();
        if total > MAX_SIBLINGS {
            out.push(LineageChild::More(total - MAX_SIBLINGS));
        }
        out
    };

    Some(LineageNode {
        session_id: id.to_string(),
        edge,
        children,
    })
}

/// What a flattened row represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineageRowKind {
    Node {
        session_id: String,
        edge: LineageEdge,
    },
    /// Activity on a node's own timeline within ONE window — bounded by its
    /// own creation, each fork child's fork-out/merge-back points on this
    /// node's timeline, and "now" (or this node's own terminal point, if it
    /// has one). A rail annotation, not attached to any one node's own
    /// line — see the "Design notes" doc comment above `push_segment` for
    /// how these windows are computed. Not selectable — skipped by
    /// `selectable_indices`, same as `More`.
    Segment {
        /// Messages/turns within this window (`SessionSummary::event_count`
        /// / `ForkedFrom::transcript_seq` / `ForkMerge::merged_seq` units —
        /// all the same transcript sequence counter).
        delta_events: u64,
        /// Start of this window, epoch ms.
        start_ms: i64,
        /// End of this window, epoch ms. `None` means this window is still
        /// open — its end is "now" at render time rather than baked in
        /// here, exactly like `render_lineage_row` used to take `now_ms`
        /// at render time rather than storing elapsed text on the row.
        end_ms: Option<i64>,
    },
    /// A "+N more" collapse marker — not selectable.
    More(usize),
}

/// One flattened, renderable line of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineageRow {
    pub depth: usize,
    /// Whether this row is the last among its siblings — selects the
    /// `└─` vs `├─` connector and whether the rail continues below it.
    pub is_last: bool,
    /// Per-ancestor-level: draw a continuing `│` rail (`true`) or blank
    /// space (`false`) in that column. Length is `depth` (root excluded).
    pub rails: Vec<bool>,
    pub kind: LineageRowKind,
}

impl LineageRow {
    pub fn session_id(&self) -> Option<&str> {
        match &self.kind {
            LineageRowKind::Node { session_id, .. } => Some(session_id),
            LineageRowKind::Segment { .. } | LineageRowKind::More(_) => None,
        }
    }

    pub fn is_selectable(&self) -> bool {
        matches!(self.kind, LineageRowKind::Node { .. })
    }

    /// Compact `git log --graph`-style rail prefix, e.g. `"│ ├─"` — glyphs
    /// only, no session label (callers append the edge glyph/status/stats,
    /// or the segment text, after this).
    ///
    /// A `Segment` row always gets a plain continuing `│` rather than a
    /// `├─`/`└─` branch connector — it isn't a branch, it's an annotation
    /// sitting on the rail between the markers it describes, so it never
    /// changes the tree's shape the way a real child does.
    pub fn rail_prefix(&self) -> String {
        let mut out = String::new();
        for r in &self.rails {
            out.push_str(if *r { "\u{2502} " } else { "  " });
        }
        if self.depth > 0 {
            match &self.kind {
                LineageRowKind::Segment { .. } => out.push_str("\u{2502} "),
                _ => out.push_str(if self.is_last {
                    "\u{2514}\u{2500}"
                } else {
                    "\u{251c}\u{2500}"
                }),
            }
        }
        out
    }
}

/// Indices of the selectable (non-`More`) rows within a flattened row list,
/// in on-screen order — the shared "which rows can the cursor land on"
/// logic behind keyboard navigation. Kept here, next to `flatten`, so both
/// the lineage preview's rendering (`ui.rs::render_lineage_preview`, to
/// highlight the selected row) and its keyboard navigation
/// (`app/lineage_preview.rs`, to move/clamp the selection) share one
/// definition rather than re-deriving it.
pub fn selectable_indices(rows: &[LineageRow]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.is_selectable())
        .map(|(i, _)| i)
        .collect()
}

/// Flatten a tree into renderable rows in depth-first, top-to-bottom order —
/// the order a `git log --graph`-style rail expects. `sessions` is used to
/// derive each node's activity-segment rows (see "Design notes" below) — the
/// same slice `build_tree` used to construct `root`, passed again here
/// rather than threaded through the tree itself, since segment math needs
/// live `SessionSummary` fields (`event_count`, `forked_from`, `merge`) that
/// `LineageNode` deliberately doesn't carry.
///
/// ### Design notes: segment boundaries
///
/// Each node's own timeline is chopped into activity windows at the points
/// where fork children branch off it and merge back into it — all on the
/// SAME counter (`SessionSummary::event_count` == `ForkedFrom::
/// transcript_seq` == `ForkMerge::merged_seq`, the transcript's own
/// sequence counter), so the boundaries and their deltas are plain
/// arithmetic over data already in memory, no extra fetch:
///
/// - `0` (the node's own creation).
/// - Each FORK child's `forked_from.transcript_seq` (subagents don't share
///   this timeline relationship — spec 0014 doesn't stamp a parent-timeline
///   position the way spec 0078 forks do — so they're skipped as
///   checkpoints and simply recursed into in place).
/// - Each fork child's `merge.merged_seq`, but ONLY when it actually merged
///   (`ForkMergeMode::Result`) — a discard never injects anything into the
///   parent's transcript, so it contributes no further checkpoint beyond
///   its own fork-out point.
/// - The node's own current `event_count` as the final checkpoint — except
///   when the node ITSELF has a terminal outcome (it's a fork that has
///   since merged/discarded, per its own `merge`), in which case its own
///   timeline effectively froze at `merge.at_ms` and that's used as the
///   final segment's end instead of "now".
///
/// A leaf (no fork/subagent children at all) still gets exactly one segment
/// — its whole life is a single window, computed the same way as any
/// node's trailing "since the last checkpoint" segment would be. This
/// keeps every node's activity visible somewhere, not just nodes with
/// forks; a node whose only children are subagents falls out of the same
/// general loop with an identical result (subagents never advance the
/// checkpoint, so the whole lifetime becomes one trailing segment,
/// positioned after their subtrees).
///
/// A window with zero messages in it (two consecutive checkpoints land on
/// the same `transcript_seq`, e.g. nothing happened on the parent while a
/// fork was outstanding) is skipped entirely rather than rendered as a "0
/// msgs" line — true of every window, not just the trailing one.
pub fn flatten(root: &LineageNode, sessions: &[SessionSummary]) -> Vec<LineageRow> {
    let by_id: HashMap<&str, &SessionSummary> =
        sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut out = Vec::new();
    flatten_rec(root, &by_id, 0, &[], true, &mut out);
    out
}

fn flatten_rec(
    node: &LineageNode,
    by_id: &HashMap<&str, &SessionSummary>,
    depth: usize,
    rails: &[bool],
    is_last: bool,
    out: &mut Vec<LineageRow>,
) {
    out.push(LineageRow {
        depth,
        is_last,
        rails: rails.to_vec(),
        kind: LineageRowKind::Node {
            session_id: node.session_id.clone(),
            edge: node.edge,
        },
    });
    let mut child_rails = rails.to_vec();
    if depth > 0 {
        child_rails.push(!is_last);
    }

    // A node's own timeline can only be carved into segments with its
    // summary in hand (event_count, created_at, merge). Without one — e.g.
    // it was deleted between `build_tree` and this call — fall back to
    // plain recursion with no segment rows for this node; there's nothing
    // to compute them from.
    let Some(summary) = by_id.get(node.session_id.as_str()).copied() else {
        let n = node.children.len();
        for (i, child) in node.children.iter().enumerate() {
            push_child(child, by_id, depth + 1, &child_rails, i + 1 == n, out);
        }
        return;
    };

    if node.children.is_empty() {
        push_segment(
            out,
            depth + 1,
            &child_rails,
            summary.event_count,
            summary.created_at.timestamp_millis(),
            summary.merge.as_ref().map(|m| m.at_ms),
        );
        return;
    }

    // Walk this node's own timeline forward through its children in
    // display order (subagents first, then forks — see `build_subtree`),
    // emitting a segment for the gap since the last checkpoint immediately
    // before each fork's subtree, and advancing the checkpoint past that
    // fork's merge (if any) immediately after.
    let mut checkpoint_seq = 0u64;
    let mut checkpoint_ms = summary.created_at.timestamp_millis();
    let n = node.children.len();
    for (i, child) in node.children.iter().enumerate() {
        let child_is_last = i + 1 == n;
        if let LineageChild::Node(cn) = child {
            if cn.edge == LineageEdge::Fork {
                if let Some(forked) = by_id
                    .get(cn.session_id.as_str())
                    .and_then(|s| s.forked_from.as_ref())
                {
                    push_segment(
                        out,
                        depth + 1,
                        &child_rails,
                        forked.transcript_seq.saturating_sub(checkpoint_seq),
                        checkpoint_ms,
                        Some(forked.at_ms),
                    );
                    checkpoint_seq = forked.transcript_seq;
                    checkpoint_ms = forked.at_ms;
                }
            }
        }
        push_child(child, by_id, depth + 1, &child_rails, child_is_last, out);
        if let LineageChild::Node(cn) = child {
            if cn.edge == LineageEdge::Fork {
                if let Some(m) = by_id
                    .get(cn.session_id.as_str())
                    .and_then(|s| s.merge.as_ref())
                    .filter(|m| m.mode == ForkMergeMode::Result)
                {
                    checkpoint_seq = m.merged_seq;
                    checkpoint_ms = m.at_ms;
                }
            }
        }
    }
    push_segment(
        out,
        depth + 1,
        &child_rails,
        summary.event_count.saturating_sub(checkpoint_seq),
        checkpoint_ms,
        summary.merge.as_ref().map(|m| m.at_ms),
    );
}

fn push_child(
    child: &LineageChild,
    by_id: &HashMap<&str, &SessionSummary>,
    depth: usize,
    rails: &[bool],
    is_last: bool,
    out: &mut Vec<LineageRow>,
) {
    match child {
        LineageChild::Node(cn) => flatten_rec(cn, by_id, depth, rails, is_last, out),
        LineageChild::More(count) => out.push(LineageRow {
            depth,
            is_last,
            rails: rails.to_vec(),
            kind: LineageRowKind::More(*count),
        }),
    }
}

/// Push one activity-segment row for the window
/// `[start_ms, end_ms.unwrap_or(<now, at render time>))`, unless it's empty
/// — a `0`-message window isn't worth a "0 msgs" line.
fn push_segment(
    out: &mut Vec<LineageRow>,
    depth: usize,
    rails: &[bool],
    delta_events: u64,
    start_ms: i64,
    end_ms: Option<i64>,
) {
    if delta_events == 0 {
        return;
    }
    out.push(LineageRow {
        depth,
        is_last: false,
        rails: rails.to_vec(),
        kind: LineageRowKind::Segment {
            delta_events,
            start_ms,
            end_ms,
        },
    });
}

/// Status glyph for a node — reuses [`SessionState::glyph`], the same
/// vocabulary the session list and `/tasks` popup already use, rather than
/// inventing a parallel icon set.
pub fn status_glyph(state: SessionState) -> &'static str {
    state.glyph()
}

/// Compact elapsed-time label (`"3s"`, `"12m34s"`) from `since_ms` (epoch
/// ms) to `now_ms`.
pub fn format_elapsed_ms(since_ms: i64, now_ms: i64) -> String {
    let secs = now_ms.saturating_sub(since_ms).max(0) / 1000;
    let m = secs / 60;
    let s = secs % 60;
    if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Renderable text for one activity-segment row: `"N msg(s) · elapsed"`.
/// `end_ms` is the segment's own end when known, else `now_ms` (the render
/// frame's live clock) — same split `render_lineage_row` used to take
/// `now_ms` for per-node stats before those moved to segments. Cost is
/// deliberately not shown here (unlike the old per-node stats label): it's
/// a single cumulative total on `SessionSummary`, with no per-checkpoint
/// snapshot the way `event_count` has via `transcript_seq`/`merged_seq`, so
/// there's no correct way to attribute it to one window rather than another.
pub fn segment_label(delta_events: u64, start_ms: i64, end_ms: Option<i64>, now_ms: i64) -> String {
    let elapsed = format_elapsed_ms(start_ms, end_ms.unwrap_or(now_ms));
    let unit = if delta_events == 1 { "msg" } else { "msgs" };
    format!("{delta_events} {unit} \u{00b7} {elapsed}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentd_protocol::{ForkMerge, ForkedFrom};
    use chrono::{TimeZone, Utc};

    fn base(id: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            harness: "smith".into(),
            cwd: "/tmp".into(),
            title: None,
            state: SessionState::Running,
            created_at: Utc.timestamp_opt(0, 0).unwrap(),
            last_event_at: None,
            cost_usd: None,
            model: None,
            worktree: None,
            pending_input: false,
            last_prompt: None,
            event_count: 0,
            has_pty: false,
            mode: None,
            pinned: false,
            position: 0,
            group_id: None,
            parent_session_id: None,
            last_pty_at_ms: None,
            approval_mode: agentd_protocol::ApprovalMode::Manual,
            kind: SessionKind::User,
            archived: false,
            operator_loop_disabled: false,
            needs_attention: false,
            forked_from: None,
            merge: None,
        }
    }

    fn forked_from(mut s: SessionSummary, parent: &str) -> SessionSummary {
        s.forked_from = Some(ForkedFrom {
            session_id: parent.to_string(),
            transcript_seq: 0,
            at_ms: 0,
        });
        s
    }

    fn subagent_of(mut s: SessionSummary, parent: &str) -> SessionSummary {
        s.kind = SessionKind::Subagent;
        s.parent_session_id = Some(parent.to_string());
        s
    }

    /// Like `forked_from`, but with explicit `transcript_seq`/`at_ms` —
    /// needed for segment-boundary tests, where `forked_from`'s always-zero
    /// defaults would collapse every window to zero length.
    fn forked_from_at(
        mut s: SessionSummary,
        parent: &str,
        transcript_seq: u64,
        at_ms: i64,
    ) -> SessionSummary {
        s.forked_from = Some(ForkedFrom {
            session_id: parent.to_string(),
            transcript_seq,
            at_ms,
        });
        s
    }

    fn merged_at(
        mut s: SessionSummary,
        mode: ForkMergeMode,
        merged_seq: u64,
        at_ms: i64,
    ) -> SessionSummary {
        s.merge = Some(ForkMerge {
            mode,
            at_ms,
            merged_seq,
        });
        s
    }

    fn with_created_at_ms(mut s: SessionSummary, ms: i64) -> SessionSummary {
        s.created_at = Utc.timestamp_millis_opt(ms).unwrap();
        s
    }

    fn with_event_count(mut s: SessionSummary, n: u64) -> SessionSummary {
        s.event_count = n;
        s
    }

    /// Segment rows' `delta_events`, in flattened (on-screen) order — the
    /// shared assertion helper for the segment-boundary tests below.
    fn segment_deltas(rows: &[LineageRow]) -> Vec<u64> {
        rows.iter()
            .filter_map(|r| match &r.kind {
                LineageRowKind::Segment { delta_events, .. } => Some(*delta_events),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn single_session_is_a_lone_root() {
        let sessions = vec![base("a")];
        let tree = build_tree("a", &sessions).expect("tree");
        assert_eq!(tree.session_id, "a");
        assert_eq!(tree.edge, LineageEdge::Root);
        assert!(tree.children.is_empty());
    }

    #[test]
    fn unknown_focus_session_returns_none() {
        let sessions = vec![base("a")];
        assert!(build_tree("ghost", &sessions).is_none());
    }

    #[test]
    fn fork_and_subagent_children_coexist_with_distinct_edges() {
        let sessions = vec![
            base("a"),
            forked_from(base("a-fork"), "a"),
            subagent_of(base("a-sub"), "a"),
        ];
        let tree = build_tree("a", &sessions).expect("tree");
        assert_eq!(tree.children.len(), 2);
        // Subagents sort before forks (see build_subtree).
        let LineageChild::Node(first) = &tree.children[0] else {
            panic!("expected node")
        };
        assert_eq!(first.session_id, "a-sub");
        assert_eq!(first.edge, LineageEdge::Subagent);
        let LineageChild::Node(second) = &tree.children[1] else {
            panic!("expected node")
        };
        assert_eq!(second.session_id, "a-fork");
        assert_eq!(second.edge, LineageEdge::Fork);
    }

    #[test]
    fn opening_the_view_from_any_descendant_finds_the_same_root() {
        let sessions = vec![
            base("a"),
            forked_from(base("b"), "a"),
            forked_from(base("c"), "b"),
        ];
        for focus in ["a", "b", "c"] {
            let tree = build_tree(focus, &sessions).expect("tree");
            assert_eq!(
                tree.session_id, "a",
                "focus {focus} should resolve to root a"
            );
        }
    }

    #[test]
    fn recursive_fork_of_a_fork_nests_at_depth_two() {
        let sessions = vec![
            base("a"),
            forked_from(base("b"), "a"),
            forked_from(base("c"), "b"),
        ];
        let tree = build_tree("a", &sessions).unwrap();
        let rows = flatten(&tree, &sessions);
        let c_row = rows
            .iter()
            .find(|r| r.session_id() == Some("c"))
            .expect("c row");
        assert_eq!(c_row.depth, 2);
    }

    #[test]
    fn breadth_beyond_cap_collapses_into_a_more_marker() {
        let mut sessions = vec![base("root")];
        for i in 0..(MAX_SIBLINGS + 5) {
            sessions.push(forked_from(base(&format!("f{i}")), "root"));
        }
        let tree = build_tree("root", &sessions).unwrap();
        assert_eq!(tree.children.len(), MAX_SIBLINGS + 1); // +1 for the More marker
        let last = tree.children.last().unwrap();
        assert_eq!(*last, LineageChild::More(5));
    }

    #[test]
    fn depth_beyond_cap_collapses_into_a_more_marker() {
        // A straight-line chain deeper than MAX_DEPTH.
        let mut sessions = vec![base("s0")];
        for i in 1..(MAX_DEPTH + 3) {
            sessions.push(forked_from(base(&format!("s{i}")), &format!("s{}", i - 1)));
        }
        let tree = build_tree("s0", &sessions).unwrap();
        let rows = flatten(&tree, &sessions);
        // Depths 0..MAX_DEPTH-1 render as real nodes; beyond that collapses.
        assert!(rows
            .iter()
            .any(|r| matches!(r.kind, LineageRowKind::More(_))));
        let deepest_node_depth = rows
            .iter()
            .filter(|r| r.is_selectable())
            .map(|r| r.depth)
            .max()
            .unwrap();
        assert!(deepest_node_depth < MAX_DEPTH);
    }

    #[test]
    fn fork_status_reflects_merge_outcome() {
        let mut open = forked_from(base("f"), "root");
        assert_eq!(ForkStatus::of(&open), ForkStatus::Open);

        open.merge = Some(ForkMerge {
            mode: ForkMergeMode::Result,
            at_ms: 0,
            merged_seq: 0,
        });
        assert_eq!(ForkStatus::of(&open), ForkStatus::Merged);

        open.merge = Some(ForkMerge {
            mode: ForkMergeMode::Discard,
            at_ms: 0,
            merged_seq: 0,
        });
        assert_eq!(ForkStatus::of(&open), ForkStatus::Discarded);
    }

    #[test]
    fn rail_prefix_matches_git_log_graph_shape() {
        // root
        // ├─ a
        // │  └─ b
        // └─ c
        let sessions = vec![
            base("root"),
            forked_from(base("a"), "root"),
            forked_from(base("b"), "a"),
            forked_from(base("c"), "root"),
        ];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions);
        let by_id: HashMap<&str, &LineageRow> = rows
            .iter()
            .filter_map(|r| r.session_id().map(|id| (id, r)))
            .collect();

        assert_eq!(by_id["root"].rail_prefix(), "");
        assert_eq!(by_id["a"].rail_prefix(), "\u{251c}\u{2500}");
        assert_eq!(by_id["b"].rail_prefix(), "\u{2502} \u{2514}\u{2500}");
        assert_eq!(by_id["c"].rail_prefix(), "\u{2514}\u{2500}");
    }

    #[test]
    fn segment_label_reports_message_count_and_elapsed() {
        let label = segment_label(42, 0, Some(65_000), 999_999);
        assert!(label.contains("42 msgs"));
        assert!(label.contains("1m05s"));
    }

    #[test]
    fn segment_label_singular_for_one_message() {
        let label = segment_label(1, 0, Some(1_000), 999_999);
        assert!(label.contains("1 msg "), "expected singular 'msg': {label}");
        assert!(!label.contains("msgs"));
    }

    #[test]
    fn segment_label_falls_back_to_now_when_end_is_open() {
        // An open-ended segment (`end_ms: None`) measures against the live
        // render-time clock (`now_ms`), not a baked-in end.
        let label = segment_label(3, 0, None, 5_000);
        assert!(
            label.contains("5s"),
            "expected elapsed against now_ms: {label}"
        );
    }

    #[test]
    fn has_lineage_is_false_for_an_ordinary_session() {
        let sessions = vec![base("a"), base("b")];
        assert!(!has_lineage("a", &sessions));
    }

    #[test]
    fn has_lineage_is_true_for_a_fork_itself() {
        let sessions = vec![base("root"), forked_from(base("f"), "root")];
        assert!(has_lineage("f", &sessions));
    }

    #[test]
    fn has_lineage_is_true_for_a_session_with_a_fork_descendant() {
        let sessions = vec![base("root"), forked_from(base("f"), "root")];
        assert!(has_lineage("root", &sessions));
    }

    #[test]
    fn has_lineage_is_true_for_a_session_with_a_subagent_descendant() {
        let sessions = vec![base("root"), subagent_of(base("sub"), "root")];
        assert!(has_lineage("root", &sessions));
    }

    #[test]
    fn has_lineage_is_false_for_an_unknown_session_id() {
        let sessions = vec![base("root")];
        assert!(!has_lineage("ghost", &sessions));
    }

    #[test]
    fn selectable_indices_skips_more_markers() {
        let mut sessions = vec![base("root")];
        for i in 0..(MAX_SIBLINGS + 2) {
            sessions.push(forked_from(base(&format!("f{i}")), "root"));
        }
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions);
        let selectable = selectable_indices(&rows);
        assert_eq!(
            selectable.len(),
            MAX_SIBLINGS + 1,
            "the collapsed +N more row must not count as selectable"
        );
        for idx in selectable {
            assert!(rows[idx].is_selectable());
        }
    }

    #[test]
    fn leaf_node_gets_a_single_trailing_segment() {
        let root = with_event_count(with_created_at_ms(base("root"), 0), 9);
        let sessions = vec![root];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions);
        assert_eq!(
            segment_deltas(&rows),
            vec![9],
            "a childless node still gets one segment covering its whole life"
        );
        let LineageRowKind::Segment {
            start_ms, end_ms, ..
        } = &rows
            .iter()
            .find(|r| matches!(r.kind, LineageRowKind::Segment { .. }))
            .unwrap()
            .kind
        else {
            unreachable!()
        };
        assert_eq!(*start_ms, 0);
        assert_eq!(
            *end_ms, None,
            "a still-open node's trailing segment has no baked-in end — it's \"now\" at render time"
        );
    }

    #[test]
    fn leaf_forks_trailing_segment_ends_at_its_own_merge_not_now() {
        // A fork that has itself merged/discarded froze at that instant —
        // its own trailing segment must end there, not keep growing against
        // a live "now" the way a still-open node's does.
        let fork = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("f"), "root", 5, 1_000), 1_000),
                7,
            ),
            ForkMergeMode::Result,
            10,
            3_000,
        );
        let sessions = vec![base("root"), fork];
        // `build_tree` walks up to the topmost ancestor — here that's
        // "root", with "f" as its child — so "f"'s own leaf segment is the
        // SECOND segment row (root's own "before f forked" segment comes
        // first); find it by its distinctive delta rather than assuming
        // position.
        let tree = build_tree("f", &sessions).unwrap();
        let rows = flatten(&tree, &sessions);
        let seg = rows
            .iter()
            .find_map(|r| match &r.kind {
                LineageRowKind::Segment {
                    delta_events: 7,
                    start_ms,
                    end_ms,
                } => Some((*start_ms, *end_ms)),
                _ => None,
            })
            .expect("f's own leaf segment (delta_events = f.event_count = 7)");
        assert_eq!(seg, (1_000, Some(3_000)));
    }

    #[test]
    fn single_open_fork_produces_a_pre_fork_and_a_fork_own_segment() {
        let root = with_event_count(with_created_at_ms(base("root"), 0), 20);
        let fork = with_event_count(
            with_created_at_ms(forked_from_at(base("f"), "root", 12, 500), 500),
            2,
        );
        let sessions = vec![root, fork];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions);
        // root: [0, 12) before the fork, then [12, 20) since the fork (still
        // open, so it's a trailing "to now" segment); f: its own [0, 2)
        // life, still open too.
        assert_eq!(segment_deltas(&rows), vec![12, 2, 8]);
    }

    #[test]
    fn multiple_forks_mixed_merged_discarded_open_produce_the_expected_segment_sequence() {
        // root -> A (merged) -> B (discarded) -> C (still open), with root
        // continuing to accrue its own messages between each.
        let root = with_event_count(with_created_at_ms(base("root"), 0), 30);
        let a = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("a"), "root", 5, 1_000), 1_000),
                7,
            ),
            ForkMergeMode::Result,
            10,
            3_000,
        );
        let b = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("b"), "root", 15, 4_000), 4_000),
                3,
            ),
            ForkMergeMode::Discard,
            // A discard's own `merged_seq`/`at_ms` must NOT move the
            // parent's checkpoint — deliberately set to values that would
            // fail the assertions below if the implementation used them.
            999,
            5_000,
        );
        let c = with_event_count(
            with_created_at_ms(forked_from_at(base("c"), "root", 20, 6_000), 6_000),
            2,
        );
        let sessions = vec![root, a, b, c];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions);
        assert_eq!(
            segment_deltas(&rows),
            vec![
                5, // root, before A forked: [0, 5)
                7, // A's own whole life
                5, // root, between A merging back (seq 10) and B forking (seq 15)
                3, // B's own whole life
                5, // root, between B forking (seq 15, a discard doesn't move the
                // checkpoint past it) and C forking (seq 20)
                2,  // C's own whole life
                10, // root, since C forked (seq 20) to root's current event_count (30)
            ]
        );
    }

    #[test]
    fn a_merge_boundary_with_zero_gap_is_skipped_not_rendered_as_zero() {
        // A fork whose merge lands exactly where the next fork branches off
        // (root did nothing of its own in between) must not render a "0
        // msgs" line.
        let root = with_event_count(with_created_at_ms(base("root"), 0), 12);
        let a = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("a"), "root", 5, 1_000), 1_000),
                4,
            ),
            ForkMergeMode::Result,
            8,
            2_000,
        );
        // b forks out exactly at seq 8 — the same point a merged back.
        let b = with_event_count(
            with_created_at_ms(forked_from_at(base("b"), "root", 8, 2_000), 2_000),
            1,
        );
        let sessions = vec![root, a, b];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions);
        assert_eq!(
            segment_deltas(&rows),
            vec![
                5, // root before a forked
                4, // a's own life
                // no zero-length "a merged to b forked" segment
                1, // b's own life
                4, // root since b forked (seq 8) to now (event_count 12)
            ]
        );
    }

    #[test]
    fn subagent_children_do_not_split_the_parent_timeline() {
        // A node with only subagent children (no forks) gets exactly one
        // trailing segment for its whole life, positioned after the
        // subagent's own subtree — subagents don't stamp a parent-timeline
        // checkpoint the way forks do (spec 0014 has no `transcript_seq`).
        let root = with_event_count(with_created_at_ms(base("root"), 0), 9);
        let sub = with_event_count(with_created_at_ms(subagent_of(base("s"), "root"), 500), 2);
        let sessions = vec![root, sub];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions);
        assert_eq!(
            segment_deltas(&rows),
            vec![2, 9],
            "s's own leaf segment, then root's whole-life segment (unsplit by the subagent)"
        );
        // And root's segment must come after s's entire subtree in render
        // order (it's the last row).
        assert!(matches!(
            rows.last().unwrap().kind,
            LineageRowKind::Segment {
                delta_events: 9,
                ..
            }
        ));
    }

    #[test]
    fn segment_rows_are_never_selectable() {
        let root = with_event_count(with_created_at_ms(base("root"), 0), 5);
        let sessions = vec![root];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions);
        let selectable = selectable_indices(&rows);
        for idx in selectable {
            assert!(!matches!(rows[idx].kind, LineageRowKind::Segment { .. }));
        }
        assert!(
            rows.iter()
                .any(|r| matches!(r.kind, LineageRowKind::Segment { .. })),
            "sanity: this tree does have a segment row"
        );
    }

    #[test]
    fn segment_row_rail_prefix_never_uses_a_branch_connector() {
        // Segment rows always render a plain continuing bar, never a
        // "├─"/"└─" branch connector — they're annotations on the rail, not
        // tree branches.
        let row = LineageRow {
            depth: 1,
            is_last: true,
            rails: vec![],
            kind: LineageRowKind::Segment {
                delta_events: 3,
                start_ms: 0,
                end_ms: Some(1_000),
            },
        };
        assert_eq!(row.rail_prefix(), "\u{2502} ");
    }
}
