//! The changed-line map: which lines head adds or modifies relative to base.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Repo-relative path in base. None for an added file.
    pub old_path: Option<String>,
    /// Repo-relative path in head. None for a deleted file.
    pub new_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    /// Changed lines: 1-based lines in head that head adds or modifies.
    pub added: BTreeSet<u32>,
    /// 1-based lines in base that head removes or modifies.
    pub removed: BTreeSet<u32>,
    pub hunks: Vec<Hunk>,
}

/// One hunk of a zero-context diff: the base lines it removes and the head lines it adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub removed: Vec<u32>,
    pub added: Vec<u32>,
}

impl FileChange {
    /// The path in head, or in base for a deleted file.
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .expect("a change has a path")
    }

    /// The base lines replaced by the hunk that adds head `line`. Empty when the hunk only
    /// adds lines or no hunk adds `line`.
    pub fn replaced(&self, line: u32) -> &[u32] {
        self.hunks
            .iter()
            .find(|h| h.added.contains(&line))
            .map(|h| h.removed.as_slice())
            .unwrap_or_default()
    }
}

/// Every file that differs between base and head.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    pub files: Vec<FileChange>,
}

impl ChangeSet {
    /// Parses `git diff --unified=0` output.
    pub fn parse(patch: &str) -> ChangeSet {
        let mut files = Vec::new();
        let mut current: Option<FileChange> = None;
        for line in patch.lines() {
            if let Some(rest) = line.strip_prefix("diff --git ") {
                files.extend(current.take());
                let (old, new) = split_diff_header(rest);
                current = Some(FileChange {
                    old_path: old,
                    new_path: new,
                    status: FileStatus::Modified,
                    binary: false,
                    added: BTreeSet::new(),
                    removed: BTreeSet::new(),
                    hunks: Vec::new(),
                });
                continue;
            }
            let Some(change) = current.as_mut() else {
                continue;
            };
            if line.starts_with("new file mode") {
                change.status = FileStatus::Added;
                change.old_path = None;
            } else if line.starts_with("deleted file mode") {
                change.status = FileStatus::Deleted;
                change.new_path = None;
            } else if let Some(path) = line.strip_prefix("rename from ") {
                change.status = FileStatus::Renamed;
                change.old_path = Some(unquote(path));
            } else if let Some(path) = line.strip_prefix("rename to ") {
                change.status = FileStatus::Renamed;
                change.new_path = Some(unquote(path));
            } else if let Some(path) = line.strip_prefix("--- ") {
                if path != "/dev/null" {
                    change.old_path = Some(strip_side(&unquote(path)));
                }
            } else if let Some(path) = line.strip_prefix("+++ ") {
                if path != "/dev/null" {
                    change.new_path = Some(strip_side(&unquote(path)));
                }
            } else if line.starts_with("Binary files ") {
                change.binary = true;
            } else if let Some(hunk) = line.strip_prefix("@@ ") {
                if let Some((old, new)) = parse_hunk(hunk) {
                    change.removed.extend(old.iter().copied());
                    change.added.extend(new.iter().copied());
                    change.hunks.push(Hunk {
                        removed: old,
                        added: new,
                    });
                }
            }
        }
        files.extend(current);
        ChangeSet { files }
    }

    pub fn get(&self, path: &str) -> Option<&FileChange> {
        self.files
            .iter()
            .find(|f| f.new_path.as_deref() == Some(path))
    }

    /// Changed lines per head path, for files that still exist in head.
    pub fn added_lines(&self) -> BTreeMap<&str, &BTreeSet<u32>> {
        self.files
            .iter()
            .filter_map(|f| f.new_path.as_deref().map(|p| (p, &f.added)))
            .collect()
    }
}

/// Splits `a/x b/y` into its two paths. Unreliable when paths contain spaces; the `---`,
/// `+++`, and rename lines that follow override it.
fn split_diff_header(rest: &str) -> (Option<String>, Option<String>) {
    if rest.starts_with('"') {
        if let Some(end) = closing_quote(rest) {
            let old = unquote(&rest[..=end]);
            let new = unquote(rest[end + 1..].trim_start());
            return (Some(strip_side(&old)), Some(strip_side(&new)));
        }
    }
    match rest.split_once(" b/") {
        Some((old, new)) => (
            Some(strip_side(old)),
            Some(strip_side(&unquote(&format!("b/{new}")))),
        ),
        None => (None, None),
    }
}

