//! `wiki edit`: um plano de edições com seed fixa (substituir, remover,
//! readicionar) aplicado sobre um banco já carregado, e o estado final que o
//! mesmo plano produz, para o oráculo (`wiki oracle --edits`).

use std::path::Path;
use std::time::Instant;

use fire_flower::{CommitStats, Config, DocId, Engine};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use serde_json::json;

use crate::AnyResult;
use crate::corpus::{Corpus, Doc, stem};
use crate::env::{config_json, dir_size, mb, rss, save_result, write_metrics};
use crate::load::CommitTotals;
use crate::rng::Rng;

/// O plano: quantos documentos da fatia são substituídos, removidos e
/// readicionados. Os textos novos são os artigos seguintes à fatia no corpus.
#[derive(clap::Args, Clone, Copy, Debug)]
pub(crate) struct EditPlanArgs {
    /// Seed do plano de edições
    #[arg(long, default_value_t = 7)]
    pub(crate) edit_seed: u64,
    /// Fração dos documentos da fatia que recebem o texto de outro artigo
    #[arg(long, default_value_t = 0.5)]
    pub(crate) update_frac: f64,
    /// Fração dos documentos da fatia que são removidos
    #[arg(long, default_value_t = 0.1)]
    pub(crate) delete_frac: f64,
    /// Fração dos documentos da fatia que são removidos e depois voltam com o texto original
    #[arg(long, default_value_t = 0.01)]
    pub(crate) readd_frac: f64,
}

/// Uma operação do plano.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edit {
    /// O documento passa a ter o texto do artigo `donor` (0 = o primeiro depois da fatia).
    Update { id: DocId, donor: usize },
    Delete(DocId),
    /// Volta com o texto original (sempre depois do `Delete` dele).
    Readd(DocId),
}

fn shuffle<T>(items: &mut [T], rng: &mut Rng) {
    for i in (1..items.len()).rev() {
        items.swap(i, rng.below(i + 1));
    }
}

/// O plano para os documentos `ids` (a fatia carregada), na ordem em que é aplicado.
pub(crate) fn plan(ids: &[DocId], args: &EditPlanArgs) -> Vec<Edit> {
    let mut rng = Rng(args.edit_seed);
    let mut order = ids.to_vec();
    shuffle(&mut order, &mut rng);
    let n = ids.len() as f64;
    let updates = ((n * args.update_frac) as usize).min(order.len());
    let deletes = ((n * args.delete_frac) as usize).min(order.len() - updates);
    let readds = ((n * args.readd_frac) as usize).min(deletes);
    let mut ops: Vec<Edit> = order[..updates].iter().enumerate().map(|(donor, &id)| Edit::Update { id, donor }).collect();
    ops.extend(order[updates..updates + deletes].iter().map(|&id| Edit::Delete(id)));
    shuffle(&mut ops, &mut rng);
    for &id in &order[updates..updates + readds] {
        let at = ops.iter().position(|e| *e == Edit::Delete(id)).expect("o documento foi removido antes");
        let to = rng.range(at + 1, ops.len());
        ops.insert(to, Edit::Readd(id));
    }
    ops
}

/// Os IDs da fatia (os `take` primeiros artigos do corpus).
pub(crate) fn slice_ids(corpus: &Path, take: u64) -> AnyResult<Vec<DocId>> {
    Corpus::open(corpus, 0, Some(take), false)?.map(|d| d.map(|d| d.id)).collect()
}

/// Os textos que o plano usa: os artigos seguintes à fatia (doadores) e os
/// originais dos documentos readicionados.
struct Sources {
    donors: Vec<Doc>,
    originals: HashMap<DocId, Doc>,
}

fn sources(corpus: &Path, take: u64, ops: &[Edit]) -> AnyResult<Sources> {
    let donors_needed = ops.iter().filter(|e| matches!(e, Edit::Update { .. })).count() as u64;
    let readd: HashSet<DocId> = ops.iter().filter_map(|e| if let Edit::Readd(id) = e { Some(*id) } else { None }).collect();
    let mut originals = HashMap::default();
    for doc in Corpus::open(corpus, 0, Some(take), false)? {
        let doc = doc?;
        if readd.contains(&doc.id) {
            originals.insert(doc.id, doc);
        }
    }
    let donors: Vec<Doc> = Corpus::open(corpus, take, Some(donors_needed), false)?.collect::<AnyResult<_>>()?;
    if (donors.len() as u64) < donors_needed {
        return Err(format!("o corpus tem só {} artigos depois da fatia; o plano precisa de {donors_needed}", donors.len()).into());
    }
    Ok(Sources { donors, originals })
}

/// O estado final da fatia depois do plano: os documentos que ficaram, com
/// o texto que têm no fim, na ordem do corpus.
pub(crate) fn final_docs(corpus: &Path, take: u64, args: &EditPlanArgs) -> AnyResult<Vec<Doc>> {
    let ids = slice_ids(corpus, take)?;
    let ops = plan(&ids, args);
    let mut donors: Vec<Option<Doc>> = sources(corpus, take, &ops)?.donors.into_iter().map(Some).collect();
    let mut updated: HashMap<DocId, usize> = HashMap::default();
    let mut gone: HashSet<DocId> = HashSet::default();
    for op in &ops {
        match *op {
            Edit::Update { id, donor } => drop(updated.insert(id, donor)),
            Edit::Delete(id) => drop(gone.insert(id)),
            Edit::Readd(id) => drop(gone.remove(&id)),
        }
    }
    let mut out = Vec::with_capacity(ids.len());
    for doc in Corpus::open(corpus, 0, Some(take), false)? {
        let doc = doc?;
        if gone.contains(&doc.id) {
            continue;
        }
        match updated.get(&doc.id) {
            Some(&j) => {
                let donor = donors[j].take().expect("cada doador é usado uma vez");
                out.push(Doc { line: doc.line, id: doc.id, ..donor });
            }
            None => out.push(doc),
        }
    }
    Ok(out)
}

