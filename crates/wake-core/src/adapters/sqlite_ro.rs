use rusqlite::{Connection, OpenFlags};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// 只读打开的别家 SQLite 连接;若走了 copy 降级,临时目录随 drop 清理。
pub struct SqliteRo {
    pub conn: Connection,
    _tmp: Option<TempDirGuard>,
}

struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 只读读取别家 SQLite,与 codex.rs 同一约定:readonly 直开(探测查询验证可用)
/// → copy 三件套到临时目录 → 放弃。绝不写、绝不 immutable=1(WAL 并发写下不安全)。
pub fn open_sqlite_ro(db: &Path, tag: &str) -> Option<SqliteRo> {
    if !db.is_file() {
        return None;
    }
    if let Ok(conn) = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        let probe: rusqlite::Result<i64> =
            conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0));
        if probe.is_ok() {
            return Some(SqliteRo { conn, _tmp: None });
        }
    }
    // 目录名带库路径的哈希:同一 tag 的两个实例(Hermes 多档案、Codex 默认 + 自定义根、
    // 多 host 镜像)并发走到这里时不能共用一个目录,否则互相覆盖 db.sqlite、先退出的
    // 一方 remove_dir_all 把另一方的连接从脚下抽走(2026-09-22 review)
    let path_hash = {
        use std::hash::{Hash as _, Hasher as _};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        db.hash(&mut h);
        h.finish()
    };
    let tmp = std::env::temp_dir().join(format!(
        "wake-{tag}-{}-{path_hash:016x}",
        std::process::id()
    ));
    fs::create_dir_all(&tmp).ok()?;
    let guard = TempDirGuard(tmp.clone());
    let db_copy = tmp.join("db.sqlite");
    fs::copy(db, &db_copy).ok()?;
    for suffix in ["-wal", "-shm"] {
        let src = PathBuf::from(format!("{}{suffix}", db.display()));
        if src.is_file() {
            let _ = fs::copy(&src, tmp.join(format!("db.sqlite{suffix}")));
        }
    }
    let conn = Connection::open_with_flags(&db_copy, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    Some(SqliteRo {
        conn,
        _tmp: Some(guard),
    })
}

/// 某张表现有的列名;表不存在或读不出给空集。逐版 ALTER 的别家库靠它做列级
/// 降级——缺列给默认值,老库不得整家消失(ZCode 按库 mtime 缓存探测结果;
/// Hermes 的 `HermesSchema` 是同一件事的 OnceLock 版)
pub fn table_columns(conn: &Connection, table: &str) -> HashSet<String> {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return HashSet::new();
    };
    stmt.query_map([], |r| r.get::<_, String>(1))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// SQLite 型数据源没有每会话独立文件,用 `<db路径>#<会话id>` 作虚拟 file_path。
/// 该路径磁盘上不存在:trash 会自动跳过(仅 tombstone),watcher 不追踪。
pub fn virtual_path(db: &Path, id: &str) -> String {
    format!("{}#{id}", db.display())
}

/// virtual_path 的逆:磁盘上不存在的路径剥掉 `#<id>` 还原库文件本体,
/// 真实存在的路径原样返回(文件管理器 reveal 用)。格式知识只在本文件。
pub fn strip_virtual_path(path: &str) -> &str {
    if Path::new(path).exists() {
        path
    } else {
        path.rsplit_once('#').map(|(db, _)| db).unwrap_or(path)
    }
}

/// 行缓存的失效戳:主库与 `-wal` 的 mtime 取新者。WAL 库的新写入往往只落
/// `-wal`(写端尚未 checkpoint;远程 rsync 镜像更是永远没人 checkpoint 主库),
/// 只看主库 mtime 会让缓存抱着旧快照不放、新会话直到重启才出现。
pub fn db_cache_stamp(db: &Path) -> i64 {
    let stamp = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| super::parse_utils::mtime_ms(&m))
            .unwrap_or(0)
    };
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    stamp(db).max(stamp(Path::new(&wal)))
}
