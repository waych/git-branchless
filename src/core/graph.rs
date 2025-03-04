//! Renders the smartlog commit graph based on user activity.
//!
//! This is the basic data structure that most of branchless operates on.

use std::collections::{HashMap, HashSet, VecDeque};

use fn_error_context::context;
use log::warn;

use crate::core::eventlog::{CommitVisibility, Event, EventCursor, EventReplayer};
use crate::core::mergebase::MergeBaseDb;

/// The OID of the repo's HEAD reference.
#[derive(Debug)]
pub struct HeadOid(pub Option<git2::Oid>);

/// The OID that the repo's main branch points to.
#[derive(Debug)]
pub struct MainBranchOid(pub git2::Oid);

/// The OIDs of any branches whose pointed-to commits should be included in the
/// commit graph.
#[derive(Debug)]
pub struct BranchOids(pub HashSet<git2::Oid>);

/// The OIDs of any visible commits that should be included in the commit graph.
#[derive(Debug)]
pub struct CommitOids(pub HashSet<git2::Oid>);

/// Node contained in the smartlog commit graph.
#[derive(Debug)]
pub struct Node<'repo> {
    /// The underlying commit object.
    pub commit: git2::Commit<'repo>,

    /// The OID of the parent node in the smartlog commit graph.
    ///
    /// This is different from inspecting `commit.parents()`, since the smartlog
    /// will hide most nodes from the commit graph, including parent nodes.
    pub parent: Option<git2::Oid>,

    /// The OIDs of the children nodes in the smartlog commit graph.
    pub children: HashSet<git2::Oid>,

    /// Indicates that this is a commit to the main branch.
    ///
    /// These commits are considered to be immutable and should never leave the
    /// `main` state. However, this can still happen sometimes if the user's
    /// workflow is different than expected.
    pub is_main: bool,

    /// Indicates that this commit should be considered "visible".
    ///
    /// A visible commit is a commit that hasn't been checked into the main
    /// branch, but the user is actively working on. We may infer this from user
    /// behavior, e.g. they committed something recently, so they are now working
    /// on it.
    ///
    /// In contrast, a hidden commit is a commit that hasn't been checked into
    /// the main branch, and the user is no longer working on. We may infer this
    /// from user behavior, e.g. they have rebased a commit and no longer want to
    /// see the old version of that commit. The user can also manually hide
    /// commits.
    ///
    /// Occasionally, a main commit can be marked as hidden, such as if a commit
    /// in the main branch has been rewritten. We don't expect this to happen in
    /// the monorepo workflow, but it can happen in other workflows where you
    /// commit directly to the main branch and then later rewrite the commit.
    pub is_visible: bool,

    /// The latest event to affect this commit.
    ///
    /// It's possible that no event affected this commit, and it was simply
    /// visible due to a reference pointing to it. In that case, this field is
    /// `None`.
    pub event: Option<Event>,
}

/// Graph of commits that the user is working on.
pub type CommitGraph<'repo> = HashMap<git2::Oid, Node<'repo>>;

fn find_path_to_merge_base_internal<'repo>(
    repo: &'repo git2::Repository,
    merge_base_db: &MergeBaseDb,
    commit_oid: git2::Oid,
    target_oid: git2::Oid,
    mut visited_commit_callback: impl FnMut(git2::Oid),
) -> anyhow::Result<Option<Vec<git2::Commit<'repo>>>> {
    let mut queue = VecDeque::new();
    visited_commit_callback(commit_oid);
    queue.push_back(vec![repo.find_commit(commit_oid)?]);
    let merge_base_oid = merge_base_db.get_merge_base_oid(repo, commit_oid, target_oid)?;
    while let Some(path) = queue.pop_front() {
        let last_commit = path
            .last()
            .expect("find_path_to_merge_base: empty path in queue");
        if last_commit.id() == target_oid {
            return Ok(Some(path));
        }
        if Some(last_commit.id()) == merge_base_oid {
            // We've hit the common ancestor of these two commits without
            // finding a path between them. That means it's impossible to find a
            // path between them by traversing more ancestors. Possibly the
            // caller passed them in in the wrong order, i.e. `commit_oid` is
            // actually a parent of `target_oid`.
            continue;
        }

        for parent in last_commit.parents() {
            visited_commit_callback(parent.id());
            let mut new_path = path.clone();
            new_path.push(parent);
            queue.push_back(new_path);
        }
    }
    Ok(None)
}

