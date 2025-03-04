//! Move commits and subtrees from one place to another.
//!
//! Under the hood, this makes use of Git's advanced rebase functionality, which
//! is also used to preserve merge commits using the `--rebase-merges` option.

use std::time::SystemTime;

use crate::core::eventlog::{EventLogDb, EventReplayer};
use crate::core::formatting::Glyphs;
use crate::core::graph::{make_graph, BranchOids, CommitGraph, HeadOid, MainBranchOid};
use crate::core::mergebase::MergeBaseDb;
use crate::core::rewrite::{execute_rebase_plan, make_rebase_plan};
use crate::util::get_main_branch_oid;
use crate::util::{
    get_branch_oid_to_names, get_db_conn, get_head_oid, get_repo, resolve_commits, GitExecutable,
    ResolveCommitsResult,
};

fn resolve_base_commit(graph: &CommitGraph, oid: git2::Oid) -> git2::Oid {
    let node = &graph[&oid];
    if node.is_main {
        oid
    } else {
        match node.parent {
            Some(parent_oid) => {
                if graph[&parent_oid].is_main {
                    oid
                } else {
                    resolve_base_commit(graph, parent_oid)
                }
            }
            None => oid,
        }
    }
}

/// Move a subtree from one place to another.
pub fn r#move(
    git_executable: &GitExecutable,
    source: Option<String>,
    dest: Option<String>,
    base: Option<String>,
    force_on_disk: bool,
) -> anyhow::Result<isize> {
    let repo = get_repo()?;
    let head_oid = get_head_oid(&repo)?;
    let (source, should_resolve_base_commit) = match (source, base) {
        (Some(_), Some(_)) => {
            println!("The --source and --base options cannot both be provided.");
            return Ok(1);
        }
        (Some(source), None) => (source, false),
        (None, Some(base)) => (base, true),
        (None, None) => {
            let source_oid = head_oid
            .expect(
                "No --source or --base argument was provided, and no OID for HEAD is available as a default",
            )
            .to_string();
            (source_oid, false)
        }
    };
    let dest = match dest {
        Some(dest) => dest,
        None => head_oid
            .expect(
                "No --dest argument was provided, and no OID for HEAD is available as a default",
            )
            .to_string(),
    };
    let (source_oid, dest_oid) = match resolve_commits(&repo, vec![source, dest])? {
        ResolveCommitsResult::Ok { commits } => match &commits.as_slice() {
            [source_commit, dest_commit] => (source_commit.id(), dest_commit.id()),
            _ => anyhow::bail!("Unexpected number of returns values from resolve_commits"),
        },
        ResolveCommitsResult::CommitNotFound { commit } => {
            println!("Commit not found: {}", commit);
            return Ok(1);
        }
    };

    let main_branch_oid = get_main_branch_oid(&repo)?;
    let branch_oid_to_names = get_branch_oid_to_names(&repo)?;
    let conn = get_db_conn(&repo)?;
    let merge_base_db = MergeBaseDb::new(&conn)?;
    let event_log_db = EventLogDb::new(&conn)?;
    let event_replayer = EventReplayer::from_event_log_db(&event_log_db)?;
    let event_cursor = event_replayer.make_default_cursor();
    let graph = make_graph(
        &repo,
        &merge_base_db,
        &event_replayer,
        event_cursor,
        &HeadOid(Some(source_oid)),
        &MainBranchOid(main_branch_oid),
        &BranchOids(branch_oid_to_names.keys().copied().collect()),
        true,
    )?;

    let source_oid = if should_resolve_base_commit {
        resolve_base_commit(&graph, source_oid)
    } else {
        source_oid
    };

    let glyphs = Glyphs::detect();
    let now = SystemTime::now();
    let event_tx_id = event_log_db.make_transaction_id(now, "move")?;
    let rebase_plan = make_rebase_plan(
        &repo,
        &merge_base_db,
        &graph,
        &MainBranchOid(main_branch_oid),
        source_oid,
    )?;
    let result = execute_rebase_plan(
        &glyphs,
        git_executable,
        &repo,
        event_tx_id,
        &rebase_plan,
        source_oid,
        dest_oid,
        force_on_disk,
    )?;
    Ok(result)
}
