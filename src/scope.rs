//! Project scoping: resolve the repository containing the current working
//! directory so commands can default to "sessions for this project" across
//! all harnesses.
//!
//! Scope semantics (see `ProjectScope::matches_workspace`):
//! - A conversation is in scope when its recorded workspace path equals a
//!   scope root or lives underneath one (path-component prefix match).
//! - Conversations with no recorded workspace stay visible in scoped views;
//!   hiding them would silently drop whole connectors that never record cwd.
//! - Worktrees of one repository count as one project: resolution walks git
//!   worktree metadata (and jj workspace pointers) to collect sibling roots.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// A resolved project scope: the repo root for the current directory plus any
/// sibling worktree roots of the same repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectScope {
    /// The repo root that contains the current working directory.
    pub primary: PathBuf,
    /// All root paths whose subtrees count as this project (includes
    /// `primary`). Stored as strings without trailing slashes for matching
    /// against recorded workspace paths.
    pub roots: Vec<String>,
}

impl ProjectScope {
    /// Human-readable one-line description, e.g. `~/repos/sess (+2 worktrees)`.
    pub fn describe(&self) -> String {
        let path = display_path(&self.primary);
        let extra = self.roots.len().saturating_sub(1);
        if extra > 0 {
            format!("{} (+{} linked root{})", path, extra, plural(extra))
        } else {
            path
        }
    }

    /// Short label for constrained UI (the repo directory name).
    pub fn short_label(&self) -> String {
        self.primary
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| display_path(&self.primary))
    }

    /// Whether a recorded workspace path belongs to this project.
    ///
    /// `None` and empty workspaces match by design: sources that never record
    /// a cwd must not vanish from scoped views.
    pub fn matches_workspace(&self, workspace: Option<&str>) -> bool {
        let Some(ws) = workspace else { return true };
        let ws = ws.trim_end_matches('/');
        if ws.is_empty() {
            return true;
        }
        self.roots.iter().any(|root| path_is_within(ws, root))
    }
}

/// `true` when `path` equals `root` or is inside `root` (component boundary,
/// so `/a/bc` is not within `/a/b`).
pub fn path_is_within(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return false;
    }
    path == root
        || (path.len() > root.len()
            && path.starts_with(root)
            && path.as_bytes()[root.len()] == b'/')
}

/// Resolve the project scope for `cwd`. Returns `None` when `cwd` is not
/// inside a git or jj repository (callers then stay global).
pub fn resolve(cwd: &Path) -> Option<ProjectScope> {
    let root = find_repo_root(cwd)?;

    let mut roots: BTreeSet<PathBuf> = BTreeSet::new();
    roots.insert(root.clone());

    // Git worktree unification (best-effort; failures leave a single root).
    if let Some(git_common_dir) = resolve_git_common_dir(&root) {
        // Main checkout root: the directory containing the common `.git` dir.
        if git_common_dir
            .file_name()
            .is_some_and(|name| name == ".git")
            && let Some(main_root) = git_common_dir.parent()
        {
            roots.insert(main_root.to_path_buf());
        }
        // Linked worktrees recorded under `<common>/worktrees/*/gitdir`.
        for wt_root in linked_worktree_roots(&git_common_dir) {
            roots.insert(wt_root);
        }
    }

    // jj workspace pointer: a non-colocated secondary workspace stores the
    // path of the main workspace's `.jj/repo` directory in a `repo` file.
    if let Some(main_root) = resolve_jj_main_root(&root) {
        roots.insert(main_root);
    }

    // Match against both the literal and canonical spelling of each root:
    // recorded workspaces may use either. Keep the primary root first for
    // display purposes.
    let mut root_strs: Vec<String> = Vec::new();
    let mut push = |root_strs: &mut Vec<String>, p: &Path| {
        let s = p.to_string_lossy().trim_end_matches('/').to_string();
        if !s.is_empty() && !root_strs.contains(&s) {
            root_strs.push(s);
        }
    };
    push(&mut root_strs, &root);
    if let Ok(canon) = root.canonicalize() {
        push(&mut root_strs, &canon);
    }
    for r in &roots {
        push(&mut root_strs, r);
        if let Ok(canon) = r.canonicalize() {
            push(&mut root_strs, &canon);
        }
    }

    Some(ProjectScope {
        primary: root,
        roots: root_strs,
    })
}

