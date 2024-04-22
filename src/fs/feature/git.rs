//! Getting the Git status of files and directories.

use std::env;
use std::ffi::OsStr;
#[cfg(target_family = "unix")]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use log::*;

use crate::fs::fields as f;

/// A **Git cache** is assembled based on the user’s input arguments.
///
/// This uses vectors to avoid the overhead of hashing: it’s not worth it when the
/// expected number of Git repositories per exa invocation is 0 or 1...
pub struct GitCache {
    /// A list of discovered Git repositories and their paths.
    repos: Vec<GitRepo>,

    /// Paths that we’ve confirmed do not have Git repositories underneath them.
    misses: Vec<PathBuf>,
}

impl GitCache {
    pub fn has_anything_for(&self, index: &Path) -> bool {
        self.repos.iter().any(|e| e.has_path(index))
    }

    pub fn get(&self, index: &Path, prefix_lookup: bool) -> f::Git {
        self.repos
            .iter()
            .find(|repo| repo.has_path(index))
            .map(|repo| repo.get_status(index, prefix_lookup))
            .unwrap_or_default()
    }

    pub fn has_in_submodule(&self, path: &Path) -> bool {
        self.repos
            .iter()
            .find(|repo| repo.has_path(path))
            .map(|repo| repo.has_in_submodule(path))
            .unwrap_or(false)
    }
}

use std::iter::FromIterator;
impl FromIterator<PathBuf> for GitCache {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let iter = iter.into_iter();
        let mut git = Self {
            repos: Vec::with_capacity(iter.size_hint().0),
            misses: Vec::new(),
        };

        if let Ok(path) = env::var("GIT_DIR") {
            // These flags are consistent with how `git` uses GIT_DIR:
            let flags = git2::RepositoryOpenFlags::NO_SEARCH | git2::RepositoryOpenFlags::NO_DOTGIT;
            match GitRepo::discover(path.into(), flags) {
                Ok(repo) => {
                    debug!("Opened GIT_DIR repo");
                    git.repos.push(repo);
                }
                Err(miss) => {
                    git.misses.push(miss);
                }
            }
        }

        for path in iter {
            if git.misses.contains(&path) {
                debug!("Skipping {:?} because it already came back Gitless", path);
            } else if git.repos.iter().any(|e| e.has_path(&path)) {
                debug!("Skipping {:?} because we already queried it", path);
            } else {
                let flags = git2::RepositoryOpenFlags::FROM_ENV;
                match GitRepo::discover(path, flags) {
                    Ok(r) => {
                        if let Some(r2) = git.repos.iter_mut().find(|e| e.has_workdir(&r.workdir)) {
                            debug!(
                                "Adding to existing repo (workdir matches with {:?})",
                                r2.workdir
                            );
                            r2.extra_paths.push(r.original_path);
                            continue;
                        }

                        debug!("Discovered new Git repo");
                        git.repos.push(r);
                    }
                    Err(miss) => {
                        git.misses.push(miss);
                    }
                }
            }
        }

        git
    }
}

/// A **Git repository** is one we’ve discovered somewhere on the filesystem.
pub struct GitRepo {
    /// All the interesting Git stuff goes through this.
    repo: Mutex<git2::Repository>,

    /// Cached path->status mapping.
    statuses: RwLock<Option<GitStatuses>>,

    /// Cached list of the relative paths of all submodules in this repository.
    /// This is used to optionally ignore submodule contents when listing recursively.
    relative_submodule_paths: RwLock<Option<Result<Vec<PathBuf>, git2::Error>>>,

    /// The working directory of this repository.
    /// This is used to check whether two repositories are the same.
    workdir: PathBuf,

    /// The path that was originally checked to discover this repository.
    /// This is as important as the extra_paths (it gets checked first), but
    /// is separate to avoid having to deal with a non-empty Vec.
    original_path: PathBuf,

    /// Any other paths that were checked only to result in this same
    /// repository.
    extra_paths: Vec<PathBuf>,
}