/// Find a shortest path between the given commits.
///
/// This is particularly important for multi-parent commits (i.e. merge commits).
/// If we don't happen to traverse the correct parent, we may end up traversing a
/// huge amount of commit history, with a significant performance hit.
///
/// Args:
/// * `repo`: The Git repository.
/// * `commit_oid`: The OID of the commit to start at. We take parents of the
/// provided commit until we end up at the target OID.
/// * `target_oid`: The OID of the commit to end at.
///
/// Returns: A path of commits from `commit_oid` through parents to `target_oid`.
/// The path includes `commit_oid` at the beginning and `target_oid` at the end.
/// If there is no such path, returns `None`.
#[context("Finding path from {:?} to {:?}", commit_oid, target_oid)]
pub fn find_path_to_merge_base<'repo>(
    repo: &'repo git2::Repository,
    merge_base_db: &MergeBaseDb,
    commit_oid: git2::Oid,
    target_oid: git2::Oid,
) -> anyhow::Result<Option<Vec<git2::Commit<'repo>>>> {
    find_path_to_merge_base_internal(repo, merge_base_db, commit_oid, target_oid, |_commit| {})
}

/// Find additional commits that should be displayed.
///
/// For example, if you check out a commit that has intermediate parent commits
/// between it and the main branch, those intermediate commits should be shown
/// (or else you won't get a good idea of the line of development that happened
/// for this commit since the main branch).
#[context("Walking from commits: {:?}", commit_oids)]
fn walk_from_commits<'repo>(
    repo: &'repo git2::Repository,
    merge_base_db: &MergeBaseDb,
    event_replayer: &EventReplayer,
    event_cursor: EventCursor,
    main_branch_oid: &MainBranchOid,
    commit_oids: &CommitOids,
) -> anyhow::Result<CommitGraph<'repo>> {
    let mut graph: CommitGraph = Default::default();

    for commit_oid in &commit_oids.0 {
        let commit = repo.find_commit(*commit_oid);
        let current_commit = match commit {
            Ok(commit) => commit,

            // Commit may have been garbage-collected.
            Err(_) => continue,
        };

        let merge_base_oid =
            merge_base_db.get_merge_base_oid(repo, current_commit.id(), main_branch_oid.0)?;
        let path_to_merge_base = match merge_base_oid {
            // Occasionally we may find a commit that has no merge-base with the
            // main branch. For example: a rewritten initial commit. This is
            // somewhat pathological. We'll just add it to the graph as a
            // standalone component and hope it works out.
            None => vec![current_commit],
            Some(merge_base_oid) => {
                let path_to_merge_base = find_path_to_merge_base(
                    repo,
                    merge_base_db,
                    current_commit.id(),
                    merge_base_oid,
                )?;
                match path_to_merge_base {
                    None => {
                        warn!("No path to merge-base for commit {}", current_commit.id());
                        continue;
                    }
                    Some(path_to_merge_base) => path_to_merge_base,
                }
            }
        };

        for current_commit in path_to_merge_base.iter() {
            if graph.contains_key(&current_commit.id()) {
                // This commit (and all of its parents!) should be in the graph
                // already, so no need to continue this iteration.
                break;
            }

            let visibility =
                event_replayer.get_cursor_commit_visibility(event_cursor, current_commit.id());
            let is_visible = match visibility {
                Some(CommitVisibility::Visible) | None => true,
                Some(CommitVisibility::Hidden) => false,
            };

            let is_main = match merge_base_oid {
                Some(merge_base_oid) => (current_commit.id() == merge_base_oid),
                None => false,
            };

            let event = event_replayer
                .get_cursor_commit_latest_event(event_cursor, current_commit.id())
                .cloned();
            graph.insert(
                current_commit.id(),
                Node {
                    commit: current_commit.clone(),
                    parent: None,
                    children: HashSet::new(),
                    is_main,
                    is_visible,
                    event,
                },
            );
        }

        if let Some(merge_base_oid) = merge_base_oid {
            if !graph.contains_key(&merge_base_oid) {
                warn!("Could not find merge base OID {}", merge_base_oid);
            }
        }
    }

    // Find immediate parent-child links.
    let links: Vec<(git2::Oid, git2::Oid)> = graph
        .iter()
        .filter(|(_child_oid, node)| !node.is_main)
        .flat_map(|(child_oid, node)| {
            node.commit
                .parent_ids()
                .filter(|parent_oid| graph.contains_key(parent_oid))
                .map(move |parent_oid| (*child_oid, parent_oid))
        })
        .collect();
    for (child_oid, parent_oid) in links.iter() {
        graph.get_mut(child_oid).unwrap().parent = Some(*parent_oid);
        graph
            .get_mut(parent_oid)
            .unwrap()
            .children
            .insert(*child_oid);
    }

    Ok(graph)
}

