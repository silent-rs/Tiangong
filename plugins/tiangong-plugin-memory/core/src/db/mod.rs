//! 数据库模块（SQLite 明文元数据库与历史加密库兼容）

pub(crate) mod migration;
mod schema;
pub(crate) mod sqlite;

pub(crate) use sqlite::MemoryDb;