/// Resolve scope from the process working directory, honoring an explicit
/// global request.
pub fn resolve_current(force_global: bool) -> Option<ProjectScope> {
    if force_global {
        return None;
    }
    std::env::current_dir().ok().and_then(|cwd| resolve(&cwd))
}

/// Walk up from `start` to the nearest directory containing `.git` (file or
/// dir) or `.jj`.
fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_absolute() {
        start.to_path_buf()
    } else {
        start.canonicalize().ok()?
    };
    loop {
        if dir.join(".git").exists() || dir.join(".jj").is_dir() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Resolve the shared (common) git dir for a repo root, following `.git` file
/// indirection used by linked worktrees. Returns an absolute path.
fn resolve_git_common_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let gitdir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        // `.git` file: `gitdir: <path>` — for worktrees this points at
        // `<common>/worktrees/<name>`.
        let contents = std::fs::read_to_string(&dot_git).ok()?;
        let raw = contents.strip_prefix("gitdir:")?.trim();
        let mut path = PathBuf::from(raw);
        if path.is_relative() {
            path = root.join(path);
        }
        // Normalize `<common>/worktrees/<name>` -> `<common>`.
        let mut common = path.clone();
        if common.parent().and_then(|p| p.file_name()) == Some("worktrees".as_ref())
            && let Some(grandparent) = common.parent().and_then(|p| p.parent())
        {
            common = grandparent.to_path_buf();
        }
        common
    } else {
        return None;
    };
    // A `commondir` file (worktree gitdirs have one) supersedes the guess.
    let commondir_file = gitdir.join("commondir");
    if let Ok(contents) = std::fs::read_to_string(&commondir_file) {
        let raw = contents.trim();
        let mut path = PathBuf::from(raw);
        if path.is_relative() {
            path = gitdir.join(path);
        }
        if let Ok(canon) = path.canonicalize() {
            return Some(canon);
        }
        return Some(path);
    }
    Some(gitdir)
}

/// Enumerate roots of linked worktrees recorded under the common git dir.
fn linked_worktree_roots(git_common_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let worktrees_dir = git_common_dir.join("worktrees");
    let Ok(entries) = std::fs::read_dir(&worktrees_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let gitdir_file = entry.path().join("gitdir");
        let Ok(contents) = std::fs::read_to_string(&gitdir_file) else {
            continue;
        };
        // Contents: absolute path to `<worktree-root>/.git`.
        let path = PathBuf::from(contents.trim());
        if let Some(wt_root) = path.parent()
            && wt_root.is_dir()
        {
            out.push(wt_root.to_path_buf());
        }
    }
    out
}

/// For a non-colocated jj secondary workspace, `.jj/repo` is a file holding
/// the path of the main workspace's repo directory (`<main>/.jj/repo`).
fn resolve_jj_main_root(root: &Path) -> Option<PathBuf> {
    let repo_pointer = root.join(".jj").join("repo");
    if !repo_pointer.is_file() {
        return None;
    }
    let contents = std::fs::read_to_string(&repo_pointer).ok()?;
    let repo_dir = PathBuf::from(contents.trim());
    // `<main>/.jj/repo` -> `<main>`
    let jj_dir = repo_dir.parent()?;
    if jj_dir.file_name()? != ".jj" {
        return None;
    }
    let main_root = jj_dir.parent()?;
    main_root.is_dir().then(|| main_root.to_path_buf())
}

