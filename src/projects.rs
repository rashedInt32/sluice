//! The folders the project picker offers, `sluice projects`: one
//! `label<TAB>path` line each, the label the path under home.
//!
//! What to list comes from the config (`projects`, `project-roots`,
//! `project-skip`). With none of them set, every folder under home is
//! listed, a few levels deep, so a fresh install finds projects wherever
//! they live. Worktrees made by the worktree picker, under `~/.worktrees`,
//! are listed either way.
//!
//! Lines are written as they are found: fzf shows them as they arrive, so a
//! long walk never holds the picker up.

use crate::config::Projects;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// How deep the default walk goes below home: `~/a/b/c`.
const DEFAULT_DEPTH: usize = 3;

/// Folders the default walk never lists or enters: build output, package
/// caches and the like, which would bury the projects.
const DEFAULT_SKIP_NAMES: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    "__pycache__",
    "venv",
    "Library",
    "Applications",
];

/// Write the list to `out`. `home` is where `~` points.
pub fn list(cfg: &Projects, home: &Path, out: &mut impl Write) -> std::io::Result<()> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let skip: HashSet<PathBuf> = cfg.skip.iter().map(|s| expand(s, home)).collect();
    let mut emit = |p: PathBuf, out: &mut dyn Write| -> std::io::Result<()> {
        if skip.contains(&p) || !seen.insert(p.clone()) {
            return Ok(());
        }
        writeln!(out, "{}\t{}", label(&p, home), p.display())
    };
    if cfg.dirs.is_empty() && cfg.roots.is_empty() {
        walk(home, 1, &mut |p, out| emit(p, out), out)?;
    } else {
        for d in &cfg.dirs {
            let p = expand(d, home);
            if p.is_dir() {
                emit(p, out)?;
            }
        }
        for r in &cfg.roots {
            for p in subdirs(&expand(r, home)) {
                emit(p, out)?;
            }
        }
    }
    for p in worktrees(&home.join(".worktrees")) {
        emit(p, out)?;
    }
    Ok(())
}

/// `~` and `~/x` from home; anything else as written.
fn expand(s: &str, home: &Path) -> PathBuf {
    match s {
        "~" => home.to_path_buf(),
        _ => match s.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(s),
        },
    }
}

/// The path under home, `Documents/codes/x`, or the whole path elsewhere.
/// What fzf shows and searches.
fn label(p: &Path, home: &Path) -> String {
    let shown = p.strip_prefix(home).unwrap_or(p).display().to_string();
    // a name with a tab or line end would break the line it is on
    shown.replace(['\t', '\n', '\r'], " ")
}

/// The folders directly inside `dir`, by name, hidden ones too: listed
/// because the config named `dir`. Symlinks are not followed, as `find
/// -type d` does not.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// The default: every folder under `dir` to `DEFAULT_DEPTH`, parents first,
/// skipping hidden folders and `DEFAULT_SKIP_NAMES` and all below them.
fn walk(
    dir: &Path,
    depth: usize,
    emit: &mut impl FnMut(PathBuf, &mut dyn Write) -> std::io::Result<()>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    if depth > DEFAULT_DEPTH {
        return Ok(());
    }
    for p in subdirs(dir) {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') || DEFAULT_SKIP_NAMES.contains(&name) {
            continue;
        }
        emit(p.clone(), out)?;
        walk(&p, depth + 1, emit, out)?;
    }
    Ok(())
}

/// Worktrees under `base`, laid out `<repo>/<branch>` where a branch may hold
/// slashes, so matched on the `.git` marker rather than a fixed depth: any
/// folder one to four levels down with a `.git` in it, outside
/// `node_modules`.
fn worktrees(base: &Path) -> Vec<PathBuf> {
    fn find(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 4 {
            return;
        }
        for p in subdirs(dir) {
            if p.file_name().is_some_and(|n| n == "node_modules" || n == ".git") {
                continue;
            }
            if p.join(".git").exists() {
                out.push(p.clone());
            }
            find(&p, depth + 1, out);
        }
    }
    let mut v = Vec::new();
    find(base, 1, &mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(PathBuf);
    impl Tree {
        fn new(tag: &str, dirs: &[&str]) -> Tree {
            let root = std::env::temp_dir().join(format!("sluice-proj-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            for d in dirs {
                std::fs::create_dir_all(root.join(d)).unwrap();
            }
            Tree(root)
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn run(cfg: &Projects, home: &Path) -> Vec<String> {
        let mut out = Vec::new();
        list(cfg, home, &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| l.split('\t').next().unwrap().to_string())
            .collect()
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn with_nothing_configured_every_folder_is_listed_a_few_levels_deep() {
        let t = Tree::new(
            "default",
            &[
                "code/app/src/deep",
                "code/app/node_modules/x",
                ".hidden/x",
                "Library/Caches",
                "notes",
            ],
        );
        let got = run(&Projects::default(), &t.0);
        assert_eq!(got, ["code", "code/app", "code/app/src", "notes"]);
    }

    #[test]
    fn configured_lists_name_folders_roots_and_skips() {
        let t = Tree::new(
            "cfg",
            &[".claude", ".config/nvim", ".config/zed", "codes/a", "codes/js/x", "codes/js/y", "other/z"],
        );
        let home = t.0.display().to_string();
        let cfg = Projects {
            dirs: strings(&["~/.claude", "~/missing"]),
            roots: strings(&["~/.config", "~/codes", &format!("{home}/codes/js")]),
            skip: strings(&["~/codes/js"]),
        };
        let got = run(&cfg, &t.0);
        assert_eq!(
            got,
            [".claude", ".config/nvim", ".config/zed", "codes/a", "codes/js/x", "codes/js/y"],
            "absolute and ~ paths both work; a missing folder is left out"
        );
    }

    #[test]
    fn a_folder_named_twice_is_listed_once() {
        let t = Tree::new("dup", &["a/b"]);
        let cfg = Projects {
            dirs: strings(&["~/a/b"]),
            roots: strings(&["~/a", "~/a"]),
            skip: vec![],
        };
        assert_eq!(run(&cfg, &t.0), ["a/b"]);
    }

    #[test]
    fn worktrees_are_listed_either_way() {
        let t = Tree::new(
            "wt",
            &[
                ".worktrees/repo/main/.git",
                ".worktrees/repo/feat/login/.git",
                ".worktrees/repo/feat/login/node_modules/p/.git",
                "proj",
            ],
        );
        let got = run(&Projects::default(), &t.0);
        assert_eq!(got, ["proj", ".worktrees/repo/feat/login", ".worktrees/repo/main"]);
    }

    #[test]
    fn labels_never_carry_a_tab() {
        let home = Path::new("/h");
        assert_eq!(label(Path::new("/h/a\tb"), home), "a b");
        assert_eq!(label(Path::new("/elsewhere/x"), home), "/elsewhere/x");
        assert_eq!(expand("~", home), PathBuf::from("/h"));
        assert_eq!(expand("~/x", home), PathBuf::from("/h/x"));
        assert_eq!(expand("~x", home), PathBuf::from("~x"));
    }
}