fn should_hide(
    cache: &mut HashMap<git2::Oid, bool>,
    graph: &CommitGraph,
    unhideable_oids: &HashSet<git2::Oid>,
    oid: &git2::Oid,
) -> bool {
    let result = {
        match cache.get(oid) {
            Some(result) => *result,
            None => {
                if unhideable_oids.contains(oid) {
                    false
                } else {
                    let node = &graph[oid];
                    if node.is_main {
                        // We only want to hide "uninteresting" main branch nodes. Main
                        // branch nodes should normally be visible, so instead, we only hide
                        // it if it's *not* visible, which is an anomaly that should be
                        // addressed by the user.
                        node.is_visible
                            && node
                                .children
                                .iter()
                                // Don't consider the next commit in the main branch as a child
                                // for hiding purposes.
                                .filter(|child_oid| !graph[child_oid].is_main)
                                .all(|child_oid| {
                                    should_hide(cache, graph, unhideable_oids, child_oid)
                                })
                    } else {
                        !node.is_visible
                            && node.children.iter().all(|child_oid| {
                                should_hide(cache, graph, unhideable_oids, child_oid)
                            })
                    }
                }
            }
        }
    };
    cache.insert(*oid, result);
    result
}

/// Remove commits from the graph according to their status.
fn do_remove_commits(graph: &mut CommitGraph, head_oid: &HeadOid, branch_oids: &BranchOids) {
    // OIDs which are pointed to by HEAD or a branch should not be hidden.
    // Therefore, we can't hide them *or* their ancestors.
    let mut unhideable_oids = branch_oids.0.clone();
    if let Some(head_oid) = head_oid.0 {
        unhideable_oids.insert(head_oid);
    }

    let mut cache = HashMap::new();
    let all_oids_to_hide: HashSet<git2::Oid> = graph
        .keys()
        .filter(|oid| should_hide(&mut cache, graph, &unhideable_oids, oid))
        .cloned()
        .collect();

    // Actually update the graph and delete any parent-child links, as
    // appropriate.
    for oid in all_oids_to_hide {
        let parent_oid = graph[&oid].parent;
        graph.remove(&oid);
        match parent_oid {
            Some(parent_oid) if graph.contains_key(&parent_oid) => {
                graph.get_mut(&parent_oid).unwrap().children.remove(&oid);
            }
            _ => {}
        }
    }
}

/// Construct the smartlog graph for the repo.
///
/// Args:
/// * `repo`: The Git repository.
/// * `merge_base_db`: The merge-base database.
/// * `event_replayer`: The event replayer.
/// * `head_oid`: The OID of the repository's `HEAD` reference.
/// * `main_branch_oid`: The OID of the main branch.
/// * `branch_oids`: The set of OIDs pointed to by branches.
/// * `hide_commits`: If set to `True`, then, after constructing the graph,
/// remove nodes from it that appear to be hidden by user activity. This should
/// be set to `True` for most display-related purposes.
///
/// Returns: A tuple of the head OID and the commit graph.
#[context("Creating commit graph")]
pub fn make_graph<'repo>(
    repo: &'repo git2::Repository,
    merge_base_db: &MergeBaseDb,
    event_replayer: &EventReplayer,
    event_cursor: EventCursor,
    head_oid: &HeadOid,
    main_branch_oid: &MainBranchOid,
    branch_oids: &BranchOids,
    remove_commits: bool,
) -> anyhow::Result<CommitGraph<'repo>> {
    let mut commit_oids: HashSet<git2::Oid> = event_replayer
        .get_cursor_active_oids(event_cursor)
        .into_iter()
        .collect();
    commit_oids.extend(branch_oids.0.iter().cloned());
    if let HeadOid(Some(head_oid)) = head_oid {
        commit_oids.insert(*head_oid);
    }
    let commit_oids = &CommitOids(commit_oids);
    let mut graph = walk_from_commits(
        repo,
        merge_base_db,
        event_replayer,
        event_cursor,
        main_branch_oid,
        commit_oids,
    )?;
    if remove_commits {
        do_remove_commits(&mut graph, head_oid, branch_oids);
    }
    Ok(graph)
}

#[test]
fn test_find_path_to_merge_base_stop_early() -> anyhow::Result<()> {
    crate::testing::with_git(|git| {
        git.init_repo()?;
        let test1_oid = git.commit_file("test1", 1)?;
        let test2_oid = git.commit_file("test2", 2)?;
        git.detach_head()?;
        let test3_oid = git.commit_file("test3", 3)?;

        let repo = git.get_repo()?;
        let conn = crate::util::get_db_conn(&repo)?;
        let merge_base_db = MergeBaseDb::new(&conn)?;

        let mut seen_oids = HashSet::new();
        let path =
            find_path_to_merge_base_internal(&repo, &merge_base_db, test2_oid, test3_oid, |oid| {
                seen_oids.insert(oid);
            })?;
        assert!(path.is_none());

        println!("Seen OIDs is {:?}", &seen_oids);
        assert!(seen_oids.contains(&test2_oid));
        assert!(!seen_oids.contains(&test3_oid));
        assert!(!seen_oids.contains(&test1_oid));

        Ok(())
    })
}