impl GitRepo {
    /// Searches through this repository for a path (to a file or directory,
    /// depending on the prefix-lookup flag) and returns its Git status.
    ///
    /// Actually querying the `git2` repository for the mapping of paths to
    /// Git statuses is only done once, and gets cached so we don’t need to
    /// re-query the entire repository the times after that.
    ///
    /// “Prefix lookup” means that it should report an aggregate status of all
    /// paths starting with the given prefix (in other words, a directory).
    fn get_status(&self, index: &Path, prefix_lookup: bool) -> f::Git {
        {
            let statuses = self.statuses.read().unwrap();
            if let Some(ref cached_statuses) = *statuses {
                debug!("Git repo {:?} has been found in cache", &self.workdir);
                return cached_statuses.status(index, prefix_lookup);
            }
        }

        let mut statuses = self.statuses.write().unwrap();
        if let Some(ref cached_statuses) = *statuses {
            debug!("Git repo {:?} has been found in cache", &self.workdir);
            return cached_statuses.status(index, prefix_lookup);
        }

        debug!("Querying Git repo {:?} for the first time", &self.workdir);
        let repo = self.repo.lock().unwrap();
        let new_statuses = repo_to_statuses(&repo, &self.workdir);
        let result = new_statuses.status(index, prefix_lookup);
        *statuses = Some(new_statuses);
        result
    }

    /// Whether this repository has the given working directory.
    fn has_workdir(&self, path: &Path) -> bool {
        self.workdir == path
    }

    /// Whether this repository cares about the given path at all.
    fn has_path(&self, path: &Path) -> bool {
        path.starts_with(&self.original_path)
            || self.extra_paths.iter().any(|e| path.starts_with(e))
    }

    /// Open a Git repository. Depending on the flags, the path is either
    /// the repository's "gitdir" (or a "gitlink" to the gitdir), or the
    /// path is the start of a rootwards search for the repository.
    fn discover(path: PathBuf, flags: git2::RepositoryOpenFlags) -> Result<Self, PathBuf> {
        info!("Opening Git repository for {:?} ({:?})", path, flags);
        let unused: [&OsStr; 0] = [];
        let repo = match git2::Repository::open_ext(&path, flags, unused) {
            Ok(r) => r,
            Err(e) => {
                error!("Error opening Git repository for {path:?}: {e:?}");
                return Err(path);
            }
        };

        if let Some(workdir) = repo.workdir() {
            let workdir = workdir.to_path_buf();
            Ok(Self {
                repo: Mutex::new(repo),
                statuses: RwLock::new(None),
                relative_submodule_paths: RwLock::new(None),
                workdir,
                original_path: path,
                extra_paths: Vec::new(),
            })
        } else {
            warn!("Repository has no workdir?");
            Err(path)
        }
    }

    fn has_in_submodule(&self, path: &Path) -> bool {
        fn check_submodule_paths(
            paths: &[PathBuf],
            path: &Path,
            extra_paths: &[PathBuf],
            original_path: &Path,
        ) -> bool {
            if let Ok(relative_path) = path.strip_prefix(original_path) {
                if paths.iter().any(|p| relative_path.starts_with(p)) {
                    return true;
                }
            }

            extra_paths.iter().any(|extra_path| {
                if let Ok(relative_path) = path.strip_prefix(extra_path) {
                    paths.iter().any(|p| relative_path.starts_with(p))
                } else {
                    false
                }
            })
        }

        {
            let relative_submodule_paths = self.relative_submodule_paths.read().unwrap();
            match &*relative_submodule_paths {
                Some(Ok(paths)) => {
                    return check_submodule_paths(paths, path, &self.extra_paths, &self.original_path);
                }
                Some(Err(_)) => return false,
                None => {}
            }
        }

        let mut relative_submodule_paths = self.relative_submodule_paths.write().unwrap();
        match &*relative_submodule_paths {
            Some(Ok(paths)) => check_submodule_paths(paths, path, &self.extra_paths, &self.original_path),
            Some(Err(_)) => false,
            None => {
                let repo = self.repo.lock().unwrap();
                let paths_result = repo.submodules().map(|submodules| {
                    submodules
                        .iter()
                        .map(|submodule| submodule.path().to_path_buf())
                        .collect()
                });
                *relative_submodule_paths = Some(paths_result);

                match &*relative_submodule_paths {
                    Some(Ok(paths)) => {
                        check_submodule_paths(paths, path, &self.extra_paths, &self.original_path)
                    }
                    Some(Err(e)) => {
                        error!("Error looking up Git submodules: {:?}", e);
                        false
                    }
                    None => unreachable!(),
                }
            }
        }
    }
}

