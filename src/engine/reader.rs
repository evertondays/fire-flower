//! Leitor em outro processo ([`Reader`]): um `Engine` só leitura que se
//! reabre quando o banco muda, no máximo a cada `min_interval`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use rocksdb::Cache;

use crate::{Config, Engine, Result};

/// Lê um banco que outro processo grava (PHP-FPM, cluster do Node, um
/// serviço de busca ao lado do indexador).
///
/// Um `Engine` aberto com [`Engine::open_read_only`] vê o banco como estava
/// na abertura. O `Reader` guarda a instância atual e a troca por uma nova
/// quando o banco mudou ([`Reader::refresh`]): as buscas em andamento
/// terminam na instância antiga, que fecha quando a última solta. Todas as
/// instâncias usam o mesmo block cache.
///
/// **Custo de reabrir:** a instância só leitura repete o WAL na memória,
/// ~12 ms por MB. Com um processo gravando sem parar, o WAL chega a ~90 MB
/// (reabrir ~1 s); `Config::max_wal_bytes` no processo que grava limita isso.
///
/// **Como sabe que mudou:** todo commit acrescenta ao WAL do RocksDB, e um
/// flush ou uma compactação trocam o MANIFEST. O `Reader` compara o nome e o
/// tamanho desses arquivos (uma listagem da pasta, sem abrir o banco); o
/// processo que grava não paga nada. Os caches da instância nova nascem
/// vazios: nada de nós ou tamanhos velhos de outro processo.
pub struct Reader {
    path: PathBuf,
    config: Config,
    /// O mesmo block cache em todas as instâncias: a nova já nasce quente.
    cache: Cache,
    min_interval: Duration,
    current: ArcSwap<Engine>,
    /// A última conferência: quando e o que a pasta tinha.
    checked: Mutex<(Instant, Fingerprint)>,
}

/// Nome e tamanho dos arquivos que todo commit, flush ou compactação muda.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Fingerprint(Vec<(OsString, u64)>);

impl Fingerprint {
    fn of(path: &Path) -> Self {
        let mut files: Vec<(OsString, u64)> = std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".log") || name.starts_with("MANIFEST-") || name == "CURRENT"
            })
            .filter_map(|e| Some((e.file_name(), e.metadata().ok()?.len())))
            .collect();
        files.sort_unstable();
        Fingerprint(files)
    }
}

impl Reader {
    /// Abre o banco só para leitura. `min_interval`: o menor tempo entre
    /// duas conferências da pasta ([`Reader::refresh`]).
    pub fn open(path: impl AsRef<Path>, config: Config, min_interval: Duration) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let fingerprint = Fingerprint::of(&path);
        let cache = Cache::new_lru_cache(config.block_cache_bytes);
        let engine = Engine::open_read_only_with_cache(&path, config.clone(), &cache)?;
        Ok(Self {
            path,
            config,
            cache,
            min_interval,
            current: ArcSwap::from_pointee(engine),
            checked: Mutex::new((Instant::now(), fingerprint)),
        })
    }

    /// A instância atual. Barato (sem trava): chame a cada busca.
    #[inline]
    pub fn engine(&self) -> Arc<Engine> {
        self.current.load_full()
    }

    /// Reabre se o banco mudou desde a última conferência e se já passou
    /// `min_interval`; devolve se reabriu. Uma conferência em andamento em
    /// outra thread faz esta voltar na hora, sem esperar.
    pub fn refresh(&self) -> Result<bool> {
        let Ok(mut checked) = self.checked.try_lock() else { return Ok(false) };
        if checked.0.elapsed() < self.min_interval {
            return Ok(false);
        }
        self.check(&mut checked)
    }

    /// Como [`Reader::refresh`], sem esperar `min_interval` (um pedido
    /// explícito de quem sabe que o banco mudou).
    pub fn refresh_now(&self) -> Result<bool> {
        let mut checked = self.checked.lock().unwrap_or_else(|e| e.into_inner());
        self.check(&mut checked)
    }

    fn check(&self, checked: &mut (Instant, Fingerprint)) -> Result<bool> {
        checked.0 = Instant::now();
        let now = Fingerprint::of(&self.path);
        if now == checked.1 {
            return Ok(false);
        }
        // A pasta de antes da abertura: um commit no meio dela aparece na
        // próxima conferência, e não se perde.
        let engine = Engine::open_read_only_with_cache(&self.path, self.config.clone(), &self.cache)?;
        self.current.store(Arc::new(engine));
        checked.1 = now;
        Ok(true)
    }

    /// Reabre agora, mudado ou não.
    pub fn reopen(&self) -> Result<()> {
        let mut checked = self.checked.lock().unwrap_or_else(|e| e.into_inner());
        let fingerprint = Fingerprint::of(&self.path);
        let engine = Engine::open_read_only_with_cache(&self.path, self.config.clone(), &self.cache)?;
        self.current.store(Arc::new(engine));
        *checked = (Instant::now(), fingerprint);
        Ok(())
    }

    /// A pasta do banco.
    pub fn path(&self) -> &Path {
        &self.path
    }
}