/// O plano pronto para aplicar: as operações e os textos que elas usam.
pub(crate) struct Prepared {
    pub(crate) ops: Vec<Edit>,
    src: Sources,
}

/// Prepara o plano para a fatia de `take` artigos de `corpus`. `max_ops`
/// fica só com o começo dele (para medir vazão com lotes pequenos).
pub(crate) fn prepare(corpus: &Path, take: u64, args: &EditPlanArgs, max_ops: Option<usize>) -> AnyResult<Prepared> {
    let ids = slice_ids(corpus, take)?;
    let mut ops = plan(&ids, args);
    ops.truncate(max_ops.unwrap_or(usize::MAX));
    let src = sources(corpus, take, &ops)?;
    Ok(Prepared { ops, src })
}

impl Prepared {
    /// Aplica as operações com commit a cada `batch_size`; `on_commit` recebe
    /// as estatísticas de cada commit.
    pub(crate) fn apply(&self, engine: &Engine, batch_size: usize, mut on_commit: impl FnMut(CommitStats)) -> AnyResult {
        let mut batch = None;
        for (i, op) in self.ops.iter().enumerate() {
            let b = batch.get_or_insert_with(|| engine.batch());
            match *op {
                Edit::Update { id, donor } => {
                    let d = &self.src.donors[donor];
                    b.add_titled(id, &d.title, &d.text)?;
                    b.set_label(id, &d.title);
                }
                Edit::Delete(id) => b.delete_document(id)?,
                Edit::Readd(id) => {
                    let d = &self.src.originals[&id];
                    b.add_titled(id, &d.title, &d.text)?;
                    b.set_label(id, &d.title);
                }
            }
            if b.len() >= batch_size || i + 1 == self.ops.len() {
                // Um lote por vez: o próximo só abre depois do commit deste.
                on_commit(batch.take().expect("lote aberto").commit_with_stats()?);
            }
        }
        Ok(())
    }
}

/// Aplica o plano sobre o banco `db` (carregado com a fatia de `take`
/// artigos de `corpus`), com commit a cada `batch` operações, e mede.
#[allow(clippy::too_many_arguments)]
pub(crate) fn edit(
    db: &Path,
    corpus: &Path,
    take: u64,
    batch_size: usize,
    max_ops: Option<usize>,
    args: EditPlanArgs,
    collect: bool,
    label: Option<String>,
) -> AnyResult {
    let engine = Engine::open(db, Config { statistics: true, ..Config::default() })?;
    let prepared = prepare(corpus, take, &args, max_ops)?;
    let ops = &prepared.ops;
    let label = label.unwrap_or_else(|| format!("{}-{take}-b{batch_size}", stem(corpus)));
    let size_before = dir_size(db);

    let started = Instant::now();
    let mut totals = CommitTotals::default();
    let (mut replaced, mut deleted, mut tombstones) = (0usize, 0usize, 0usize);
    let mut commit_secs: Vec<f64> = Vec::new();
    prepared.apply(&engine, batch_size, |stats| {
        (replaced, deleted, tombstones) = (replaced + stats.replaced, deleted + stats.deleted, tombstones + stats.tombstones);
        commit_secs.push(stats.total_secs);
        totals.add(&stats);
    })?;
    let secs = started.elapsed().as_secs_f64();
    let collect_started = Instant::now();
    let emptied = if collect { Some(engine.collect_empty_words()?) } else { None };
    let collect_secs = collect_started.elapsed().as_secs_f64();
    commit_secs.sort_by(f64::total_cmp);
    let pct = |p: f64| commit_secs.get(((commit_secs.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0.0);
    let (_, peak) = rss();
    let result = json!({
        "db": db, "corpus": corpus, "take": take, "batch": batch_size, "plan": format!("{args:?}"),
        "config": config_json(engine.config()), "format": fire_flower::FORMAT_VERSION,
        "ops": ops.len(), "seconds": secs, "ops_per_s": ops.len() as f64 / secs,
        "replaced": replaced, "deleted": deleted, "tombstones": tombstones,
        "commits": commit_secs.len(), "commit_p50_s": pct(0.5), "commit_p99_s": pct(0.99),
        "emptied_words": emptied, "collect_seconds": collect_secs,
        "peak_rss": peak, "db_bytes_before": size_before, "db_bytes": dir_size(db),
        "write": write_metrics(&engine), "commit_steps": totals.json(secs, ops.len() as f64),
    });
    println!(
        "{} operações em {secs:.1}s: {:.0} ops/s ({replaced} substituídos, {deleted} removidos, {tombstones} lápides), commit p50 {:.1} ms, p99 {:.1} ms, banco {:.0} → {:.0} MB",
        ops.len(),
        ops.len() as f64 / secs,
        pct(0.5) * 1e3,
        pct(0.99) * 1e3,
        mb(size_before),
        mb(dir_size(db))
    );
    if let Some(n) = emptied {
        println!("limpeza: {n} palavras sem documento em {collect_secs:.2}s");
    }
    save_result("edit", &label, result)?;
    Ok(())
}