/// Iterates through a repository’s statuses, consuming it and returning the
/// mapping of files to their Git status.
/// We will have already used the working directory at this point, so it gets
/// passed in rather than deriving it from the `Repository` again.
fn repo_to_statuses(repo: &git2::Repository, workdir: &Path) -> GitStatuses {
    let mut statuses = Vec::new();

    info!("Getting Git statuses for repo with workdir {:?}", workdir);
    match repo.statuses(None) {
        Ok(es) => {
            for e in es.iter() {
                #[cfg(target_family = "unix")]
                let path = workdir.join(Path::new(OsStr::from_bytes(e.path_bytes())));
                // TODO: handle non Unix systems better:
                // https://github.com/ogham/exa/issues/698
                #[cfg(not(target_family = "unix"))]
                let path = workdir.join(Path::new(e.path().unwrap()));
                let elem = (path, e.status());
                statuses.push(elem);
            }
            // We manually add the `.git` at the root of the repo as ignored, since it is in practice.
            // Also we want to avoid `eza --tree --all --git-ignore` to display files inside `.git`.
            statuses.push((workdir.join(".git"), git2::Status::IGNORED));
        }
        Err(e) => {
            error!("Error looking up Git statuses: {:?}", e);
        }
    }

    GitStatuses { statuses }
}

// The `repo.statuses` call above takes a long time. exa debug output:
//
//   20.311276  INFO:exa::fs::feature::git: Getting Git statuses for repo with workdir "/vagrant/"
//   20.799610  DEBUG:exa::output::table: Getting Git status for file "./Cargo.toml"
//
// Even inserting another logging line immediately afterwards doesn’t make it
// look any faster.

/// Container of Git statuses for all the files in this folder’s Git repository.
struct GitStatuses {
    statuses: Vec<(PathBuf, git2::Status)>,
}

impl GitStatuses {
    /// Get either the file or directory status for the given path.
    /// “Prefix lookup” means that it should report an aggregate status of all
    /// paths starting with the given prefix (in other words, a directory).
    fn status(&self, index: &Path, prefix_lookup: bool) -> f::Git {
        if prefix_lookup {
            self.dir_status(index)
        } else {
            self.file_status(index)
        }
    }

    /// Get the user-facing status of a file.
    /// We check the statuses directly applying to a file, and for the ignored
    /// status we check if any of its parents directories is ignored by git.
    fn file_status(&self, file: &Path) -> f::Git {
        let path = reorient(file);

        let s = self
            .statuses
            .iter()
            .filter(|p| {
                if p.1 == git2::Status::IGNORED {
                    path.starts_with(&p.0)
                } else {
                    p.0 == path
                }
            })
            .fold(git2::Status::empty(), |a, b| a | b.1);

        let staged = index_status(s);
        let unstaged = working_tree_status(s);
        f::Git { staged, unstaged }
    }

    /// Get the combined, user-facing status of a directory.
    /// Statuses are aggregating (for example, a directory is considered
    /// modified if any file under it has the status modified), except for
    /// ignored status which applies to files under (for example, a directory
    /// is considered ignored if one of its parent directories is ignored).
    fn dir_status(&self, dir: &Path) -> f::Git {
        let path = reorient(dir);

        let s = self
            .statuses
            .iter()
            .filter(|p| {
                if p.1 == git2::Status::IGNORED {
                    path.starts_with(&p.0)
                } else {
                    p.0.starts_with(&path)
                }
            })
            .fold(git2::Status::empty(), |a, b| a | b.1);

        let staged = index_status(s);
        let unstaged = working_tree_status(s);
        f::Git { staged, unstaged }
    }
}

