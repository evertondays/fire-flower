//! O processo do motor visto de fora, pelo `/proc/<pid>` (só Linux): CPU,
//! memória, threads, I/O e descritores. Nada aqui fala com o motor.

use std::path::Path;

use serde::Serialize;

/// O que o `/proc` diz do processo. Campos que o sistema não mostrou (outro
/// usuário, outro PID namespace) ficam `None`.
#[derive(Debug, Clone, Default, Serialize)]
pub(super) struct Process {
    pub(super) cpu_secs: f64,
    pub(super) resident_bytes: u64,
    pub(super) virtual_bytes: u64,
    pub(super) threads: u64,
    pub(super) start_unix: Option<f64>,
    pub(super) open_fds: Option<u64>,
    pub(super) max_fds: Option<u64>,
    pub(super) io_read_bytes: Option<u64>,
    pub(super) io_write_bytes: Option<u64>,
}

/// `sysconf(_SC_CLK_TCK)`: 100 em praticamente todo Linux.
const CLOCK_TICKS: f64 = 100.0;

/// Lê o processo `pid` em `proc` (normalmente `/proc`); `None` se ele não
/// existe ou não está visível.
pub(super) fn read(proc: &Path, pid: u32) -> Option<Process> {
    let dir = proc.join(pid.to_string());
    let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
    let mut p = parse_stat(&stat)?;
    let boot = std::fs::read_to_string(proc.join("stat")).ok().and_then(|s| boot_time(&s));
    if let (Some(boot), Some(ticks)) = (boot, stat_start_ticks(&stat)) {
        p.start_unix = Some(boot + ticks as f64 / CLOCK_TICKS);
    }
    if let Ok(status) = std::fs::read_to_string(dir.join("status")) {
        let kb = |key: &str| status_kb(&status, key);
        p.resident_bytes = kb("VmRSS:").unwrap_or(p.resident_bytes);
        p.virtual_bytes = kb("VmSize:").unwrap_or(p.virtual_bytes);
    }
    if let Ok(io) = std::fs::read_to_string(dir.join("io")) {
        p.io_read_bytes = field(&io, "read_bytes:");
        p.io_write_bytes = field(&io, "write_bytes:");
    }
    p.max_fds = std::fs::read_to_string(dir.join("limits")).ok().and_then(|l| max_open_files(&l));
    p.open_fds = std::fs::read_dir(dir.join("fd")).ok().map(|d| d.count() as u64);
    Some(p)
}

/// Os campos de `/proc/<pid>/stat` depois do nome (que pode ter espaços e
/// parênteses: corta no último `)`).
fn stat_fields(stat: &str) -> Option<Vec<&str>> {
    Some(stat[stat.rfind(')')? + 2..].split_whitespace().collect())
}

fn parse_stat(stat: &str) -> Option<Process> {
    let f = stat_fields(stat)?;
    let num = |i: usize| f.get(i)?.parse::<u64>().ok();
    // Depois do nome, o campo n do arquivo é o índice n - 3.
    Some(Process {
        cpu_secs: (num(11)? + num(12)?) as f64 / CLOCK_TICKS,
        threads: num(17)?,
        virtual_bytes: num(20)?,
        resident_bytes: num(21)? * 4096,
        ..Process::default()
    })
}

fn stat_start_ticks(stat: &str) -> Option<u64> {
    stat_fields(stat)?.get(19)?.parse().ok()
}

/// `btime` de `/proc/stat`: quando o sistema ligou (Unix, s).
fn boot_time(stat: &str) -> Option<f64> {
    stat.lines().find_map(|l| l.strip_prefix("btime "))?.trim().parse().ok()
}

fn status_kb(status: &str, key: &str) -> Option<u64> {
    let line = status.lines().find_map(|l| l.strip_prefix(key))?;
    Some(line.split_whitespace().next()?.parse::<u64>().ok()? * 1024)
}

fn field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| l.strip_prefix(key))?.trim().parse().ok()
}

/// O limite "soft" de arquivos abertos em `/proc/<pid>/limits`.
fn max_open_files(limits: &str) -> Option<u64> {
    let line = limits.lines().find(|l| l.starts_with("Max open files"))?;
    line["Max open files".len()..].split_whitespace().next()?.parse().ok()
}
