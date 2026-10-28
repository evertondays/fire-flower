#!/usr/bin/env python3
"""Roda o plano de performance (docs/plano-performance.md) e gera o relatório.

    python3 scripts/perf.py all              # todas as fases na escala 10k (~5 min)
    python3 scripts/perf.py all --scale 100k
    python3 scripts/perf.py ladder            # Fase 2: carga 1k → … → escala   [--experiments]
    python3 scripts/perf.py correctness         # Fase 1: oráculo, idempotência, lotes vs. único, reabrir
    python3 scripts/perf.py queries         # Fase 3: vocabulário + conjuntos de consulta
    python3 scripts/perf.py latency          # Fase 4: latência por conjunto, merge pendente vs. compactado
    python3 scripts/perf.py concurrency      # Fase 5: escala com threads e leitura durante escrita
    python3 scripts/perf.py pr                # Fase 8: checagem rápida; falha se o p50 piorar >10%
    python3 scripts/perf.py phrases            # frases e ranking: carga com/sem posições, oráculo, latência
    python3 scripts/perf.py relevance        # ordenação: consultas de resposta conhecida (docs/plano-relevancia.md)
    python3 scripts/perf.py long-load       # carga inteira janela a janela; falha se a velocidade colapsar
                                              #   [--reproduce: roda antes o cenário que colapsava]
    python3 scripts/perf.py stemming          # stemming ligado vs. desligado (A/B no mesmo binário)
    python3 scripts/perf.py edits             # edição e remoção (docs/analise-edicao.md, plano A); até 100k
    python3 scripts/perf.py report         # escreve RESULTADOS.md no repositório de resultados
    python3 scripts/perf.py compare A.json B.json

`--scale` (1k, 10k, 100k ou full; padrão 10k) define a maior fatia do corpus
usada. A Wikipédia inteira (full) leva mais de uma hora; comece pequeno.

Cargas de até 10k docs rodam `--repetitions` vezes (padrão 3) e o relatório usa
a mediana: uma carga de 10k varia ±10% entre rodadas, e uma rodada só não
separa uma melhoria pequena de sorte. Fatias maiores rodam uma vez.

Bancos ficam em ~/bench-db/perf/ (FF_PERF_DB para mudar); o corpus em
~/datasets/ptwiki/ (FF_CORPUS_DIR). Resultados, linhas de base e relatório
ficam no repositório separado fire-flower-bench, clonado ao lado deste
(FF_BENCH_DIR para mudar), em results/, com a escala no rótulo, então escalas
diferentes não se misturam.

Regras do plano: build release, nada pesado em paralelo durante as medições,
compare só resultados da mesma máquina. Só usa a biblioteca padrão.
"""
import argparse
import datetime
import glob
import json
import os
import shutil
import statistics
import subprocess
import sys
import time

PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# Resultados ficam no repositório fire-flower-bench, clonado ao lado deste.
BENCH_DIR = os.path.expanduser(os.environ.get("FF_BENCH_DIR", os.path.join(os.path.dirname(PROJECT), "fire-flower-bench")))
RESULTS = os.path.join(BENCH_DIR, "results")
REPORT = os.path.join(BENCH_DIR, "RESULTADOS.md")
# O relatório mora em outro repositório: os links para a documentação são absolutos.
DOCS_URL = "https://github.com/evertondays/fire-flower/blob/main/docs"
WIKI = os.path.join(PROJECT, "target", "release", "examples", "wiki")
FF = os.path.join(PROJECT, "target", "release", "fire-flower")

CORPUS_DIR = os.path.expanduser(os.environ.get("FF_CORPUS_DIR", "~/datasets/ptwiki"))
DB_ROOT = os.path.expanduser(os.environ.get("FF_PERF_DB", "~/bench-db/perf"))
SLICES = {
    "1k": ("ptwiki-1000.jsonl", 1000),
    "10k": ("ptwiki-10000.jsonl", 10000),
    "100k": ("ptwiki-100000.jsonl", 100000),
    "full": ("ptwiki.jsonl", 1112246),
}
ORDER = list(SLICES)

# Cargas de até este tamanho são repetidas; as maiores demoram demais.
REPEAT_UP_TO_DOCS = 10_000
REPETITIONS = 3  # --repetitions
# Quantas rodadas recentes (do mesmo commit e rótulo) entram na mediana.
MEDIAN_OF = 5

# Artigos por commit nas cargas. Era 1000 até 26/10/2026; com 10 mil a carga
# inteira cai de 242 s para ~115 s (docs/plano-performance.md, P2).
BATCH = 10000

# Conjuntos do `pr` (comparados com a linha de base) e das frases/ranking.
PR_SETS = ["t-mid", "t-freq", "p-2", "rank-freq", "phrase-2"]
PHRASE_SETS = ["t-freq", "rank-freq", "and-2", "rank-and", "titles", "rank-titles",
              "phrase-2", "phrase-3", "phrase-stop", "phrase-freq", "phrase-miss"]

ORANGE, BOLD, DIM, RED, GREEN, RESET = "\x1b[38;5;208m", "\x1b[1m", "\x1b[2m", "\x1b[31m", "\x1b[32m", "\x1b[0m"


# ------------------------------------------------------------------ utilidades

def corpus(slice_name):
    return os.path.join(CORPUS_DIR, SLICES[slice_name][0])


def docs_in(slice_name):
    return SLICES[slice_name][1]


def smaller(a, b):
    """A menor de duas fatias."""
    return a if ORDER.index(a) <= ORDER.index(b) else b


def db(name):
    return os.path.join(DB_ROOT, name)


def queries_dir(scale):
    return os.path.join(PROJECT, "bench", "queries", scale)


def step(title):
    print(f"\n{ORANGE}{BOLD}▶ {title}{RESET}", flush=True)


def run(cmd, capture=False, check=True):
    print(f"{DIM}$ {' '.join(map(str, cmd))}{RESET}", flush=True)
    if capture:
        return subprocess.run(cmd, stdout=subprocess.PIPE, text=True, check=check).stdout
    return subprocess.run(cmd, check=check).returncode


def wiki(*args, capture=False, check=True):
    return run([WIKI, *map(str, args)], capture=capture, check=check)


def build():
    step("compilando (release)")
    # FF_CARGO_ARGS="--no-default-features": a variante sem a feature `metrics`.
    extra = os.environ.get("FF_CARGO_ARGS", "").split()
    run(["cargo", "build", "--release", *extra, "--bin", "fire-flower", "--example", "wiki"])


def fresh(path):
    if os.path.exists(path):
        shutil.rmtree(path)
    os.makedirs(os.path.dirname(path), exist_ok=True)


def copy_db(src, dst):
    fresh(dst)
    shutil.copytree(src, dst)


def dir_size(path):
    return sum(e.stat().st_size for e in os.scandir(path) if e.is_file()) if os.path.isdir(path) else 0


def digest(path):
    return json.loads(wiki("digest", "--db", path, capture=True))


def reps(slice_name):
    return REPETITIONS if docs_in(slice_name) <= REPEAT_UP_TO_DOCS else 1


def load_rounds(path, slice_name, label, *extra):
    """Carrega a fatia `reps` vezes, cada uma num banco novo, com o mesmo rótulo.
    O último banco fica em `path`."""
    n = reps(slice_name)
    for i in range(n):
        if n > 1:
            print(f"{DIM}rodada {i + 1} de {n}{RESET}")
        fresh(path)
        wiki("load", "--db", path, "--corpus", corpus(slice_name), *extra, "--label", label)


def usable(path):
    """O banco existe e este binário consegue abri-lo (um banco de um formato
    antigo é recusado sem ser alterado)."""
    if not os.path.exists(path):
        return False
    r = subprocess.run([FF, "search", path, "xyzzyinexistente"], capture_output=True, text=True)
    if r.returncode != 0:
        print(f"{DIM}{path}: {r.stderr.strip()}; recarregando{RESET}")
    return r.returncode == 0


def ensure_loaded(slice_name):
    """Banco da fatia; carrega se não existir ou se for de um formato antigo."""
    path = db(f"ptwiki-{slice_name}")
    if not usable(path):
        step(f"carga {slice_name} (banco não existia ou era de outro formato)")
        fresh(path)
        wiki("load", "--db", path, "--corpus", corpus(slice_name), "--batch", BATCH, "--label", f"ptwiki-{slice_name}")
    return path


def git_commit():
    def git(*a):
        return subprocess.run(["git", "-C", PROJECT, *a], capture_output=True, text=True).stdout.strip()
    h = git("rev-parse", "--short", "HEAD") or "unknown"
    # bench/ fica de fora: o próprio perf.py regrava consultas, baseline e relatório no meio da rodada.
    return h + ("-modificado" if git("status", "--porcelain", "--untracked-files=no", "--", ".", ":!bench") else "")


def save_result(kind, label, data):
    os.makedirs(RESULTS, exist_ok=True)
    ts = datetime.datetime.utcnow().strftime("%Y-%m-%dT%H-%M-%S")
    commit = git_commit()
    data = dict(data, kind=kind, label=label, commit=commit, when=ts)
    path = os.path.join(RESULTS, f"{ts}-{commit}-{kind}-{label}.json")
    with open(path, "w") as f:
        json.dump(data, f, indent=2, ensure_ascii=False)
    print(f"{DIM}resultado: {path}{RESET}")
    return path


# Tipos de resultado que o próprio perf.py grava (`save_result`).
PERF_KINDS = {"compact", "correctness", "edits-digest", "long-load"}


def load_results():
    out = []
    for path in sorted(glob.glob(os.path.join(RESULTS, "*.json"))):
        try:
            with open(path) as f:
                r = json.load(f)
            # Até 27/10/2026 o perf.py gravava estas chaves em português (o
            # `wiki` já gravava em inglês): sem isso, compactações e afins
            # gravados pelo próprio perf.py sumiam do relatório. Só os tipos
            # que o perf.py grava e com os dados já em inglês: os resultados
            # de antes da tradução têm tudo em português e ficam de fora.
            english = not ({"segundos", "banco_antes"} & r.keys())
            if r.get("tipo") in PERF_KINDS and "kind" not in r and english:
                for old, new in (("tipo", "kind"), ("rotulo", "label"), ("quando", "when")):
                    r[new] = r.pop(old, None)
            r["_file"] = os.path.basename(path)
            out.append(r)
        except (OSError, ValueError):
            pass
    return out


def latest(results, kind, label=None):
    """Resultado mais recente de um tipo (e rótulo). Os arquivos começam pela data."""
    found = [r for r in results if r.get("kind") == kind and (label is None or r.get("label") == label)]
    return found[-1] if found else None


def db_format(r):
    """Formato do banco de um resultado de carga: 1 antes de tf e posições.
    Até 26/10/2026 o `wiki load` gravava 2 para qualquer formato com tf."""
    return r.get("format", 1)