/// Converts a path to an absolute path based on the current directory.
/// Paths need to be absolute for them to be compared properly, otherwise
/// you’d ask a repo about “./README.md” but it only knows about
/// “/vagrant/README.md”, prefixed by the workdir.
#[cfg(unix)]
fn reorient(path: &Path) -> PathBuf {
    use std::env::current_dir;

    // TODO: I’m not 100% on this func tbh
    let path = match current_dir() {
        Err(_) => Path::new(".").join(path),
        Ok(dir) => dir.join(path),
    };

    path.canonicalize().unwrap_or(path)
}

#[cfg(windows)]
fn reorient(path: &Path) -> PathBuf {
    let unc_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    // On Windows UNC path is returned. We need to strip the prefix for it to work.
    let normal_path = unc_path
        .as_os_str()
        .to_str()
        .unwrap()
        .trim_start_matches("\\\\?\\");
    PathBuf::from(normal_path)
}

/// The character to display if the file has been modified, but not staged.
fn working_tree_status(status: git2::Status) -> f::GitStatus {
    #[rustfmt::skip]
    return match status {
        s if s.contains(git2::Status::WT_NEW)         => f::GitStatus::New,
        s if s.contains(git2::Status::WT_MODIFIED)    => f::GitStatus::Modified,
        s if s.contains(git2::Status::WT_DELETED)     => f::GitStatus::Deleted,
        s if s.contains(git2::Status::WT_RENAMED)     => f::GitStatus::Renamed,
        s if s.contains(git2::Status::WT_TYPECHANGE)  => f::GitStatus::TypeChange,
        s if s.contains(git2::Status::IGNORED)        => f::GitStatus::Ignored,
        s if s.contains(git2::Status::CONFLICTED)     => f::GitStatus::Conflicted,
        _                                             => f::GitStatus::NotModified,
    };
}

/// The character to display if the file has been modified and the change
/// has been staged.
fn index_status(status: git2::Status) -> f::GitStatus {
    #[rustfmt::skip]
    return match status {
        s if s.contains(git2::Status::INDEX_NEW)         => f::GitStatus::New,
        s if s.contains(git2::Status::INDEX_MODIFIED)    => f::GitStatus::Modified,
        s if s.contains(git2::Status::INDEX_DELETED)     => f::GitStatus::Deleted,
        s if s.contains(git2::Status::INDEX_RENAMED)     => f::GitStatus::Renamed,
        s if s.contains(git2::Status::INDEX_TYPECHANGE)  => f::GitStatus::TypeChange,
        _                                                => f::GitStatus::NotModified,
    };
}

fn current_branch(repo: &git2::Repository) -> Option<String> {
    let head = match repo.head() {
        Ok(head) => Some(head),
        Err(ref e)
            if e.code() == git2::ErrorCode::UnbornBranch
                || e.code() == git2::ErrorCode::NotFound =>
        {
            return None
        }
        Err(e) => {
            error!("Error looking up Git branch: {:?}", e);
            return None;
        }
    };

    if let Some(h) = head {
        if let Some(s) = h.shorthand() {
            let branch_name = s.to_owned();
            if branch_name.len() > 10 {
                return Some(branch_name[..8].to_string() + "..");
            }
            return Some(branch_name);
        }
    }
    None
}

impl f::SubdirGitRepo {
    pub fn from_path(dir: &Path, status: bool) -> Self {
        let path = &reorient(dir);

        if let Ok(repo) = git2::Repository::open(path) {
            let branch = current_branch(&repo);
            if !status {
                return Self {
                    status: None,
                    branch,
                };
            }
            match repo.statuses(None) {
                Ok(es) => {
                    if es.iter().any(|s| s.status() != git2::Status::IGNORED) {
                        return Self {
                            status: Some(f::SubdirGitRepoStatus::GitDirty),
                            branch,
                        };
                    }
                    return Self {
                        status: Some(f::SubdirGitRepoStatus::GitClean),
                        branch,
                    };
                }
                Err(e) => {
                    error!("Error looking up Git statuses: {e:?}");
                }
            }
        }
        f::SubdirGitRepo {
            status: if status {
                Some(f::SubdirGitRepoStatus::NoRepo)
            } else {
                None
            },
            branch: None,
        }
    }
}