fn display_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy();
        if let Some(rest) = raw.strip_prefix(home.as_ref()) {
            if rest.is_empty() {
                return "~".to_string();
            }
            if rest.starts_with('/') {
                return format!("~{rest}");
            }
        }
    }
    raw.into_owned()
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope_with_roots(roots: &[&str]) -> ProjectScope {
        ProjectScope {
            primary: PathBuf::from(roots[0]),
            roots: roots.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn path_within_requires_component_boundary() {
        assert!(path_is_within("/a/b", "/a/b"));
        assert!(path_is_within("/a/b/c", "/a/b"));
        assert!(!path_is_within("/a/bc", "/a/b"));
        assert!(!path_is_within("/a", "/a/b"));
        assert!(!path_is_within("/a/b", ""));
    }

    #[test]
    fn missing_workspace_matches_scope() {
        let scope = scope_with_roots(&["/repo"]);
        assert!(scope.matches_workspace(None));
        assert!(scope.matches_workspace(Some("")));
        assert!(scope.matches_workspace(Some("/repo")));
        assert!(scope.matches_workspace(Some("/repo/sub/dir")));
        assert!(scope.matches_workspace(Some("/repo/")));
        assert!(!scope.matches_workspace(Some("/other")));
        assert!(!scope.matches_workspace(Some("/repository")));
    }

    #[test]
    fn resolve_finds_git_dir_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();

        let scope = resolve(&nested).expect("scope");
        assert_eq!(scope.primary, root);
        assert!(
            scope
                .roots
                .iter()
                .any(|r| r == &root.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn resolve_finds_jj_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        let nested = root.join("crates").join("core");
        std::fs::create_dir_all(&nested).unwrap();

        let scope = resolve(&nested).expect("scope");
        assert_eq!(scope.primary, root);
    }

    #[test]
    fn resolve_outside_repo_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        // The walk legitimately continues into ancestors of the tempdir, and
        // the host's temp root may itself sit inside a repository (some
        // machines carry a `/tmp/.git`). Only assert that no marker-free
        // directory we created is treated as a repo root.
        if let Some(scope) = resolve(&plain) {
            assert_ne!(scope.primary, plain);
            assert_ne!(scope.primary, tmp.path());
        }
    }

    #[test]
    fn git_worktrees_unify_to_one_project() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let wt = tmp.path().join("wt1");

        // Main repo with a registered linked worktree.
        let common = main.join(".git");
        let wt_meta = common.join("worktrees").join("wt1");
        std::fs::create_dir_all(&wt_meta).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt_meta.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .unwrap();

        // Linked worktree's `.git` file points back into the main repo.
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_meta.display())).unwrap();
        std::fs::write(wt_meta.join("commondir"), "../..\n").unwrap();

        // Resolving from the worktree finds both roots.
        let scope = resolve(&wt).expect("scope");
        assert_eq!(scope.primary, wt);
        let main_str = main.canonicalize().unwrap();
        assert!(
            scope
                .roots
                .iter()
                .any(|r| Path::new(r) == main_str || Path::new(r) == main),
            "worktree scope should include main root; got {:?}",
            scope.roots
        );

        // Resolving from the main repo finds the worktree too.
        let scope = resolve(&main).expect("scope");
        assert_eq!(scope.primary, main);
        assert!(
            scope.roots.iter().any(|r| Path::new(r) == wt),
            "main scope should include linked worktree; got {:?}",
            scope.roots
        );
    }

    #[test]
    fn jj_secondary_workspace_links_main_root() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(main.join(".jj").join("repo")).unwrap();
        std::fs::create_dir_all(second.join(".jj")).unwrap();
        std::fs::write(
            second.join(".jj").join("repo"),
            format!("{}", main.join(".jj").join("repo").display()),
        )
        .unwrap();

        let scope = resolve(&second).expect("scope");
        assert_eq!(scope.primary, second);
        assert!(
            scope.roots.iter().any(|r| Path::new(r) == main),
            "secondary jj workspace should include main root; got {:?}",
            scope.roots
        );
    }
}
