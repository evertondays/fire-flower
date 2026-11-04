//! A pasta do banco e o disco dela: tamanho por tipo de arquivo (só a lista
//! da pasta, sem `du` recursivo) e espaço livre.

use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub(super) struct Disk {
    pub(super) sst_bytes: u64,
    pub(super) wal_bytes: u64,
    pub(super) other_bytes: u64,
    /// Espaço livre e total do sistema de arquivos da pasta, se o `df` disser.
    pub(super) free_bytes: Option<u64>,
    pub(super) size_bytes: Option<u64>,
}

/// Lê a pasta do banco (o RocksDB não usa subpastas).
pub(super) fn read(db: &Path) -> Option<Disk> {
    let mut disk = Disk::default();
    for entry in std::fs::read_dir(db).ok()?.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let slot = if name.ends_with(".sst") {
            &mut disk.sst_bytes
        } else if name.ends_with(".log") {
            &mut disk.wal_bytes
        } else {
            &mut disk.other_bytes
        };
        *slot += meta.len();
    }
    (disk.free_bytes, disk.size_bytes) = filesystem(db).unzip();
    Some(disk)
}

/// Espaço livre e total pelo `df` (a biblioteca padrão não tem `statvfs`).
fn filesystem(db: &Path) -> Option<(u64, u64)> {
    let out = std::process::Command::new("df").args(["-P", "-B1"]).arg(db).output().ok()?;
    parse_df(&String::from_utf8_lossy(&out.stdout))
}

/// A segunda linha do `df -P -B1`: sistema, total, usado, livre, …
pub(super) fn parse_df(text: &str) -> Option<(u64, u64)> {
    let fields: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
    Some((fields.get(3)?.parse().ok()?, fields.get(1)?.parse().ok()?))
}