fn closing_quote(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

fn strip_side(path: &str) -> String {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

/// Undoes git's C-style quoting of unusual paths.
fn unquote(path: &str) -> String {
    let path = path.trim_end_matches('\t');
    let Some(inner) = path.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else {
        return path.to_string();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let raw = inner.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] != b'\\' || i + 1 >= raw.len() {
            bytes.push(raw[i]);
            i += 1;
            continue;
        }
        let next = raw[i + 1];
        let escaped = match next {
            b'n' => Some(b'\n'),
            b't' => Some(b'\t'),
            b'"' => Some(b'"'),
            b'\\' => Some(b'\\'),
            b'a' => Some(7),
            b'b' => Some(8),
            b'f' => Some(12),
            b'r' => Some(b'\r'),
            b'v' => Some(11),
            _ => None,
        };
        if let Some(b) = escaped {
            bytes.push(b);
            i += 2;
        } else if i + 3 < raw.len() && raw[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let octal = std::str::from_utf8(&raw[i + 1..i + 4]).unwrap();
            bytes.push(u8::from_str_radix(octal, 8).unwrap_or(b'?'));
            i += 4;
        } else {
            bytes.push(next);
            i += 2;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Parses `-a,b +c,d @@` into the removed base lines and the added head lines.
fn parse_hunk(hunk: &str) -> Option<(Vec<u32>, Vec<u32>)> {
    let mut parts = hunk.split_whitespace();
    let old = parse_range(parts.next()?.strip_prefix('-')?)?;
    let new = parse_range(parts.next()?.strip_prefix('+')?)?;
    Some((old, new))
}

fn parse_range(range: &str) -> Option<Vec<u32>> {
    let (start, count) = match range.split_once(',') {
        Some((s, c)) => (s.parse::<u32>().ok()?, c.parse::<u32>().ok()?),
        None => (range.parse::<u32>().ok()?, 1),
    };
    Some((start..start + count).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_hunks_to_added_and_removed_lines() {
        let patch = "\
diff --git a/src/app.py b/src/app.py
index 1111111..2222222 100644
--- a/src/app.py
+++ b/src/app.py
@@ -3 +3,2 @@ def f():
-    return 1
+    x = 2
+    return x
@@ -10,0 +12 @@ def g():
+    pass
@@ -20,2 +22,0 @@ def h():
-    a
-    b
";
        let set = ChangeSet::parse(patch);
        assert_eq!(set.files.len(), 1);
        let file = &set.files[0];
        assert_eq!(file.status, FileStatus::Modified);
        assert_eq!(file.path(), "src/app.py");
        assert_eq!(
            file.added.iter().copied().collect::<Vec<_>>(),
            vec![3, 4, 12]
        );
        assert_eq!(
            file.removed.iter().copied().collect::<Vec<_>>(),
            vec![3, 20, 21]
        );
        assert_eq!(file.replaced(4), &[3]);
        assert_eq!(file.replaced(12), &[] as &[u32]);
        assert_eq!(file.replaced(5), &[] as &[u32]);
    }

    #[test]
    fn tracks_added_deleted_renamed_and_binary_files() {
        let patch = "\
diff --git a/new.py b/new.py
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new.py
@@ -0,0 +1,2 @@
+a = 1
+b = 2
diff --git a/gone.py b/gone.py
deleted file mode 100644
index 4444444..0000000
--- a/gone.py
+++ /dev/null
@@ -1 +0,0 @@
-x = 1
diff --git a/old name.ts b/new name.ts
similarity index 90%
rename from old name.ts
rename to new name.ts
index 5555555..6666666 100644
--- a/old name.ts
+++ b/new name.ts
@@ -2 +2 @@
-let a = 1;
+let a = 2;
diff --git a/logo.png b/logo.png
index 7777777..8888888 100644
Binary files a/logo.png and b/logo.png differ
diff --git \"a/caf\\303\\251 \\\"q\\\".py\" \"b/caf\\303\\251 \\\"q\\\".py\"
new file mode 100644
--- /dev/null
+++ \"b/caf\\303\\251 \\\"q\\\".py\"
@@ -0,0 +1 @@
+pass
";
        let set = ChangeSet::parse(patch);
        let statuses: Vec<_> = set
            .files
            .iter()
            .map(|f| (f.path().to_string(), f.status, f.binary))
            .collect();
        assert_eq!(
            statuses,
            vec![
                ("new.py".to_string(), FileStatus::Added, false),
                ("gone.py".to_string(), FileStatus::Deleted, false),
                ("new name.ts".to_string(), FileStatus::Renamed, false),
                ("logo.png".to_string(), FileStatus::Modified, true),
                ("caf\u{e9} \"q\".py".to_string(), FileStatus::Added, false),
            ]
        );
        assert_eq!(set.files[0].added.len(), 2);
        assert_eq!(set.files[1].new_path, None);
        assert_eq!(
            set.files[1].removed.iter().copied().collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(set.files[2].old_path.as_deref(), Some("old name.ts"));
        assert_eq!(set.get("new name.ts").unwrap().added.len(), 1);
        assert!(set.get("gone.py").is_none());
    }
}
