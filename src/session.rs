//! Session registry: naming, on-disk persistence, socket paths, and liveness.
//!
//! A *session* is identified by its (sanitised) name. Each session has:
//!   * a **socket** in the runtime dir (`$XDG_RUNTIME_DIR/mmux/<name>.sock`) — a
//!     live server listens here while the session is *active*;
//!   * a **record** in the data dir (`$XDG_DATA_HOME/mmux/<name>.session`)
//!     holding the display name and its space names, so a *passive* session can
//!     be reopened later.
//!
//! "active" means a server is currently listening on the socket (verified by
//! actually connecting); everything else is "passive".

use std::fs;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// Restrict a user-supplied name to something safe to use as a file/socket name.
pub fn sanitize(name: &str) -> String {
    let mut out: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    out.truncate(64);
    let trimmed = out.trim_matches('.').to_string();
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed
    }
}

fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("MMUX_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("mmux")))
        .unwrap_or_else(|| {
            // SAFETY: `getuid` is always safe — it reads the calling process's
            // real user ID and cannot fail or cause UB.
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/mmux-{uid}"))
        });
    let _ = fs::create_dir_all(&base);
    base
}

fn data_dir() -> PathBuf {
    let base = std::env::var_os("MMUX_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(|d| PathBuf::from(d).join("mmux")))
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".local/share/mmux")
        });
    let _ = fs::create_dir_all(&base);
    base
}

/// Absolute path of a session's Unix socket.
pub fn socket_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{}.sock", sanitize(name)))
}

fn record_path(name: &str) -> PathBuf {
    data_dir().join(format!("{}.session", sanitize(name)))
}

/// A session as shown in the switcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub spaces: Vec<String>,
    pub active: bool,
}

/// Is a server currently listening for this session? Verified by connecting;
/// a leftover socket file whose server has died is cleaned up and reported dead.
pub fn is_active(name: &str) -> bool {
    let path = socket_path(name);
    if !path.exists() {
        return false;
    }
    match UnixStream::connect(&path) {
        Ok(_) => true,
        Err(_) => {
            // Stale socket from a crashed server — remove it.
            let _ = fs::remove_file(&path);
            false
        }
    }
}

/// Persist a session's space names (called by the server as state changes).
pub fn save(name: &str, spaces: &[String]) {
    let path = record_path(name);
    let mut body = String::new();
    body.push_str(&format!("name {}\n", name));
    for s in spaces {
        body.push_str(&format!("space {}\n", s));
    }
    // Atomic write.
    let tmp = path.with_extension("session.tmp");
    if let Ok(mut f) = fs::File::create(&tmp) {
        if f.write_all(body.as_bytes()).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
}

/// Load a session's saved space names, if any.
pub fn load(name: &str) -> Vec<String> {
    let path = record_path(name);
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| l.strip_prefix("space ").map(str::to_string))
        .collect()
}

/// Every known session (from records) plus any live-but-unsaved ones, sorted
/// with active sessions first, then alphabetically.
pub fn list() -> Vec<SessionInfo> {
    let mut names: Vec<String> = Vec::new();

    if let Ok(entries) = fs::read_dir(data_dir()) {
        for e in entries.flatten() {
            if e.path().extension().and_then(|s| s.to_str()) == Some("session") {
                if let Some(stem) = e.path().file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    // Include live sessions that may not have a record yet.
    if let Ok(entries) = fs::read_dir(runtime_dir()) {
        for e in entries.flatten() {
            if e.path().extension().and_then(|s| s.to_str()) == Some("sock") {
                if let Some(stem) = e.path().file_stem().and_then(|s| s.to_str()) {
                    if !names.iter().any(|n| n == stem) {
                        names.push(stem.to_string());
                    }
                }
            }
        }
    }

    let mut infos: Vec<SessionInfo> = names
        .into_iter()
        .map(|name| {
            let active = is_active(&name);
            let spaces = load(&name);
            SessionInfo { name, spaces, active }
        })
        .collect();
    infos.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| a.name.cmp(&b.name)));
    infos
}

/// Remove a session's record and socket entirely (used when a server exits
/// because its last shell closed).
pub fn purge(name: &str) {
    let _ = fs::remove_file(record_path(name));
    let _ = fs::remove_file(socket_path(name));
}

/// Delete a passive session's record. Refuses to touch an active one.
pub fn delete(name: &str) -> bool {
    if is_active(name) {
        return false;
    }
    let _ = fs::remove_file(record_path(name));
    let _ = fs::remove_file(socket_path(name));
    true
}

/// Rename a passive session's record. Refuses if either side is active or the
/// target already exists.
pub fn rename(old: &str, new: &str) -> bool {
    let new = sanitize(new);
    if is_active(old) || is_active(&new) || record_path(&new).exists() {
        return false;
    }
    let spaces = load(old);
    save(&new, &spaces);
    let _ = fs::remove_file(record_path(old));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // `MMUX_DATA_DIR` is process-global, so these tests must not run in
    // parallel. Env holds a lock for the test's duration.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct Env {
        dir: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }
    impl Env {
        fn new() -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("mmux-sess-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            std::env::set_var("MMUX_DATA_DIR", dir.join("data"));
            std::env::set_var("MMUX_RUNTIME_DIR", dir.join("run"));
            Env { dir, _guard: guard }
        }
    }
    impl Drop for Env {
        fn drop(&mut self) {
            std::env::remove_var("MMUX_DATA_DIR");
            std::env::remove_var("MMUX_RUNTIME_DIR");
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn sanitize_makes_safe_names() {
        assert_eq!(sanitize(" my/session! "), "my_session_");
        assert_eq!(sanitize(""), "default");
        assert_eq!(sanitize("../etc"), "_etc"); // leading dots stripped
        assert_eq!(sanitize("work"), "work");
    }

    #[test]
    fn save_load_roundtrip() {
        let _env = Env::new();
        save("work", &["main".into(), "api".into()]);
        assert_eq!(load("work"), vec!["main".to_string(), "api".into()]);
        assert!(!is_active("work")); // no server listening
    }

    #[test]
    fn list_and_delete() {
        let _env = Env::new();
        save("alpha", &["main".into()]);
        save("beta", &["main".into(), "two".into()]);
        let names: Vec<String> = list().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"alpha".to_string()));
        assert!(names.contains(&"beta".to_string()));

        assert!(delete("alpha"));
        let names: Vec<String> = list().into_iter().map(|s| s.name).collect();
        assert!(!names.contains(&"alpha".to_string()));
    }

    #[test]
    fn rename_passive_session() {
        let _env = Env::new();
        save("old", &["main".into()]);
        assert!(rename("old", "new"));
        assert_eq!(load("new"), vec!["main".to_string()]);
        assert!(load("old").is_empty());
    }
}
