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
            LineageRowKind::More(_) => None,
        }
    }

    pub fn is_selectable(&self) -> bool {
        matches!(self.kind, LineageRowKind::Node { .. })
    }

    /// Compact `git log --graph`-style rail prefix, e.g. `"│ ├─"` — glyphs
    /// only, no session label (callers append the edge glyph/status/stats
    /// after this).
    pub fn rail_prefix(&self) -> String {
        let mut out = String::new();
        for r in &self.rails {
            out.push_str(if *r { "\u{2502} " } else { "  " });
        }
        if self.depth > 0 {
            out.push_str(if self.is_last {
                "\u{2514}\u{2500}"
            } else {
                "\u{251c}\u{2500}"
            });
        }
        out
    }
}

/// Flatten a tree into renderable rows in depth-first, top-to-bottom order —
/// the order a `git log --graph`-style rail expects.
pub fn flatten(root: &LineageNode) -> Vec<LineageRow> {
    let mut out = Vec::new();
    flatten_rec(root, 0, &[], true, &mut out);
    out
}

fn flatten_rec(
    node: &LineageNode,
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
    let n = node.children.len();
    for (i, child) in node.children.iter().enumerate() {
        let child_is_last = i + 1 == n;
        match child {
            LineageChild::Node(cn) => flatten_rec(cn, depth + 1, &child_rails, child_is_last, out),
            LineageChild::More(count) => out.push(LineageRow {
                depth: depth + 1,
                is_last: child_is_last,
                rails: child_rails.clone(),
                kind: LineageRowKind::More(*count),
            }),
        }
    }
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

/// Compact per-node stats: message/turn count (`SessionSummary::event_count`
/// — the transcript's own sequence counter) and elapsed time since
/// creation, plus cost when the daemon has attributed any
/// (`SessionSummary::cost_usd`). Token counts are only ever emitted as
/// per-event `SessionEvent::Cost` deltas, never aggregated onto
/// `SessionSummary` — so they're omitted here rather than invented.
pub fn stats_label(summary: &SessionSummary, now_ms: i64) -> String {
    let elapsed = format_elapsed_ms(summary.created_at.timestamp_millis(), now_ms);
    let mut out = format!("{}msg {elapsed}", summary.event_count);
    if let Some(cost) = summary.cost_usd {
        if cost > 0.0 {
            out.push_str(&format!(" ${cost:.2}"));
        }
    }
    out
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
        let rows = flatten(&tree);
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
        let rows = flatten(&tree);
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
        });
        assert_eq!(ForkStatus::of(&open), ForkStatus::Merged);

        open.merge = Some(ForkMerge {
            mode: ForkMergeMode::Discard,
            at_ms: 0,
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
        let rows = flatten(&tree);
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
    fn stats_label_reports_message_count_elapsed_and_cost() {
        let mut s = base("a");
        s.event_count = 42;
        s.cost_usd = Some(0.5);
        let label = stats_label(&s, 65_000);
        assert!(label.contains("42msg"));
        assert!(label.contains("1m05s"));
        assert!(label.contains("$0.50"));
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
    fn stats_label_omits_cost_when_none_or_zero() {
        let mut s = base("a");
        s.cost_usd = None;
        assert!(!stats_label(&s, 0).contains('$'));
        s.cost_usd = Some(0.0);
        assert!(!stats_label(&s, 0).contains('$'));
    }
}