def median_load(results, label, fmt=None):
    """Carga mediana (por docs/s) entre as últimas MEDIANA_DE rodadas com o
    rótulo, todas do mesmo commit da mais recente. `fmt` restringe ao formato
    do banco: 1 (sem tf nem posições) ou 2 (qualquer formato com tf, 2 em
    diante). Ganha `_rounds`, `_min` e `_max`."""
    results = [r for r in results if fmt is None or r.get("kind") != "load" or min(db_format(r), 2) == fmt]
    last = latest(results, "load", label)
    if not last:
        return None
    runs = [r for r in results if r.get("kind") == "load" and r.get("label") == label
            and r.get("commit") == last.get("commit")][-MEDIAN_OF:]
    runs.sort(key=lambda r: r["docs_per_s"])
    mid = dict(runs[(len(runs) - 1) // 2])
    mid.update(_rounds=len(runs), _min=runs[0]["docs_per_s"], _max=runs[-1]["docs_per_s"])
    return mid


def docs_s(r):
    """docs/s com a faixa entre rodadas, quando houver mais de uma."""
    if r.get("_rounds", 1) > 1:
        return f"{fmt(r['docs_per_s'])} ({fmt(r['_min'])}–{fmt(r['_max'])})"
    return fmt(r["docs_per_s"])


def can_drop_caches():
    return subprocess.run(["sudo", "-n", "true"], capture_output=True).returncode == 0


# ---------------------------------------------------------------------- fases

def phase_ladder(scale, experiments=False):
    """Fase 2: carga de cada fatia, até a escala, num banco novo; mais tamanhos."""
    for s in ORDER[: ORDER.index(scale) + 1]:
        step(f"escada: carga {s}")
        path = db(f"ptwiki-{s}")
        load_rounds(path, s, f"ptwiki-{s}", "--batch", BATCH)
        wiki("sizes", "--db", path)
    if experiments:
        phase_experiments(scale)


def phase_experiments(scale):
    """Experimentos da Fase 2, cada um num banco temporário (o plano usa 100k)."""
    s = smaller(scale, "100k")
    n = docs_in(s)
    tmp = db("tmp-exp")
    runs = [(f"batch-{b}", ["--batch", b]) for b in (100, 1000, 5000, 50000) if b <= n]
    runs += [("remapped-ids", ["--batch", BATCH, "--remap-ids"]),
             ("cache-64mb", ["--batch", BATCH, "--cache-mb", 64]),
             ("cache-1024mb", ["--batch", BATCH, "--cache-mb", 1024]),
             ("no-node-cache", ["--batch", BATCH, "--node-cache", 0]),
             ("no-positions", ["--batch", BATCH, "--no-positions"])]
    for label, extra in runs:
        step(f"experimento {s}: {label}")
        load_rounds(tmp, s, f"exp-{s}-{label}", *extra)
        if label in ("remapped-ids", "no-positions"):
            sizes_of(tmp, f"exp-{s}-{label}")
        if label == "no-positions":
            compact_and_save(tmp, f"{s}-no-positions")
    stream = smaller(s, "10k") if s != "10k" else "1k"
    step(f"experimento: streaming (1 commit por documento, {stream})")
    fresh(tmp)
    wiki("load", "--db", tmp, "--corpus", corpus(stream), "--streaming", "--label", f"exp-{s}-streaming-{stream}")
    shutil.rmtree(tmp, ignore_errors=True)


def metrics_args():
    """Com `FF_METRICS_FILE`, a CLI grava as métricas no mesmo arquivo que o
    harness (ver `examples/wiki/env.rs`)."""
    file = os.environ.get("FF_METRICS_FILE")
    return ["--metrics-file", file] if file else []


def compact_and_save(path, label):
    """Compacta e grava o tamanho antes e depois: o tamanho logo após a carga
    depende de quanto do WAL já virou SST, o compactado é estável."""
    before = dir_size(path)
    t = time.time()
    run([FF, "compact", path, *metrics_args()])
    save_result("compact", label, {"seconds": time.time() - t, "db_before": before, "db_after": dir_size(path)})


def sizes_of(path, label):
    """`wiki sizes` de um banco temporário, gravado com um rótulo próprio (o
    padrão é o nome da pasta, que se repete entre experimentos)."""
    named = os.path.join(os.path.dirname(path), label)
    if os.path.abspath(named) == os.path.abspath(path):
        wiki("sizes", "--db", path)
        return
    if os.path.exists(named):
        shutil.rmtree(named)
    os.rename(path, named)
    try:
        wiki("sizes", "--db", named)
    finally:
        os.rename(named, path)


def phase_correctness(scale):
    """Fase 1: oráculo e checagens de conteúdo lógico."""
    checks = {}
    tmp = db("tmp-correctness")
    small = smaller(scale, "10k")

    step(f"oráculo: {small}, todos os termos")
    path_small = ensure_loaded(small)
    checks[f"oracle_{small}"] = wiki("oracle", "--db", path_small, "--corpus", corpus(small),
                                      "--label", f"{small}-todos", check=False) == 0

    step(f"idempotência: indexar a fatia de {small} duas vezes")
    copy_db(path_small, tmp)
    before = digest(tmp)
    size_before = dir_size(tmp)
    wiki("load", "--db", tmp, "--corpus", corpus(small), "--label", f"idempotence-{small}")
    after = digest(tmp)
    size_twice = dir_size(tmp)
    run([FF, "compact", tmp, *metrics_args()])
    checks["idempotence"] = before["digest"] == after["digest"]
    idempotence = {"slice": small, "digest_1st": before, "digest_2nd": after, "db_1st": size_before,
            "db_2nd": size_twice, "db_2nd_compacted": dir_size(tmp)}

    step("lotes vs. único: 1k com --batch 1 e com --batch 1000")
    a, b = db("tmp-batch-1"), db("tmp-batch-1000")
    for path, batch in ((a, 1), (b, 1000)):
        fresh(path)
        wiki("load", "--db", path, "--corpus", corpus("1k"), "--batch", batch, "--label", f"batch-{batch}-1k")
    da, dbb = digest(a), digest(b)
    checks["batches_vs_single"] = da["digest"] == dbb["digest"]
    for p in (a, b):
        shutil.rmtree(p, ignore_errors=True)

    big = smaller(scale, "100k")
    if big != small:
        step(f"oráculo: {big}, amostra de 50 mil termos")
        checks[f"oracle_{big}"] = wiki("oracle", "--db", ensure_loaded(big), "--corpus", corpus(big),
                                        "--sample-terms", 50000, "--label", f"{big}-amostra", check=False) == 0

    n = docs_in(big)
    extra = max(100, n // 100)
    step(f"reabrir: copiar {big}, carregar mais {extra} docs e conferir {n + extra}")
    copy_db(ensure_loaded(big), tmp)
    wiki("load", "--db", tmp, "--corpus", corpus("full"), "--skip", n, "--take", extra, "--label", f"reopen-{big}")
    checks["reopen"] = wiki("oracle", "--db", tmp, "--corpus", corpus("full"), "--take", n + extra,
                             "--sample-terms", 20000, "--label", f"{big}-reaberto", check=False) == 0
    shutil.rmtree(tmp, ignore_errors=True)

    save_result("correctness", scale, {"ok": all(checks.values()), "checks": checks,
                                      "idempotence": idempotence, "batches": {"batch_1": da, "batch_1000": dbb}})
    for name, ok in checks.items():
        print(f"  {GREEN + '✔' if ok else RED + '✗'}{RESET} {name}")
    return all(checks.values())


def phase_queries(scale):
    """Fase 3: vocabulário da fatia e conjuntos de consulta (em bench/queries/<escala>/)."""
    step(f"vocabulário de {scale}")
    vocab = os.path.join(PROJECT, "bench", "data", f"vocab-{scale}.tsv")
    wiki("vocab", "--corpus", corpus(scale), "--out", vocab)
    step("conjuntos de consulta (seed 42)")
    wiki("queries", "--vocab", vocab, "--corpus", corpus(scale), "--seed", 42, "--out", queries_dir(scale))


def bench(path, scale, sets, label, threads=1, extra=()):
    qdir = queries_dir(scale)
    files = [os.path.join(qdir, f"{s}.txt") for s in sets] if sets else [qdir]
    # Conjuntos novos (frases, ranking) aparecem quando o `wiki queries` muda.
    if not os.path.isdir(qdir) or not all(os.path.exists(f) for f in files) \
            or not all(os.path.exists(os.path.join(qdir, f)) for f in ("rank-freq.txt", "t-inflection.txt")):
        phase_queries(scale)
    wiki("bench", "--db", path, "--queries", *files, "--threads", threads, "--label", label, *extra)


def open_time(path, runs=5):
    """Tempo de abrir o banco e fazer uma busca que não acha nada."""
    times = []
    for _ in range(runs):
        t = time.time()
        subprocess.run([FF, "search", path, "xyzzyinexistente"], stdout=subprocess.DEVNULL, check=True)
        times.append(time.time() - t)
    return {"median_s": statistics.median(times), "min_s": min(times), "max_s": max(times), "runs": runs}


def phase_latency(scale):
    """Fase 4: latência no banco da escala. Recarrega o banco para medir o merge pendente."""
    path = db(f"ptwiki-{scale}")
    step(f"carga {scale} limpa (para medir antes da compactação)")
    fresh(path)
    wiki("load", "--db", path, "--corpus", corpus(scale), "--batch", BATCH, "--label", f"ptwiki-{scale}")

    step("merge pendente: termos, títulos, frases e ranking antes da compactação")
    pre_open = open_time(path)
    bench(path, scale, ["t-freq", "titles", "and-freq", "rank-freq", "phrase-2", "phrase-freq"], f"{scale}-pre-compactacao")

    step("compactação")
    size_before = dir_size(path)
    t = time.time()
    run([FF, "compact", path, *metrics_args()])
    compact_s = time.time() - t
    save_result("compact", scale, {"seconds": compact_s, "db_before": size_before, "db_after": dir_size(path),
                                   "open_before": pre_open, "open_after": open_time(path)})
    sizes_of(path, f"ptwiki-{scale}-compacted")

    step("latência de todos os conjuntos (cache quente)")
    bench(path, scale, None, f"{scale}-t1")

    if can_drop_caches():
        step("latência com cache frio")
        bench(path, scale, ["t-rare", "t-mid", "t-freq", "titles", "mixed"], f"{scale}-t1-frio", extra=["--cold"])
    else:
        print(f"{DIM}pulando cache frio: precisa de `sudo -n` (rode `sudo -v` antes, se quiser){RESET}")

    for mb_ in (64, 1024):
        step(f"block cache de {mb_} MB")
        bench(path, scale, ["t-mid", "titles", "mixed", "p-2"], f"{scale}-cache-{mb_}mb", extra=["--cache-mb", mb_])


def phase_concurrency(scale):
    """Fase 5: escala com threads e leitura durante escrita (os últimos 10% da fatia)."""
    path = ensure_loaded(scale)
    for t in (1, 2, 4, 8, 12):
        step(f"só leitura: mixed com {t} thread(s)")
        bench(path, scale, ["mixed"], f"{scale}-mixed-t{t}", threads=t)

    n = docs_in(scale)
    base_n = n * 9 // 10
    base = db(f"ptwiki-{scale}-90")
    if not usable(base):
        step(f"carga dos primeiros {base_n} artigos (base da leitura durante escrita)")
        fresh(base)
        wiki("load", "--db", base, "--corpus", corpus(scale), "--take", base_n, "--label", f"ptwiki-{scale}-90")
    tmp = db("tmp-concurrency")
    baseline = 5 if n <= 10000 else 15
    for t in (1, 4, 8):
        step(f"leitura durante escrita: {t} leitor(es) + carga dos {n - base_n} restantes")
        copy_db(base, tmp)
        wiki("concurrent", "--db", tmp, "--corpus", corpus(scale), "--skip", base_n,
             "--queries", os.path.join(queries_dir(scale), "mixed.txt"), "--threads", t,
             "--baseline-secs", baseline, "--label", f"{scale}-mixed-t{t}")
    shutil.rmtree(tmp, ignore_errors=True)


def phase_pr(scale, save_baseline=False):
    """Fase 8: oráculo + carga + bench rápido, comparado com a linha de base salva."""
    small = smaller(scale, "10k")
    baseline_path = os.path.join(BENCH_DIR, f"baseline-pr-{scale}.json")
    tmp_small, tmp_big = db("tmp-pr-oracle"), db("tmp-pr-load")
    step(f"pr: oráculo {small}")
    fresh(tmp_small)
    wiki("load", "--db", tmp_small, "--corpus", corpus(small), "--label", f"pr-oracle-{small}")
    ok = wiki("oracle", "--db", tmp_small, "--corpus", corpus(small), "--label", f"pr-{small}", check=False) == 0
    step(f"pr: carga {scale} + bench")
    load_rounds(tmp_big, scale, f"pr-{scale}")
    bench(tmp_big, scale, PR_SETS, f"pr-{scale}")
    for p in (tmp_small, tmp_big):
        shutil.rmtree(p, ignore_errors=True)

    results = load_results()
    current = latest(results, "bench", f"pr-{scale}")
    load_now = median_load(results, f"pr-{scale}")
    if save_baseline or not os.path.exists(baseline_path):
        archive_baseline(baseline_path, current)
        with open(baseline_path, "w") as f:
            json.dump({"bench": current, "load": load_now}, f, indent=2, ensure_ascii=False)
        print(f"linha de base salva em {baseline_path}")
        return ok
    with open(baseline_path) as f:
        base = json.load(f)
    worse = compare_bench(base["bench"], current)
    docs_base, docs_now = base["load"]["docs_per_s"], load_now["docs_per_s"]
    print(f"\ncarga {scale}: {docs_base:.0f} → {docs_now:.0f} docs/s ({pct(docs_now, docs_base):+.1f}%, "
          f"mediana de {load_now.get('_rounds', 1)})")
    if not ok:
        print(f"{RED}✗ oráculo divergiu{RESET}")
    if worse:
        print(f"{RED}✗ p50 piorou mais de 10% em: {', '.join(worse)}{RESET}")
    return ok and not worse


def archive_baseline(path, current):
    """Antes de trocar a linha de base, guarda a anterior (de outro commit)
    como baseline-pr-<escala>-<commit>.json, para o relatório comparar."""
    if not os.path.exists(path):
        return
    with open(path) as f:
        old = json.load(f)
    old_commit = (old.get("bench") or {}).get("commit")
    if old_commit and current and old_commit != current.get("commit"):
        dst = path[:-len(".json")] + f"-{old_commit}.json"
        if not os.path.exists(dst):
            shutil.copy(path, dst)
            print(f"{DIM}linha de base anterior guardada em {dst}{RESET}")


def phase_phrases(scale):
    """Plano de frases e ranking, Fase E: carga com e sem posições, oráculo
    (tf, posições, frases, BM25 e título → artigo) e latência dos conjuntos
    novos, antes e depois de compactar."""
    small = smaller(scale, "10k")
    path = db(f"ptwiki-{scale}")
    step(f"carga {scale} com posições")
    load_rounds(path, scale, f"ptwiki-{scale}", "--batch", BATCH)
    sizes_of(path, f"ptwiki-{scale}")
    tmp = db("tmp-no-positions")
    step(f"carga {scale} sem posições")
    load_rounds(tmp, scale, f"exp-{smaller(scale, '100k')}-no-positions", "--batch", BATCH, "--no-positions")
    sizes_of(tmp, f"exp-{smaller(scale, '100k')}-no-positions")
    compact_and_save(tmp, f"{smaller(scale, '100k')}-no-positions")
    shutil.rmtree(tmp, ignore_errors=True)
    step(f"oráculo {small} (tf, posições, frases, ranking, título → artigo)")
    ok = wiki("oracle", "--db", ensure_loaded(small), "--corpus", corpus(small), "--label", f"{small}-todos", check=False) == 0
    step("latência: frases e ranking antes da compactação")
    bench(path, scale, PHRASE_SETS, f"{scale}-pre-compactacao")
    step("compactação")
    pre_open = open_time(path)
    size_before = dir_size(path)
    t = time.time()
    run([FF, "compact", path, *metrics_args()])
    save_result("compact", scale, {"seconds": time.time() - t, "db_before": size_before, "db_after": dir_size(path),
                                   "open_before": pre_open, "open_after": open_time(path)})
    sizes_of(path, f"ptwiki-{scale}-compacted")
    step("latência de todos os conjuntos (cache quente)")
    bench(path, scale, None, f"{scale}-t1")
    return ok


# Conjuntos medidos antes da compactação (merge pendente), como na Fase 4.
PRE_SETS = ["t-freq", "titles", "and-freq", "rank-freq", "phrase-2", "phrase-freq", "t-inflection"]
# Bancos comparados pelo `stemming` (docs/plano-stemming.md): o primeiro é a referência.
STEM_MODES = {
    "none": ["--stemming", "none"],
    "pt": ["--stemming", "pt"],
    "pt-plural": ["--stemming", "pt-plural"],
    "inflections": ["--inflections", "pt-plural"],
}
STEM_TITLES = {"none": "sem stemming", "pt": "Snowball na indexação", "pt-plural": "plural na indexação",
               "inflections": "flexões na consulta (pt-plural)"}
# Acerto de cada conjunto de relevância: top-1, top-3 ou top-10.
REL_HIT = {"rel-title": "top1", "rel-typing": "top1", "rel-hub": "top1", "rel-body": "top10",
           "rel-inflection": "top10", "rel-plural": "top10", "rel-manual": "top3", "rel-inflection-manual": "top3"}


def phase_stemming(scale):
    """Stemming e flexões, no mesmo binário: sem stemming, Snowball e plural na
    indexação e flexões na consulta. Oráculo de cada modo, carga, tamanho,
    latência antes e depois de compactar e relevância. As consultas são as
    mesmas para todos (palavras sem stemming): com stemming, cada uma casa a
    união das flexões, que é o custo real dele."""
    small = smaller(scale, "10k")
    ok = True
    for mode, flags in STEM_MODES.items():
        if mode == "none":
            continue
        oracle_db = db(f"stemming-{small}-oracle")
        step(f"oráculo {small}, {STEM_TITLES[mode]}")
        fresh(oracle_db)
        wiki("load", "--db", oracle_db, "--corpus", corpus(small), "--batch", BATCH, *flags,
             "--label", f"stemming-{small}-{mode}-oracle")
        ok &= wiki("oracle", "--db", oracle_db, "--corpus", corpus(small), "--label", f"{small}-stemming-{mode}",
                   check=False) == 0
        shutil.rmtree(oracle_db, ignore_errors=True)

    paths = {mode: db(f"stemming-{scale}-{mode}") for mode in STEM_MODES}
    for mode, path in paths.items():
        step(f"carga {scale}, {STEM_TITLES[mode]}")
        load_rounds(path, scale, f"stemming-{scale}-{mode}", "--batch", BATCH, *STEM_MODES[mode])
    for mode, path in paths.items():
        step(f"antes da compactação, {STEM_TITLES[mode]}")
        bench(path, scale, PRE_SETS, f"stemming-{scale}-{mode}-pre-compactacao")
    for mode, path in paths.items():
        step(f"compactação, {STEM_TITLES[mode]}")
        compact_and_save(path, f"stemming-{scale}-{mode}")
        sizes_of(path, f"stemming-{scale}-{mode}-compacted")
    for mode, path in paths.items():
        step(f"latência de todos os conjuntos, {STEM_TITLES[mode]}")
        bench(path, scale, None, f"stemming-{scale}-{mode}-t1")

    rel = smaller(scale, "100k")
    sets = os.path.join(PROJECT, "bench", "relevance", rel)
    for mode in STEM_MODES:
        path = paths[mode] if rel == scale else db(f"stemming-{rel}-{mode}")
        if rel != scale:
            step(f"carga {rel} para a relevância, {STEM_TITLES[mode]}")
            fresh(path)
            wiki("load", "--db", path, "--corpus", corpus(rel), "--batch", BATCH, *STEM_MODES[mode],
                 "--label", f"stemming-{rel}-{mode}-relevance")
        step(f"relevância {rel}, {STEM_TITLES[mode]}")
        wiki("relevance", "--db", path, "--corpus", corpus(rel), "--sets", sets, "--label", f"{rel}-stemming-{mode}")
    return ok


def stemming_section(R, scale):
    modes = [m for m in STEM_MODES if median_load(R, f"stemming-{scale}-{m}")]
    if "none" not in modes or len(modes) < 2:
        return []
    base = "none"
    loads = {m: median_load(R, f"stemming-{scale}-{m}") for m in modes}
    sizes = {m: latest(R, "sizes", f"stemming-{scale}-{m}-compacted") for m in modes}
    comps = {m: latest(R, "compact", f"stemming-{scale}-{m}") for m in modes}
    small = smaller(scale, "10k")
    keys = lambda r, space: next((e["keys"] for e in (r or {}).get("spaces", []) if e["space"] == space), None)
    space_bytes = lambda r, space: next((e["key_bytes"] + e["value_bytes"] for e in (r or {}).get("spaces", [])
                                         if e["space"] == space), None)

    def cells(values, f=fmt):
        out = []
        for m in modes:
            v, b = values.get(m), values.get(base)
            txt = f(v) if v is not None else "—"
            if m != base and v is not None and b:
                txt += f" ({pct(v, b):+.0f}%)"
            out.append(txt)
        return " | ".join(out)

    head = "| | " + " | ".join(STEM_TITLES[m] for m in modes) + " |"
    sep = "|---" * (len(modes) + 1) + "|"
    L = [f"## Stemming e flexões ({scale})", "",
         "Os mesmos dados e consultas em bancos criados de jeitos diferentes "
         "([docs/plano-stemming.md](" + DOCS_URL + "/plano-stemming.md)). As consultas são palavras do vocabulário "
         "sem stemming; com stemming, cada uma casa a união das flexões.", ""]
    oracles = [(m, latest(R, "oracle", f"{small}-stemming-{m}")) for m in modes if m != base]
    if any(o for _, o in oracles):
        L += ["- Oráculo de " + small + ": " + ", ".join(
            f"{STEM_TITLES[m]} **{o['divergences']} divergências**" for m, o in oracles if o) + ".", ""]
    L += [head, sep,
          "| Carga (s) | " + cells({m: loads[m]["seconds"] for m in modes}, lambda v: fmt(v, 1)) + " |",
          "| Pico de RSS (MB) | " + cells({m: loads[m]["peak_rss"] for m in modes}, mb) + " |",
          "| Termos distintos | " + cells({m: keys(sizes[m], "post") for m in modes}) + " |",
          "| Banco compactado (MB) | " + cells({m: (comps[m] or {}).get("db_after") for m in modes}, mb) + " |",
          "| `pos:` (MB) | " + cells({m: space_bytes(sizes[m], "pos") for m in modes}, mb) + " |",
          "| `stem:` (MB) | " + cells({m: space_bytes(sizes[m], "stem") for m in modes}, mb) + " |",
          ""]
    for title, suffix in (("Latência p50 (cache quente, compactado)", "t1"), ("Antes da compactação (p50)", "pre-compaction")):
        benches = {m: latest(R, "bench", f"stemming-{scale}-{m}-{suffix}") for m in modes}
        if not all(benches.values()):
            continue
        by = {m: {x["set"]: x for x in benches[m]["sets"]} for m in modes}
        L += [f"### {title}", "", "| Conjunto | " + " | ".join(STEM_TITLES[m] for m in modes) + " |", sep]
        for name in by[base]:
            L.append(f"| `{name}` | " + cells({m: by[m].get(name, {}).get("p50_us") for m in modes}, us) + " |")
        if suffix == "t1":
            L += ["", "p99:", "", "| Conjunto | " + " | ".join(STEM_TITLES[m] for m in modes) + " |", sep]
            for name in by[base]:
                L.append(f"| `{name}` | " + cells({m: by[m].get(name, {}).get("p99_us") for m in modes}, us) + " |")
        L.append("")
    rel = smaller(scale, "100k")
    rels = {m: latest(R, "relevance", f"{rel}-stemming-{m}") for m in modes}
    if all(rels.values()):
        by = {m: {c["set"]: c for c in rels[m]["sets"]} for m in modes}
        L += [f"### Relevância ({rel})", "", "Acerto: top-1 em título, digitando e hub; top-3 nas consultas à mão; top-10 no resto.", "",
              "| Conjunto | " + " | ".join(STEM_TITLES[m] for m in modes) + " |", sep]
        for name in by[base]:
            hit = REL_HIT.get(name, "top10")
            row = " | ".join(f"{fmt(by[m][name][hit] * 100, 1)}%" if name in by[m] else "—" for m in modes)
            L.append(f"| `{name}` ({hit.replace('top', 'top-')}) | {row} |")
        L.append("")
    return L


# Edição e remoção (docs/plano-edicao.md). Os textos novos do plano são os
# artigos seguintes à fatia, então ele usa o corpus da fatia seguinte.
EDIT_DONORS = {"1k": "10k", "10k": "100k", "100k": "full"}
# Medidos no banco limpo e no editado: os do merge pendente, mais autocompletar e prefixo.
EDIT_SETS = ["t-mid", "t-freq", "titles", "and-freq", "rank-freq", "rank-titles", "phrase-2", "phrase-freq",
             "s-1", "s-2", "p-1", "p-2"]
# Lotes da vazão das edições e quantas operações cada um aplica (None = o plano inteiro).
EDIT_BATCHES = [(10000, None), (100, 3000), (1, 300)]
# Latência: cada banco editado contra um carregado direto com o mesmo estado
# final (mesmo conteúdo, sem edições).
EDIT_STAGES = [("clean-pre", "fatia carregada, antes de compactar"),
               ("trickle-pre", "fatia + 1.000 edições de 1 documento, antes de compactar"),
               ("final-pre", "estado final carregado, antes de compactar"),
               ("edited-pre", "editado, antes de compactar"),
               ("final-compacted", "estado final carregado, compactado"),
               ("edited-compacted", "editado, compactado"),
               ("clean-compacted", "fatia, compactada")]
# Com o que cada etapa é comparada.
EDIT_BASES = {"trickle-pre": "clean-pre", "edited-pre": "final-pre", "edited-compacted": "final-compacted"}


def phase_edits(scale):
    """Edição e remoção (docs/analise-edicao.md, plano A): oráculo e
    equivalência com um plano de edições, vazão das edições por tamanho de
    lote, latência do banco editado contra o limpo (antes e depois de
    compactar) e buscas com um editor em paralelo. A carga e o banco sem
    edições se comparam com a `main` no A/B de sempre."""
    if scale not in EDIT_DONORS:
        sys.exit(f"edits: escala {scale} sem artigos depois da fatia para o plano; use 100k ou menor")
    n = docs_in(scale)
    src = corpus(EDIT_DONORS[scale])
    small = smaller(scale, "10k")
    small_src = corpus(EDIT_DONORS[small])
    ok = True

    step(f"oráculo {small} com o plano de edições, e digest contra a carga direta do estado final")
    edited, final = db(f"edits-{small}-oracle"), db(f"edits-{small}-final")
    for path in (edited, final):
        fresh(path)
    wiki("load", "--db", edited, "--corpus", small_src, "--take", docs_in(small), "--batch", BATCH,
         "--label", f"edits-{small}-oracle")
    wiki("edit", "--db", edited, "--corpus", small_src, "--take", docs_in(small), "--batch", 1000,
         "--label", f"edits-{small}-oracle")
    wiki("load", "--db", final, "--corpus", small_src, "--take", docs_in(small), "--batch", BATCH, "--edits",
         "--label", f"edits-{small}-final")
    # Antes da compactação as versões mortas ainda estão nas listas: o
    # conteúdo lógico (listas sem os mortos) já tem que ser o mesmo. As notas
    # (o df conta os mortos) e o autocompletar só batem depois dela.
    same_pre = digest(edited)["digest"] == digest(final)["digest"]
    run([FF, "compact", edited, *metrics_args()])
    ok &= wiki("oracle", "--db", edited, "--corpus", small_src, "--take", docs_in(small), "--edits",
               "--label", f"{small}-edits", check=False) == 0
    same = same_pre and digest(edited)["digest"] == digest(final)["digest"]
    save_result("edits-digest", small, {"same": same, "same_before_compaction": same_pre})
    if not same:
        print(f"{RED}✗ o banco editado difere do carregado direto no estado final{RESET}")
    ok &= same
    for path in (edited, final):
        shutil.rmtree(path, ignore_errors=True)

    clean = db(f"edits-{scale}-clean")
    step(f"carga {scale}")
    fresh(clean)
    wiki("load", "--db", clean, "--corpus", corpus(scale), "--batch", BATCH, "--label", f"edits-{scale}-clean")

    tmp = db("tmp-edits")
    for batch, max_ops in EDIT_BATCHES:
        step(f"vazão das edições: lote de {batch}" + (f", {max_ops} operações" if max_ops else ", o plano inteiro"))
        copy_db(clean, tmp)
        extra = ["--max-ops", max_ops] if max_ops else []
        wiki("edit", "--db", tmp, "--corpus", src, "--take", n, "--batch", batch, *extra,
             "--label", f"edits-{scale}-b{batch}")
    step("latência: edições contínuas (1.000 commits de 1 documento) contra a fatia sem edições")
    copy_db(clean, tmp)
    wiki("edit", "--db", tmp, "--corpus", src, "--take", n, "--batch", 1, "--max-ops", 1000,
         "--label", f"edits-{scale}-trickle")
    bench(clean, scale, EDIT_SETS, f"edits-{scale}-clean-pre")
    bench(tmp, scale, EDIT_SETS, f"edits-{scale}-trickle-pre")
    shutil.rmtree(tmp, ignore_errors=True)

    edited, final = db(f"edits-{scale}-edited"), db(f"edits-{scale}-final")
    step("latência: estado final carregado direto, antes de compactar")
    fresh(final)
    wiki("load", "--db", final, "--corpus", src, "--take", n, "--batch", BATCH, "--edits",
         "--label", f"edits-{scale}-final")
    bench(final, scale, EDIT_SETS, f"edits-{scale}-final-pre")
    step("latência: editado antes de compactar (versões mortas nas listas)")
    copy_db(clean, edited)
    wiki("edit", "--db", edited, "--corpus", src, "--take", n, "--batch", BATCH, "--label", f"edits-{scale}-latency")
    bench(edited, scale, EDIT_SETS, f"edits-{scale}-edited-pre")
    step("compactação dos três bancos")
    banks = ((final, "final"), (edited, "edited"), (clean, "clean"))
    for path, label in banks:
        compact_and_save(path, f"edits-{scale}-{label}")
    sizes_of(edited, f"edits-{scale}-edited-compacted")
    sizes_of(final, f"edits-{scale}-final-compacted")
    for (path, _), stage in zip(banks, ("final-compacted", "edited-compacted", "clean-compacted")):
        step(f"latência: {dict(EDIT_STAGES)[stage]}")
        bench(path, scale, EDIT_SETS, f"edits-{scale}-{stage}")
    shutil.rmtree(final, ignore_errors=True)

    step("buscas com o plano de edições em paralelo (4 leitores)")
    tmp = db("tmp-edits-concurrent")
    copy_db(clean, tmp)
    wiki("concurrent", "--db", tmp, "--corpus", src, "--skip", n, "--edits", "--batch", 1000,
         "--queries", os.path.join(queries_dir(scale), "mixed.txt"), "--threads", 4,
         "--baseline-secs", 5 if n <= 10000 else 15, "--label", f"edits-{scale}-mixed-t4")
    for label, extra in (("trickle", []), ("trickle-maintenance", ["--maintenance", 1000])):
        step("buscas com 3.000 edições de 1 documento em paralelo (4 leitores)"
             + (", com manutenção a cada 1.000" if extra else ""))
        copy_db(clean, tmp)
        wiki("concurrent", "--db", tmp, "--corpus", src, "--skip", n, "--edits", "--batch", 1, "--take", 3000,
             "--queries", os.path.join(queries_dir(scale), "mixed.txt"), "--threads", 4, *extra,
             "--baseline-secs", 5 if n <= 10000 else 15, "--label", f"edits-{scale}-{label}-t4")
    shutil.rmtree(tmp, ignore_errors=True)
    return ok


def edits_section(R, scale):
    load = latest(R, "load", f"edits-{scale}-clean")
    if not load:
        return []
    small = smaller(scale, "10k")
    comps = {k: latest(R, "compact", f"edits-{scale}-{k}") for k in ("clean", "final", "edited")}
    oracle = latest(R, "oracle", f"{small}-edits")
    same = latest(R, "edits-digest", small)
    L = [f"## Edição e remoção ({scale})", "",
         "Plano com seed fixa sobre a fatia: troca o texto de 50% dos documentos pelo de artigos seguintes, "
         "remove 10% e readiciona 1% ([docs/analise-edicao.md](" + DOCS_URL + "/analise-edicao.md), plano A).", ""]
    if oracle or same:
        L += [f"- Oráculo de {small} depois do plano (compactado): "
              f"**{oracle['divergences'] if oracle else '—'} divergências**; digest do banco editado "
              f"{'**igual**' if same and same.get('same') else '**diferente**'} ao da carga direta do estado final "
              "(antes e depois de compactar).", ""]
    def cell(v, base, f):
        if v is None:
            return "—"
        return f"{f(v)} ({pct(v, base):+.1f}%)" if base else f(v)
    db_final, db_edited = (comps["final"] or {}).get("db_after"), (comps["edited"] or {}).get("db_after")
    L += [f"- Carga da fatia: {fmt(load['seconds'], 1)} s ({docs_s(load)} docs/s).",
          f"- Compactação: fatia {fmt((comps['clean'] or {}).get('seconds'), 1)} s; estado final carregado "
          f"{fmt((comps['final'] or {}).get('seconds'), 1)} s; editado {fmt((comps['edited'] or {}).get('seconds'), 1)} s.",
          f"- Banco compactado: estado final carregado {mb(db_final) if db_final else '—'} MB; editado "
          f"{cell(db_edited, db_final, mb)} MB.", ""]
    rows = [(batch, latest(R, "edit", f"edits-{scale}-b{batch}")) for batch, _ in EDIT_BATCHES]
    if any(r for _, r in rows):
        L += ["### Vazão das edições", "", "| Lote | Operações | ops/s | commit p50 | commit p99 | versões mortas |",
              "|---|---|---|---|---|---|"]
        for batch, r in rows:
            if r:
                L.append(f"| {fmt(batch)} | {fmt(r['ops'])} | {fmt(r['ops_per_s'])} "
                         f"| {fmt(r['commit_p50_s'] * 1e3, 1)} ms | {fmt(r['commit_p99_s'] * 1e3, 1)} ms "
                         f"| {fmt(r.get('killed', r.get('tombstones')))} |")
        L.append("")
    benches = {stage: latest(R, "bench", f"edits-{scale}-{stage}") for stage, _ in EDIT_STAGES}
    if all(benches.values()):
        by = {stage: {x["set"]: x for x in b["sets"]} for stage, b in benches.items()}
        L += ["### Latência p50", "",
              "Entre parênteses, a diferença para a referência: as edições contínuas contra a fatia sem edições; "
              "o banco do plano inteiro contra o estado final carregado direto (mesmo conteúdo) no mesmo estado "
              "de compactação.", "",
              "| Conjunto | " + " | ".join(title for _, title in EDIT_STAGES) + " |",
              "|---" * (len(EDIT_STAGES) + 1) + "|"]
        for name in by["clean-pre"]:
            cells = []
            for stage, _ in EDIT_STAGES:
                v = by[stage].get(name, {}).get("p50_us")
                base = by[EDIT_BASES[stage]].get(name, {}).get("p50_us") if stage in EDIT_BASES else None
                cells.append(cell(v, base, us))
            L.append(f"| `{name}` | " + " | ".join(cells) + " |")
        L.append("")
    concs = [(title, latest(R, "concurrent", f"edits-{scale}-{label}-t4"))
             for label, title in (("mixed", "o plano inteiro (lote de 1.000)"),
                                  ("trickle", "3.000 edições de 1 documento"),
                                  ("trickle-maintenance", "3.000 edições de 1 documento, com manutenção a cada 1.000"))]
    if any(c for _, c in concs):
        L += ["### Buscas com edições em paralelo (4 leitores, `mixed`)", "",
              "| Escrita | p50 sem → com | p99 sem → com | qps sem → com |", "|---|---|---|---|"]
        for title, c in concs:
            if c:
                a, b = c["read_without_write"], c["read_with_write"]
                L.append(f"| {title} | {us(a['p50_us'])} → {us(b['p50_us'])} | {us(a['p99_us'])} → {us(b['p99_us'])} "
                         f"| {fmt(a['qps'])} → {fmt(b['qps'])} |")
        L.append("")
    return L


# ---------------------------------------------------------------- bindings

NODE_HARNESS = os.path.join(PROJECT, "bench", "node")
# Leitores em paralelo do binding: worker_threads contra threads do Rust.
BINDING_THREADS = [1, 2, 4, 8]
# Rodadas alternando nativo e Node (as diferenças são de µs).
BINDING_ROUNDS = 3


def node(script, *args):
    return run(["node", os.path.join(NODE_HARNESS, script), *map(str, args)])


def phase_bindings(scale):
    """Bindings (docs/plano-bindings.md): o binding Node contra o nativo na
    mesma máquina e no mesmo banco. O nativo mede com `--plain` (sem
    PerfContext nem estatísticas do RocksDB), como uma aplicação."""
    step("compilando o binding Node (release)")
    run(["node", os.path.join(PROJECT, "bindings", "node", "scripts", "build.js")])
    path = db(f"ptwiki-{scale}")
    if not os.path.isdir(path):
        step(f"carga {scale}")
        fresh(path)
        wiki("load", "--db", path, "--corpus", corpus(scale), "--batch", BATCH, "--label", f"ptwiki-{scale}")
    qdir = queries_dir(scale)
    if not os.path.isdir(qdir):
        phase_queries(scale)
    for r in range(BINDING_ROUNDS):
        step(f"latência, rodada {r + 1} de {BINDING_ROUNDS}: nativo e Node")
        # Os 10 melhores em todo conjunto (a medida do plano) e a operação de
        # cada conjunto (search devolve todos os IDs: mostra o custo de listas grandes).
        for suffix, mode in (("-top10", ["--mode", "top10"]), ("", [])):
            wiki("bench", "--db", path, "--queries", qdir, "--plain", *mode, "--label", f"bindings-{scale}-native{suffix}")
            node("bench.js", "--db", path, "--queries", qdir, *mode, "--label", f"bindings-{scale}-node{suffix}")
    step("leitores em paralelo: threads do Rust e worker_threads")
    mixed = os.path.join(qdir, "mixed.txt")
    for t in BINDING_THREADS:
        wiki("bench", "--db", path, "--queries", mixed, "--threads", t, "--plain",
             "--label", f"bindings-{scale}-native-t{t}")
    node("workers.js", "--db", path, "--queries", mixed, "--threads", *BINDING_THREADS,
         "--label", f"bindings-{scale}-node")
    load_scale = smaller(scale, "100k")
    tmp = db("tmp-bindings")
    for r in range(BINDING_ROUNDS):
        step(f"carga {load_scale}, rodada {r + 1} de {BINDING_ROUNDS}: nativo e Node (lote de {BATCH})")
        fresh(tmp)
        wiki("load", "--db", tmp, "--corpus", corpus(load_scale), "--batch", BATCH, "--plain",
             "--label", f"bindings-{load_scale}-native")
        fresh(tmp)
        node("load.js", "--db", tmp, "--corpus", corpus(load_scale), "--batch", BATCH,
             "--label", f"bindings-{load_scale}-node")
    shutil.rmtree(tmp, ignore_errors=True)
    return True


def phase_relevance(scale):
    """Plano de relevância: top-1/top-3/top-10/MRR em conjuntos com resposta
    conhecida, gerados da fatia (até 100k) e versionados em bench/relevance/."""
    s = smaller(scale, "100k")
    step(f"relevância {s}: título, digitando, hub, corpo e consultas à mão")
    sets = os.path.join(PROJECT, "bench", "relevance", s)
    wiki("relevance", "--db", ensure_loaded(s), "--corpus", corpus(s), "--sets", sets, "--label", s)


def relevance_section(R, scale):
    s = smaller(scale, "100k")
    rel = latest(R, "relevance", s)
    if not rel:
        return []
    base = latest(R, "relevance", f"linha-de-base-{s}")
    before = {c["set"]: c for c in base["sets"]} if base else {}
    L = [f"## Relevância ({s})", "",
         f"Consultas com resposta conhecida ([docs/plano-relevancia.md]({DOCS_URL}/plano-relevancia.md)). "
         "Acerto: top-1 em título, digitando e hub; top-10 em corpo; top-3 nas consultas à mão. "
         "Entre parênteses, a linha de base (ranking antes do plano).", "",
         "| Conjunto | Consultas | top-1 | top-3 | top-10 | MRR | fora do top-1000 |", "|---|---|---|---|---|---|---|"]
    for c in rel["sets"]:
        b = before.get(c["set"])
        def cell(key, as_pct=True):
            v = c[key]
            txt = f"{fmt(v * 100, 1)}%" if as_pct else fmt(v, 3)
            if b:
                txt += f" ({fmt(b[key] * 100, 1)}%)" if as_pct else f" ({fmt(b[key], 3)})"
            return txt
        L.append(f"| `{c['set']}` | {c['queries']} | {cell('top1')} | {cell('top3')} | {cell('top10')} | "
                 f"{cell('mrr', False)} | {c['outside_top1000']} |")
    L.append("")
    errors = [(c["set"], e) for c in rel["sets"] for e in c.get("errors", [])[:3]]
    if errors:
        L += ["Exemplos de erro (até 3 por conjunto):", "",
              "| Conjunto | Consulta | Esperado | Posição | Os 3 primeiros |", "|---|---|---|---|---|"]
        for set_name, e in errors:
            pos = f"{e['position']}º" if e.get("position") else "—"
            L.append(f"| `{set_name}` | `{e['query']}` | {e['expected']} | {pos} | {' · '.join(e['top3'])} |")
        L.append("")
    L.append(f"<sub>{rel['_file']}</sub>")
    L.append("")
    return L


def phase_long_load(scale, reproduce=False):
    """Cenário do colapso da carga: a velocidade caía de ~5.000 para ~200
    artigos/s em pontos diferentes (200k, 500k, 600k) da Wikipédia inteira.
    Causa: filtros de Bloom de um bloco só, maiores que uma parte do block
    cache (ver `bloom_filters_larger_than_...` em tests/engine/storage.rs). O `wiki
    load` acompanha a carga em janelas de 10k artigos e acusa a primeira cuja
    velocidade (MB/s) cai abaixo de 20% da mediana das anteriores.

    - estresse: 200k artigos com cache de 32 MB (partes de 512 KB), onde o
      problema aparecia já com ~60k artigos;
    - padrão: a fatia da escala, com a config padrão, como o
      `carregar_wikipedia.py` (lote de 2000);
    - com `--reproduce`, antes, o estresse com filtro inteiro, que tem que
      colapsar (confere que o cenário e o detector funcionam)."""
    tmp = db("tmp-long-load")
    runs = [("stress-cache-32mb", "full", ["--take", 200000, "--cache-mb", 32], False),
            (f"default-{scale}", scale, [], False)]
    if reproduce:
        runs.insert(0, ("stress-whole-filter", "full", ["--take", 200000, "--cache-mb", 32, "--whole-filter"], True))
    ok, summary = True, []
    for name, slice_name, extra, expect_collapse in runs:
        label = f"long-load-{name}"
        step(f"carga longa: {name}" + (" (tem que colapsar)" if expect_collapse else ""))
        fresh(tmp)
        wiki("load", "--db", tmp, "--corpus", corpus(slice_name), "--batch", BATCH, *extra, "--label", label)
        r = latest(load_results(), "load", label)
        collapse = r.get("collapse")
        good = bool(collapse) == expect_collapse
        ok &= good
        summary.append({"scenario": name, "label": label, "expected_collapse": expect_collapse, "ok": good,
                        "collapse_at": collapse and collapse["docs"], "docs": r["docs"], "seconds": r["seconds"]})
        print(f"  {GREEN + '✔' if good else RED + '✗'}{RESET} {name}: "
              f"{'colapso em ' + fmt(collapse['docs']) + ' docs' if collapse else 'sem colapso'}, "
              f"{fmt(r['docs'])} docs em {dur(r['seconds'])}")
    shutil.rmtree(tmp, ignore_errors=True)
    save_result("long-load", scale, {"ok": ok, "scenarios": summary})
    return ok


def long_load_section(R, scale):
    res = latest(R, "long-load", scale)
    if not res:
        return []
    L = [f"## Carga longa ({scale})", "",
         "Cenário do colapso da carga (filtros de Bloom maiores que uma parte do block cache). Uma janela "
         "colapsa quando o MB/s cai abaixo de 20% da mediana das 10 anteriores.", "",
         "| Cenário | Esperado | Resultado | Docs | Tempo | docs/s |", "|---|---|---|---|---|---|"]
    for c in res["scenarios"]:
        r = latest(R, "load", c["label"]) or {}
        got = f"colapso em {fmt(c['collapse_at'])} docs" if c["collapse_at"] else "sem colapso"
        L.append(f"| {c['scenario']} | {'collapse' if c['expected_collapse'] else 'sem colapso'} | "
                 f"{'✅' if c['ok'] else '❌'} {got} | {fmt(c['docs'])} | {dur(c['seconds'])} | {fmt(r.get('docs_per_s'))} |")
    L.append("")
    main = next((latest(R, "load", c["label"]) for c in res["scenarios"] if c["scenario"].startswith("default")), None)
    windows = (main or {}).get("windows") or []
    if windows:
        step_ = max(1, len(windows) // 15)
        L += ["### Janelas da carga padrão", "",
              "| Docs | MB/s | docs/s | commit | Gets/doc | filtro relido | stall | L0 | RSS |", "|---|---|---|---|---|---|---|---|---|"]
        for w in windows[step_ - 1::step_]:
            L.append(f"| {fmt(w['docs'])} | {fmt(w['mb_per_s'], 1)} | {fmt(w['docs_per_s'])} | {fmt(w['commit_fraction'] * 100)}% | "
                     f"{fmt(w['gets_per_doc'])} | {fmt(w['filter_mb'])} MB | {fmt(w['stall_s'], 1)}s | {w['l0']} | {fmt(w['rss_mb'])} MB |")
        L.append("")
    return L


# ------------------------------------------------------------------ comparação

def bindings_section(R, scale):
    """O binding Node contra o nativo: mediana das rodadas alternadas."""
    def runs(kind, label):
        return [r for r in R if r.get("kind") == kind and r.get("label") == label]
    def med(rs, name, field):
        vals = [x[field] for r in rs for x in r["sets"] if x["set"] == name]
        return statistics.median(vals) if vals else None
    def table(suffix, sets=None):
        native, node_ = runs("bench", f"bindings-{scale}-native{suffix}"), runs("bench", f"bindings-{scale}-node{suffix}")
        if not native or not node_:
            return [], None, None
        out = ["| Conjunto | p50 nativo | p50 Node | Δ | fronteira | p99 nativo | p99 Node |",
               "|---|---|---|---|---|---|---|"]
        for s in native[-1]["sets"]:
            if sets and s["mode"] not in sets:
                continue
            n50, b50 = med(native, s["set"], "p50_us"), med(node_, s["set"], "p50_us")
            n99, b99 = med(native, s["set"], "p99_us"), med(node_, s["set"], "p99_us")
            inside = med(node_, s["set"], "inside_p50_us")
            if b50 is None:
                continue
            edge = f"{fmt(b50 - inside, 2)} µs" if inside is not None else "—"
            out.append(f"| {s['set']} | {us(n50)} | {us(b50)} | {fmt(b50 - n50, 2)} µs ({pct(b50, n50):+.1f}%) "
                       f"| {edge} | {us(n99)} | {us(b99)} |")
        return out, native, node_
    top, native, node_ = table("-top10")
    if not top:
        return []
    L = [f"## Binding Node ({scale})", "",
         f"Mediana de {len(node_)} rodadas alternando o `wiki bench --plain` e o `bench/node/bench.js` no mesmo "
         "banco ([docs/plano-bindings.md](" + DOCS_URL + "/plano-bindings.md)).", "",
         "Δ é o Node contra o `wiki` (outro processo); **fronteira** é o Node contra a mesma consulta rodada "
         "dentro do addon, no processo do Node: o custo de atravessar do JS para o Rust. O resto do Δ é rodar "
         "dentro do processo do Node (V8, GC e suas threads disputando o cache da CPU).", "",
         "### Os 10 melhores (`searchTopSync`, a medida do plano)", ""] + top
    every, _, _ = table("", sets={"search", "suggest"})
    if every:
        L += ["", "### A operação de cada conjunto (`searchSync` devolve todos os IDs; `suggestSync`)", ""] + every
    last = node_[-1]
    noop = statistics.median([r["noop_ns"] for r in node_ if "noop_ns" in r] or [0])
    labels_node = statistics.median([r["labels_10_us"] for r in node_ if "labels_10_us" in r] or [0])
    labels_native = statistics.median([r["labels_10_us"] for r in native if "labels_10_us" in r] or [0])
    L += ["", f"- Chamada vazia: {fmt(noop, 1)} ns (meta ≤ 200 ns).",
          f"- Rótulos de 10 resultados: Node {fmt(labels_node, 2)} µs, nativo {fmt(labels_native, 2)} µs "
          f"(meta ≤ nativo + 2 µs)."]
    if last.get("async"):
        a = last["async"]
        L.append(f"- `t-mid` síncrono {fmt(a['sync_p50_us'], 1)} µs, assíncrono (pool do libuv) "
                 f"{fmt(a['async_p50_us'], 1)} µs.")
    workers = latest(R, "workers", f"bindings-{scale}-node")
    if workers:
        L += ["", "| Leitores | qps nativo (threads) | qps Node (worker_threads) | Δ |", "|---|---|---|---|"]
        for w in workers["sets"]:
            n = latest(R, "bench", f"bindings-{scale}-native-t{w['threads']}")
            nq = n["sets"][0]["qps"] if n else None
            L.append(f"| {w['threads']} | {fmt(nq)} | {fmt(w['qps'])} | {pct(w['qps'], nq):+.1f}% |" if nq
                     else f"| {w['threads']} | — | {fmt(w['qps'])} | — |")
    load_scale = smaller(scale, "100k")
    ln, lb = runs("load", f"bindings-{load_scale}-native"), runs("load", f"bindings-{load_scale}-node")
    if ln and lb:
        n, b = statistics.median(r["docs_per_s"] for r in ln), statistics.median(r["docs_per_s"] for r in lb)
        L += ["", f"- Carga {load_scale} (lote de {BATCH}): nativo {fmt(n)} docs/s, Node {fmt(b)} docs/s "
              f"({b / n * 100:.1f}% do nativo; meta ≥ 97%)."]
    return L + [""]


def pct(new, old):
    return (new - old) / old * 100 if old else 0.0


def compare_bench(a, b, limit=10.0):
    """Imprime p50/p99 de dois resultados de bench; devolve os conjuntos cujo p50 piorou além do limite."""
    sets_a = {s["set"]: s for s in a["sets"]}
    worse = []
    print(f"\n{'set':<16} {'p50 antes':>10} {'p50 agora':>10} {'Δ':>8}   {'p99 antes':>10} {'p99 agora':>10} {'Δ':>8}")
    for s in b["sets"]:
        o = sets_a.get(s["set"])
        if not o:
            continue
        d50, d99 = pct(s["p50_us"], o["p50_us"]), pct(s["p99_us"], o["p99_us"])
        flag = f"  {RED}piorou{RESET}" if d50 > limit else ""
        if d50 > limit:
            worse.append(s["set"])
        print(f"{s['set']:<16} {o['p50_us']:>9.0f}µ {s['p50_us']:>9.0f}µ {d50:>+7.1f}%   "
              f"{o['p99_us']:>9.0f}µ {s['p99_us']:>9.0f}µ {d99:>+7.1f}%{flag}")
    print(f"{DIM}commits: {a.get('commit')} → {b.get('commit')}{RESET}")
    return worse


def cmd_compare(args):
    with open(args.a) as f:
        a = json.load(f)
    with open(args.b) as f:
        b = json.load(f)
    sys.exit(1 if compare_bench(a, b) else 0)


# -------------------------------------------------------------------- relatório

def fmt(n, decimals=0):
    if n is None:
        return "—"
    s = f"{n:,.{decimals}f}"
    return s.replace(",", "_").replace(".", ",").replace("_", ".")


def mb(n):
    return fmt(n / (1 << 20), 1) if n is not None else "—"


def dur(s):
    if s is None:
        return "—"
    m, sec = divmod(int(round(s)), 60)
    return f"{m}min{sec:02d}s" if m else f"{s:.1f}s"


def pct_txt(v):
    return "—" if v is None else f"{fmt(v * 100, 1)}%"


def us(v):
    if v is None:
        return "—"
    if v >= 1000:
        return f"{fmt(v / 1000, 1)} ms"
    return f"{fmt(v)} µs"


def bench_table(r):
    if not r:
        return ["_sem resultado_", ""]
    rows = ["| Conjunto | p50 | p95 | p99 | máx | qps | resultados (média) | Gets/consulta | block reads/consulta |",
            "|---|---|---|---|---|---|---|---|---|"]
    for s in r["sets"]:
        rows.append(f"| `{s['set']}` | {us(s['p50_us'])} | {us(s['p95_us'])} | {us(s['p99_us'])} | {us(s['max_us'])} | "
                    f"{fmt(s['qps'])} | {fmt(s['mean_results'], 1)} | {fmt(s['gets_per_query'], 1)} | "
                    f"{fmt(s['block_reads_per_query'], 2)} |")
    return rows + ["", f"<sub>{r['_file']}</sub>", ""]


def cmd_report(scale):
    R = load_results()
    if not R:
        sys.exit(f"nenhum resultado em {RESULTS}")
    machine = next((r["machine"] for r in reversed(R) if "machine" in r), {})
    L = [
        f"# Resultados de performance (escala {scale})",
        "",
        f"Gerado por `python3 scripts/perf.py report --scale {scale}` a partir de `results/`. "
        "Cada seção mostra o resultado **mais recente** daquele tipo; as cargas mostram a **mediana** das "
        f"últimas rodadas do mesmo commit (até {MEDIAN_OF}), com a faixa mín–máx entre parênteses. O plano está em "
        f"[docs/plano-performance.md]({DOCS_URL}/plano-performance.md).",
        "",
        f"- Último resultado: `{R[-1]['_file']}`",
        f"- Máquina: {machine.get('cores', '?')} núcleos, {mb(machine.get('ram_total'))} MB de RAM, "
        f"kernel `{machine.get('kernel', '?')}`",
        "",
    ]

    # Fase 1
    L += ["## Fase 1: corretude", ""]
    correctness = latest(R, "correctness", scale)
    if correctness:
        L += ["| Checagem | Resultado |", "|---|---|"]
        for k, ok in correctness["checks"].items():
            L.append(f"| `{k}` | {'✅ ok' if ok else '❌ divergiu'} |")
        idempotence = correctness.get("idempotence", {})
        if idempotence:
            L += ["", f"Idempotência ({idempotence.get('slice')}): banco de {mb(idempotence.get('db_1st'))} MB após uma carga, "
                      f"{mb(idempotence.get('db_2nd'))} MB após duas e {mb(idempotence.get('db_2nd_compacted'))} MB depois de compactar."]
        L.append("")
    oracles = {}
    for r in R:
        if r.get("kind") == "oracle" and not r.get("label", "").startswith("pr-"):
            oracles[r["label"]] = r
    for label, r in oracles.items():
        L += [f"**Oráculo `{label}`** ({fmt(r['docs'])} docs, {fmt(r['oracle_terms'])} termos): "
              f"{'zero divergências' if r['divergences'] == 0 else str(r['divergences']) + ' divergências'}", "",
              "| Verificação | Casos | Divergências | Notas |", "|---|---|---|---|"]
        for c in r["checks"]:
            L.append(f"| {c['check']} | {fmt(c['cases'])} | {c['divergences']} | {'; '.join(c['notes'])} |")
        rel = r.get("relevance")
        if rel:
            L += ["", f"Relevância (título → artigo, {fmt(rel['queries'])} consultas): top-1 {pct_txt(rel['top1'])}, "
                      f"top-10 {pct_txt(rel['top10'])}, MRR {fmt(rel['mrr'], 3)}, fora do top-1000: {rel['outside_top1000']}."]
        L.append("")

    # Fase 2
    L += ["## Fase 2: escada de indexação", "",
          "| Fatia | Commit | Rodadas | Tempo | docs/s | MB/s | Pico RSS | Banco | Banco ÷ texto | Nós da Trie | Termos | Write amp | Stalls |",
          "|---|---|---|---|---|---|---|---|---|---|---|---|---|"]
    for s in ORDER:
        r = median_load(R, f"ptwiki-{s}")
        if not r:
            continue
        sz = latest(R, "sizes", f"ptwiki-{s}")
        terms = next((e["keys"] for e in sz["spaces"] if e["space"] == "post"), None) if sz else None
        w = r.get("write") or {}
        L.append(f"| {s} | `{r.get('commit', '?')}` | {r['_rounds']} | {dur(r['seconds'])} | {docs_s(r)} | {fmt(r['mb_per_s'], 1)} | "
                 f"{mb(r['peak_rss'])} MB | {mb(r['db_bytes'])} MB | {fmt(r['db_per_text'], 2)} | "
                 f"{fmt(r['trie_nodes'])} | {fmt(terms)} | {fmt(w.get('write_amp'), 2)} | "
                 f"{fmt((w.get('stall_micros') or 0) / 1e6, 1)}s |")
    L.append("")
    exp_slice = smaller(scale, "100k")
    prefix = f"exp-{exp_slice}-"
    exps = {}
    for r in R:
        if r.get("kind") == "load" and r.get("label", "").startswith(prefix):
            exps[r["label"][len(prefix):]] = median_load(R, r["label"])
    if exps:
        base = median_load(R, f"ptwiki-{exp_slice}")
        L += [f"### Experimentos ({exp_slice})", "", "| Experimento | docs/s | vs. padrão | Pico RSS | Banco |", "|---|---|---|---|---|"]
        if base:
            L.append(f"| padrão (lote {BATCH}) | {docs_s(base)} | — | {mb(base['peak_rss'])} MB | {mb(base['db_bytes'])} MB |")
        for label, r in exps.items():
            vs = f"{pct(r['docs_per_s'], base['docs_per_s']):+.0f}%" if base and "streaming" not in label else "—"
            L.append(f"| {label} | {docs_s(r)} | {vs} | {mb(r['peak_rss'])} MB | {mb(r['db_bytes'])} MB |")
        L.append("")
    sz = latest(R, "sizes", f"ptwiki-{scale}")
    if sz:
        L += [f"### Tamanhos ({scale})", "", "| Espaço | Chaves | Chaves (MB) | Valores (MB) |", "|---|---|---|---|"]
        for e in sz["spaces"]:
            L.append(f"| `{e['space']}:` | {fmt(e['keys'])} | {mb(e['key_bytes'])} | {mb(e['value_bytes'])} |")
        L += ["", f"Raiz (`node:0`): {fmt(sz['root_bytes'])} bytes. "
              + "Listas: " + ", ".join(f"\"{t}\" {fmt(b / 1024, 1)} KB" for t, b in sz.get("lists", {}).items()), ""]

    # Fase 4
    post = latest(R, "bench", f"{scale}-t1")
    L += [f"## Fase 4: latência de busca ({scale}, cache quente, 1 thread)", ""] + bench_table(post)
    pre = latest(R, "bench", f"{scale}-pre-compactacao")
    if pre and post:
        post_sets = {s["set"]: s for s in post["sets"]}
        L += ["### Merge pendente vs. compactado", "", "| Conjunto | p50 antes | p50 depois | p99 antes | p99 depois | merges/consulta antes |",
              "|---|---|---|---|---|---|"]
        for s in pre["sets"]:
            o = post_sets.get(s["set"])
            if o:
                L.append(f"| `{s['set']}` | {us(s['p50_us'])} | {us(o['p50_us'])} | {us(s['p99_us'])} | {us(o['p99_us'])} | "
                         f"{fmt(s.get('merges_per_query'), 1)} |")
        L.append("")
    comp = latest(R, "compact", scale)
    if comp:
        L += [f"Compactação: {dur(comp['seconds'])}, banco de {mb(comp['db_before'])} MB → {mb(comp['db_after'])} MB. "
              f"Abrir o banco + 1 busca: {fmt(comp['open_before']['median_s'] * 1000)} ms antes, "
              f"{fmt(comp['open_after']['median_s'] * 1000)} ms depois (mediana de {comp['open_after']['runs']}).", ""]
    c64, c1g = latest(R, "bench", f"{scale}-cache-64mb"), latest(R, "bench", f"{scale}-cache-1024mb")
    if (c64 or c1g) and post:
        post_sets = {s["set"]: s for s in post["sets"]}
        a = {s["set"]: s for s in (c64 or {}).get("sets", [])}
        b = {s["set"]: s for s in (c1g or {}).get("sets", [])}
        L += ["### Block cache (p50)", "",
              "O cache LRU se divide em 64 partes. Um bloco maior que uma parte (cache ÷ 64) não fica no cache: "
              "se o filtro de Bloom de um SST passar desse tamanho, ele é relido a cada `get` (veja block reads "
              "por consulta no JSON).", "",
              "| Conjunto | 64 MB | 256 MB | 1024 MB |", "|---|---|---|---|"]
        for name in a or b:
            L.append(f"| `{name}` | {us(a.get(name, {}).get('p50_us'))} | {us(post_sets.get(name, {}).get('p50_us'))} | "
                     f"{us(b.get(name, {}).get('p50_us'))} |")
        L.append("")
    cold = latest(R, "bench", f"{scale}-t1-frio")
    if cold:
        L += ["### Cache frio", ""] + bench_table(cold)

    # Fase 5
    scaling = [(t, latest(R, "bench", f"{scale}-mixed-t{t}")) for t in (1, 2, 4, 8, 12)]
    if any(r for _, r in scaling):
        L += ["## Fase 5: concorrência", "", "### Só leitura (`mixed`)", "", "| Threads | qps | ganho vs. 1 | p50 | p99 |", "|---|---|---|---|---|"]
        q1 = next((r["sets"][0]["qps"] for t, r in scaling if t == 1 and r), None)
        for t, r in scaling:
            if r:
                s = r["sets"][0]
                L.append(f"| {t} | {fmt(s['qps'])} | {fmt(s['qps'] / q1, 1) + '×' if q1 else '—'} | {us(s['p50_us'])} | {us(s['p99_us'])} |")
        L.append("")
    conc = [(t, latest(R, "concurrent", f"{scale}-mixed-t{t}")) for t in (1, 4, 8)]
    if any(r for _, r in conc):
        L += ["### Leitura durante escrita (carga dos últimos 10% da fatia)", "",
              "| Leitores | p50 sem escrita | p50 com escrita | p99 sem escrita | p99 com escrita | indexação docs/s | stalls |",
              "|---|---|---|---|---|---|---|"]
        for t, r in conc:
            if r:
                a, b = r["read_without_write"], r["read_with_write"]
                stall = ((r["write_after"] or {}).get("stall_micros", 0) - (r["write_before"] or {}).get("stall_micros", 0)) / 1e6
                L.append(f"| {t} | {us(a['p50_us'])} | {us(b['p50_us'])} | {us(a['p99_us'])} | {us(b['p99_us'])} | "
                         f"{fmt(r['indexing']['docs_per_s'])} | {fmt(stall, 1)}s |")
        L.append("")

    L += phrases_section(R, scale, post, oracles)
    L += long_load_section(R, scale)

    # Metas
    load_scale = median_load(R, f"ptwiki-{scale}")
    t1 = {s["set"]: s for s in (post or {}).get("sets", [])}
    c4 = latest(R, "concurrent", f"{scale}-mixed-t4")
    divs = sum(r["divergences"] for r in oracles.values()) if oracles else None
    swap_growth = (load_scale or {}).get("swap_growth")
    swap_txt = "—" if swap_growth is None else ("sim" if swap_growth < 64 << 20 else f"não (+{mb(swap_growth)} MB)")
    L += relevance_section(R, scale)
    L += stemming_section(R, scale)
    L += edits_section(R, scale)
    L += bindings_section(R, scale)
    L += [f"## Metas (linha de base em {scale})", "", "| Métrica | Linha de base | Meta |", "|---|---|---|",
          f"| Divergências do oráculo | {divs if divs is not None else '—'} | **0** |",
          f"| Carga sem swap (aumento < 64 MB) | {swap_txt} | **sim** |",
          f"| docs/s na carga de {scale} | {fmt(load_scale['docs_per_s']) if load_scale else '—'} | |",
          f"| Banco ÷ texto bruto | {fmt(load_scale['db_per_text'], 2) if load_scale else '—'} | |",
          f"| p99 de `titles` (quente) | {us(t1.get('titles', {}).get('p99_us'))} | |",
          f"| p99 de `t-freq` (quente) | {us(t1.get('t-freq', {}).get('p99_us'))} | |",
          f"| p99 de `mixed` com indexação em paralelo | {us(c4['read_with_write']['p99_us']) if c4 else '—'} | |",
          f"| Abrir o banco + 1 busca (após a carga) | {fmt(comp['open_before']['median_s'] * 1000) + ' ms' if comp else '—'} | |",
          f"| p50 de `rank-freq` ÷ p50 de `t-freq` | {ratio_txt(t1, 'rank-freq', 't-freq')} | **≤ 2×** |",
          f"| p50 de `phrase-2` | {us(t1.get('phrase-2', {}).get('p50_us'))} | |",
          ""]

    with open(REPORT, "w") as f:
        f.write("\n".join(L))
    print(f"relatório: {REPORT}")


def ratio_txt(sets, a, b):
    """p50 de `a` ÷ p50 de `b`, como "1,8×"."""
    pa, pb = sets.get(a, {}).get("p50_us"), sets.get(b, {}).get("p50_us")
    return f"{fmt(pa / pb, 2)}×" if pa and pb else "—"


def phrases_section(R, scale, post, oracles):
    """Fase E do plano de frases e ranking: custo do formato v2 e latência."""
    exp = smaller(scale, "100k")
    v1 = median_load(R, f"ptwiki-{scale}", fmt=1)
    v2 = median_load(R, f"ptwiki-{scale}", fmt=2)
    nopos = median_load(R, f"exp-{exp}-no-positions", fmt=2)
    if not v2:
        return []
    sets = {s["set"]: s for s in (post or {}).get("sets", [])}

    def delta(r):
        return f" ({pct(r['docs_per_s'], v1['docs_per_s']):+.0f}%)" if r and v1 else ""

    L = ["## Frases e ranking (formato v2)", "",
         f"Plano em [docs/plano-frases-e-ranking.md]({DOCS_URL}/plano-frases-e-ranking.md). A linha de base é a última "
         "carga no formato v1 (sem tf nem posições) com o mesmo rótulo.", "",
         "| Métrica | Linha de base (v1) | Agora | Meta |", "|---|---|---|---|",
         f"| Carga {scale} com posições | {docs_s(v1) + ' docs/s' if v1 else '—'} | {docs_s(v2)} docs/s{delta(v2)} | queda ≤ 30% |"]
    if nopos and exp == scale:
        L.append(f"| Carga {scale} sem posições | {docs_s(v1) + ' docs/s' if v1 else '—'} | {docs_s(nopos)} docs/s{delta(nopos)} | queda ≤ 10% |")
    # Logo após a carga: inclui o WAL ainda não compactado, varia entre rodadas.
    L += [f"| Banco ÷ texto logo após a carga, com posições | {fmt(v1['db_per_text'], 2) if v1 else '—'} | {fmt(v2['db_per_text'], 2)} | registrar |"]
    if nopos and exp == scale:
        L.append(f"| Banco ÷ texto logo após a carga, sem posições | {fmt(v1['db_per_text'], 2) if v1 else '—'} | {fmt(nopos['db_per_text'], 2)} | |")
    comp, comp_nopos = latest(R, "compact", scale), latest(R, "compact", f"{exp}-no-positions")
    v1_commits = {r.get("commit") for r in R if r.get("kind") == "load" and db_format(r) == 1}
    comp_v1 = next((r for r in reversed(R) if r.get("kind") == "compact" and r.get("label") == scale
                    and r.get("commit") in v1_commits), None)
    text = v2.get("bytes_corpus") or 1
    ratio = lambda c: fmt(c["db_after"] / text, 2) + f" ({mb(c['db_after'])} MB)" if c else "—"
    L += [f"| Banco compactado ÷ texto, com posições | {ratio(comp_v1)} | {ratio(comp)} | registrar |"]
    if comp_nopos and exp == scale:
        L.append(f"| Banco compactado ÷ texto, sem posições | {ratio(comp_v1)} | {ratio(comp_nopos)} | |")
    L += [f"| Pico de RSS na carga, com posições | {mb(v1['peak_rss']) + ' MB' if v1 else '—'} | {mb(v2['peak_rss'])} MB | |",
          f"| p50 de `rank-freq` ÷ p50 de `t-freq` | — | {ratio_txt(sets, 'rank-freq', 't-freq')} | ≤ 2× |",
          f"| p50 de `phrase-2` | — | {us(sets.get('phrase-2', {}).get('p50_us'))} | registrar |"]
    rel = next((r.get("relevance") for r in reversed(list(oracles.values())) if r.get("relevance")), None)
    if rel:
        L.append(f"| Título → artigo (MRR) | — | {fmt(rel['mrr'], 3)} (top-1 {pct_txt(rel['top1'])}, top-10 {pct_txt(rel['top10'])}) | registrar e acompanhar |")
    L.append("")

    with_pos, without = latest(R, "sizes", f"ptwiki-{scale}"), latest(R, "sizes", f"exp-{exp}-no-positions")
    if with_pos and with_pos.get("positions") is not None:
        a = {e["space"]: e for e in with_pos["spaces"]}
        b = {e["space"]: e for e in (without or {}).get("spaces", [])}
        L += [f"### Espaço por tipo de chave ({scale}, valores lógicos)", "",
              "| Espaço | Com posições (MB) | Sem posições (MB) |", "|---|---|---|"]
        for name in sorted(set(a) | set(b)):
            size = lambda e: mb(e["key_bytes"] + e["value_bytes"]) if e else "—"
            L.append(f"| `{name}:` | {size(a.get(name))} | {size(b.get(name))} |")
        L.append("")

    pairs = [("t-freq", "rank-freq"), ("and-2", "rank-and"), ("titles", "rank-titles")]
    if any(r in sets for _, r in pairs):
        L += [f"### Ranking: top 10 vs. todos os IDs ({scale}, p50)", "",
              "| Consulta | Sem ranking | Top 10 (BM25) | Razão |", "|---|---|---|---|"]
        for plain, ranked in pairs:
            if ranked in sets:
                L.append(f"| `{plain}` / `{ranked}` | {us(sets.get(plain, {}).get('p50_us'))} | "
                         f"{us(sets[ranked]['p50_us'])} | {ratio_txt(sets, ranked, plain)} |")
        L.append("")
    phrase = [s for s in ("phrase-2", "phrase-3", "phrase-stop", "phrase-freq", "phrase-miss") if s in sets]
    if phrase:
        L += [f"### Frases ({scale})", "", "| Conjunto | p50 | p95 | p99 | resultados (média) | Gets/consulta |", "|---|---|---|---|---|---|"]
        for name in phrase:
            x = sets[name]
            L.append(f"| `{name}` | {us(x['p50_us'])} | {us(x['p95_us'])} | {us(x['p99_us'])} | "
                     f"{fmt(x['mean_results'], 1)} | {fmt(x['gets_per_query'], 1)} |")
        L.append("")
    return L


# ------------------------------------------------------------------------ main

def main():
    global REPETITIONS
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--scale", choices=ORDER, default="10k",
                        help="maior fatia do corpus usada (padrão: 10k)")
    common.add_argument("--repetitions", type=int, default=REPETITIONS,
                        help=f"rodadas de cada carga de até {REPEAT_UP_TO_DOCS} docs (padrão: {REPETITIONS})")
    p = argparse.ArgumentParser(description="Plano de performance do fire-flower")
    sub = p.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("ladder", parents=[common], help="Fase 2")
    e.add_argument("--experiments", action="store_true")
    for name, help_ in (("correctness", "Fase 1"), ("queries", "Fase 3"), ("latency", "Fase 4"),
                        ("concurrency", "Fase 5"), ("phrases", "frases e ranking (Fase E do plano)"),
                        ("relevance", "ordenação: consultas de resposta conhecida"),
                        ("stemming", "stemming ligado vs. desligado (A/B no mesmo binário)"),
                        ("edits", "edição e remoção: oráculo, vazão, latência antes e depois de compactar"),
                        ("bindings", "binding Node contra o nativo (latência, leitores, carga)"),
                        ("report", "gera RESULTADOS.md no repositório de resultados")):
        sub.add_parser(name, parents=[common], help=help_)
    pr = sub.add_parser("pr", parents=[common], help="Fase 8")
    pr.add_argument("--save-baseline", action="store_true")
    cl = sub.add_parser("long-load", parents=[common], help="cenário do colapso da carga (padrão: escala full)")
    cl.add_argument("--reproduce", action="store_true", help="roda antes o cenário com filtro inteiro, que tem que colapsar")
    c = sub.add_parser("compare", help="compara dois resultados de bench")
    c.add_argument("a")
    c.add_argument("b")
    t = sub.add_parser("all", parents=[common], help="todas as fases, na ordem")
    t.add_argument("--no-concurrency", action="store_true", help="pula a Fase 5")
    args = p.parse_args()

    if args.cmd == "compare":
        return cmd_compare(args)
    if args.cmd == "report":
        return cmd_report(args.scale)
    REPETITIONS = max(1, args.repetitions)
    scale = args.scale
    if args.cmd == "long-load" and "--scale" not in sys.argv:
        scale = "full"  # o colapso só aparece em cargas grandes
    build()
    started = time.time()
    ok = True
    if args.cmd == "all":
        phase_ladder(scale, experiments=True)
        ok &= phase_correctness(scale)
        phase_relevance(scale)
        phase_queries(scale)
        phase_latency(scale)
        if not args.no_concurrency:
            phase_concurrency(scale)
        ok &= phase_pr(scale, save_baseline=True)
    elif args.cmd == "ladder":
        phase_ladder(scale, args.experiments)
    elif args.cmd == "pr":
        ok = phase_pr(scale, args.save_baseline)
    elif args.cmd == "long-load":
        ok = phase_long_load(scale, args.reproduce)
    else:
        result = {"correctness": phase_correctness, "queries": phase_queries, "latency": phase_latency,
                  "concurrency": phase_concurrency, "phrases": phase_phrases,
                  "relevance": phase_relevance, "stemming": phase_stemming, "edits": phase_edits,
                  "bindings": phase_bindings}[args.cmd](scale)
        ok = result is not False
    cmd_report(scale)
    status = f"{GREEN}pronto" if ok else f"{RED}terminou com falhas"
    print(f"\n{status} em {dur(time.time() - started)}{RESET}")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
