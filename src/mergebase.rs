//! Persistent storage to cache merge-base queries.
//!
//! A "merge-base" can be described as the common ancestor of two commits.
//! Merge-bases are calculated to determine
//!
//!  1) Whether a commit is a branch off of the main branch.
//!  2) How to order two commits topologically.
//!
//! In a large repository, merge-base queries can be quite expensive when
//! comparing commits which are far away from each other. This can happen, for
//! example, whenever you do a `git pull` to update the main branch, but you
//! haven't yet updated any of your lines of work. Your lines of work are now far
//! away from the current main branch commit, so the merge-base calculation may
//! take a while. It can also happen when simply checking out an old commit to
//! examine it.
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyTuple;

use crate::python::map_err_to_py_err;

struct MergeBaseDb {
    conn: rusqlite::Connection,
}

fn init_tables(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute(
        "
CREATE TABLE IF NOT EXISTS merge_base_oids (
    lhs_oid TEXT NOT NULL,
    rhs_oid TEXT NOT NULL,
    merge_base_oid TEXT,
    UNIQUE (lhs_oid, rhs_oid)
)
",
        rusqlite::params![],
    )?;
    Ok(())
}

fn wrap_git_error(error: git2::Error) -> anyhow::Error {
    anyhow::anyhow!("Git error {:?}: {}", error.code(), error.message())
}

impl MergeBaseDb {
    fn new(conn: rusqlite::Connection) -> anyhow::Result<Self> {
        init_tables(&conn)?;
        Ok(MergeBaseDb { conn })
    }

    /// Get the merge-base for two given commits.
    ///
    /// If the query is already in the cache, return the cached result. If
    /// not, it is computed, cached, and returned.
    ///
    /// Args:
    /// * `repo`: The Git repo.
    /// * `lhs_oid`: The first OID (ordering is arbitrary).
    /// * `rhs_oid`: The second OID (ordering is arbitrary).
    ///
    /// Returns: The merge-base OID for these two commits. Returns `None` if no
    /// merge-base could be found.
    fn get_merge_base_oid(
        &self,
        repo: git2::Repository,
        lhs_oid: git2::Oid,
        rhs_oid: git2::Oid,
    ) -> anyhow::Result<Option<git2::Oid>> {
        match repo.merge_base(lhs_oid, rhs_oid) {
            Ok(merge_base_oid) => Ok(Some(merge_base_oid)),
            Err(err) => {
                if err.code() == git2::ErrorCode::NotFound {
                    Ok(None)
                } else {
                    Err(wrap_git_error(err))
                }
            }
        }
    }
}

#[pyclass]
pub struct PyMergeBaseDb {
    merge_base_db: MergeBaseDb,
}

#[pymethods]
impl PyMergeBaseDb {
    #[new]
    fn new(py: Python, conn: PyObject) -> PyResult<Self> {
        // https://stackoverflow.com/a/14505973
        let query_result =
            conn.call_method1(py, "execute", PyTuple::new(py, &["PRAGMA database_list;"]))?;
        let rows: Vec<(i64, String, String)> =
            query_result.call_method0(py, "fetchall")?.extract(py)?;
        let db_path = match rows.as_slice() {
            [(_, _, path)] => path,
            _ => {
                return Err(PyRuntimeError::new_err(
                    "Could not process response from query: PRAGMA database_list",
                ))
            }
        };

        let conn = rusqlite::Connection::open(db_path);
        let conn = map_err_to_py_err(conn, "Could not open SQLite database")?;

        let merge_base_db = MergeBaseDb::new(conn);
        let merge_base_db =
            map_err_to_py_err(merge_base_db, "Could not construct merge-base database")?;

        let merge_base_db = PyMergeBaseDb { merge_base_db };
        Ok(merge_base_db)
    }

    fn get_merge_base_oid(
        &self,
        py: Python,
        repo: PyObject,
        lhs_oid: PyObject,
        rhs_oid: PyObject,
    ) -> PyResult<PyObject> {
        let repo_path: String = repo.getattr(py, "path")?.extract(py)?;
        let py_repo = repo;
        let repo = git2::Repository::open(repo_path);
        let repo = map_err_to_py_err(repo, "Could not open Git repo")?;

        let lhs_oid: String = lhs_oid.getattr(py, "hex")?.extract(py)?;
        let lhs_oid = git2::Oid::from_str(&lhs_oid);
        let lhs_oid = map_err_to_py_err(lhs_oid, "Could not process LHS OID")?;

        let rhs_oid: String = rhs_oid.getattr(py, "hex")?.extract(py)?;
        let rhs_oid = git2::Oid::from_str(&rhs_oid);
        let rhs_oid = map_err_to_py_err(rhs_oid, "Could not process RHS OID")?;

        let merge_base_oid = self
            .merge_base_db
            .get_merge_base_oid(repo, lhs_oid, rhs_oid);
        let merge_base_oid = map_err_to_py_err(merge_base_oid, "Could not get merge base OID")?;
        match merge_base_oid {
            Some(merge_base_oid) => {
                let args = PyTuple::new(py, &[merge_base_oid.to_string()]);
                let merge_base_commit = py_repo.call_method1(py, "__getitem__", args)?;
                let merge_base_oid = merge_base_commit.getattr(py, "oid")?;
                Ok(merge_base_oid)
            }
            None => Ok(Python::None(py)),
        }
    }
}
