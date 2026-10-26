#!/usr/bin/env python3
"""Roda o plano de performance (docs/plano-performance.md) e gera o relatório.

    python3 scripts/perf.py tudo              # todas as fases na escala 10k (~5 min)
    python3 scripts/perf.py tudo --escala 100k
    python3 scripts/perf.py escada            # Fase 2: carga 1k → … → escala   [--experimentos]
    python3 scripts/perf.py corretude         # Fase 1: oráculo, idempotência, lotes vs. único, reabrir
    python3 scripts/perf.py consultas         # Fase 3: vocabulário + conjuntos de consulta
    python3 scripts/perf.py latencia          # Fase 4: latência por conjunto, merge pendente vs. compactado
    python3 scripts/perf.py concorrencia      # Fase 5: escala com threads e leitura durante escrita
    python3 scripts/perf.py pr                # Fase 8: checagem rápida; falha se o p50 piorar >10%
    python3 scripts/perf.py frases            # frases e ranking: carga com/sem posições, oráculo, latência
    python3 scripts/perf.py relevancia        # ordenação: consultas de resposta conhecida (docs/plano-relevancia.md)
    python3 scripts/perf.py carga-longa       # carga inteira janela a janela; falha se a velocidade colapsar
                                              #   [--reproduzir: roda antes o cenário que colapsava]
    python3 scripts/perf.py stemming          # stemming ligado vs. desligado (A/B no mesmo binário)
    python3 scripts/perf.py relatorio         # escreve RESULTADOS.md no repositório de resultados
    python3 scripts/perf.py comparar A.json B.json

`--escala` (1k, 10k, 100k ou full; padrão 10k) define a maior fatia do corpus
usada. A Wikipédia inteira (full) leva mais de uma hora; comece pequeno.

Cargas de até 10k docs rodam `--repeticoes` vezes (padrão 3) e o relatório usa
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
REPETIR_ATE_DOCS = 10_000
REPETICOES = 3  # --repeticoes
# Quantas rodadas recentes (do mesmo commit e rótulo) entram na mediana.
MEDIANA_DE = 5

# Artigos por commit nas cargas. Era 1000 até 26/10/2026; com 10 mil a carga
# inteira cai de 242 s para ~115 s (docs/plano-performance.md, P2).
LOTE = 10000

# Conjuntos do `pr` (comparados com a linha de base) e das frases/ranking.
PR_SETS = ["t-medio", "t-freq", "p-2", "rank-freq", "frase-2"]
FRASE_SETS = ["t-freq", "rank-freq", "and-2", "rank-and", "titulos", "rank-titulos",
              "frase-2", "frase-3", "frase-stop", "frase-freq", "frase-miss"]

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
    run(["cargo", "build", "--release", "--bin", "fire-flower", "--example", "wiki"])


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
    return REPETICOES if docs_in(slice_name) <= REPETIR_ATE_DOCS else 1


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
        wiki("load", "--db", path, "--corpus", corpus(slice_name), "--batch", LOTE, "--label", f"ptwiki-{slice_name}")
    return path


def git_commit():
    def git(*a):
        return subprocess.run(["git", "-C", PROJECT, *a], capture_output=True, text=True).stdout.strip()
    h = git("rev-parse", "--short", "HEAD") or "desconhecido"
    # bench/ fica de fora: o próprio perf.py regrava consultas, baseline e relatório no meio da rodada.
    return h + ("-modificado" if git("status", "--porcelain", "--untracked-files=no", "--", ".", ":!bench") else "")


def save_result(kind, label, data):
    os.makedirs(RESULTS, exist_ok=True)
    ts = datetime.datetime.utcnow().strftime("%Y-%m-%dT%H-%M-%S")
    commit = git_commit()
    data = dict(data, tipo=kind, rotulo=label, commit=commit, quando=ts)
    path = os.path.join(RESULTS, f"{ts}-{commit}-{kind}-{label}.json")
    with open(path, "w") as f:
        json.dump(data, f, indent=2, ensure_ascii=False)
    print(f"{DIM}resultado: {path}{RESET}")
    return path


def load_results():
    out = []
    for path in sorted(glob.glob(os.path.join(RESULTS, "*.json"))):
        try:
            with open(path) as f:
                r = json.load(f)
            r["_arquivo"] = os.path.basename(path)
            out.append(r)
        except (OSError, ValueError):
            pass
    return out


def latest(results, kind, label=None):
    """Resultado mais recente de um tipo (e rótulo). Os arquivos começam pela data."""
    found = [r for r in results if r.get("tipo") == kind and (label is None or r.get("rotulo") == label)]
    return found[-1] if found else None


def formato(r):
    """Formato do banco de um resultado de carga: 1 antes de tf e posições.
    Até 26/10/2026 o `wiki load` gravava 2 para qualquer formato com tf."""
    return r.get("formato", 1)


def median_load(results, label, fmt=None):
    """Carga mediana (por docs/s) entre as últimas MEDIANA_DE rodadas com o
    rótulo, todas do mesmo commit da mais recente. `fmt` restringe ao formato
    do banco: 1 (sem tf nem posições) ou 2 (qualquer formato com tf, 2 em
    diante). Ganha `_rodadas`, `_min` e `_max`."""
    results = [r for r in results if fmt is None or r.get("tipo") != "load" or min(formato(r), 2) == fmt]
    last = latest(results, "load", label)
    if not last:
        return None
    runs = [r for r in results if r.get("tipo") == "load" and r.get("rotulo") == label
            and r.get("commit") == last.get("commit")][-MEDIANA_DE:]
    runs.sort(key=lambda r: r["docs_por_s"])
    mid = dict(runs[(len(runs) - 1) // 2])
    mid.update(_rodadas=len(runs), _min=runs[0]["docs_por_s"], _max=runs[-1]["docs_por_s"])
    return mid


def docs_s(r):
    """docs/s com a faixa entre rodadas, quando houver mais de uma."""
    if r.get("_rodadas", 1) > 1:
        return f"{fmt(r['docs_por_s'])} ({fmt(r['_min'])}–{fmt(r['_max'])})"
    return fmt(r["docs_por_s"])


def can_drop_caches():
    return subprocess.run(["sudo", "-n", "true"], capture_output=True).returncode == 0


# ---------------------------------------------------------------------- fases

def fase_escada(scale, experimentos=False):
    """Fase 2: carga de cada fatia, até a escala, num banco novo; mais tamanhos."""
    for s in ORDER[: ORDER.index(scale) + 1]:
        step(f"escada: carga {s}")
        path = db(f"ptwiki-{s}")
        load_rounds(path, s, f"ptwiki-{s}", "--batch", LOTE)
        wiki("sizes", "--db", path)
    if experimentos:
        fase_experimentos(scale)


def fase_experimentos(scale):
    """Experimentos da Fase 2, cada um num banco temporário (o plano usa 100k)."""
    s = smaller(scale, "100k")
    n = docs_in(s)
    tmp = db("tmp-exp")
    runs = [(f"lote-{b}", ["--batch", b]) for b in (100, 1000, 5000, 50000) if b <= n]
    runs += [("ids-remapeados", ["--batch", LOTE, "--remap-ids"]),
             ("cache-64mb", ["--batch", LOTE, "--cache-mb", 64]),
             ("cache-1024mb", ["--batch", LOTE, "--cache-mb", 1024]),
             ("sem-cache-de-nos", ["--batch", LOTE, "--cache-nos", 0]),
             ("sem-posicoes", ["--batch", LOTE, "--sem-posicoes"])]
    for label, extra in runs:
        step(f"experimento {s}: {label}")
        load_rounds(tmp, s, f"exp-{s}-{label}", *extra)
        if label in ("ids-remapeados", "sem-posicoes"):
            sizes_of(tmp, f"exp-{s}-{label}")
        if label == "sem-posicoes":
            compact_and_save(tmp, f"{s}-sem-posicoes")
    stream = smaller(s, "10k") if s != "10k" else "1k"
    step(f"experimento: streaming (1 commit por documento, {stream})")
    fresh(tmp)
    wiki("load", "--db", tmp, "--corpus", corpus(stream), "--streaming", "--label", f"exp-{s}-streaming-{stream}")
    shutil.rmtree(tmp, ignore_errors=True)


def compact_and_save(path, label):
    """Compacta e grava o tamanho antes e depois: o tamanho logo após a carga
    depende de quanto do WAL já virou SST, o compactado é estável."""
    before = dir_size(path)
    t = time.time()
    run([FF, "compact", path])
    save_result("compact", label, {"segundos": time.time() - t, "banco_antes": before, "banco_depois": dir_size(path)})


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


def fase_corretude(scale):
    """Fase 1: oráculo e checagens de conteúdo lógico."""
    checks = {}
    tmp = db("tmp-corretude")
    small = smaller(scale, "10k")

    step(f"oráculo: {small}, todos os termos")
    path_small = ensure_loaded(small)
    checks[f"oraculo_{small}"] = wiki("oracle", "--db", path_small, "--corpus", corpus(small),
                                      "--label", f"{small}-todos", check=False) == 0

    step(f"idempotência: indexar a fatia de {small} duas vezes")
    copy_db(path_small, tmp)
    before = digest(tmp)
    size_before = dir_size(tmp)
    wiki("load", "--db", tmp, "--corpus", corpus(small), "--label", f"idempotencia-{small}")
    after = digest(tmp)
    size_twice = dir_size(tmp)
    run([FF, "compact", tmp])
    checks["idempotencia"] = before["digest"] == after["digest"]
    idem = {"fatia": small, "digest_1a": before, "digest_2a": after, "banco_1a": size_before,
            "banco_2a": size_twice, "banco_2a_compactado": dir_size(tmp)}

    step("lotes vs. único: 1k com --batch 1 e com --batch 1000")
    a, b = db("tmp-lote-1"), db("tmp-lote-1000")
    for path, batch in ((a, 1), (b, 1000)):
        fresh(path)
        wiki("load", "--db", path, "--corpus", corpus("1k"), "--batch", batch, "--label", f"lote-{batch}-1k")
    da, dbb = digest(a), digest(b)
    checks["lotes_vs_unico"] = da["digest"] == dbb["digest"]
    for p in (a, b):
        shutil.rmtree(p, ignore_errors=True)

    big = smaller(scale, "100k")
    if big != small:
        step(f"oráculo: {big}, amostra de 50 mil termos")
        checks[f"oraculo_{big}"] = wiki("oracle", "--db", ensure_loaded(big), "--corpus", corpus(big),
                                        "--sample-terms", 50000, "--label", f"{big}-amostra", check=False) == 0

    n = docs_in(big)
    extra = max(100, n // 100)
    step(f"reabrir: copiar {big}, carregar mais {extra} docs e conferir {n + extra}")
    copy_db(ensure_loaded(big), tmp)
    wiki("load", "--db", tmp, "--corpus", corpus("full"), "--skip", n, "--take", extra, "--label", f"reabrir-{big}")
    checks["reabrir"] = wiki("oracle", "--db", tmp, "--corpus", corpus("full"), "--take", n + extra,
                             "--sample-terms", 20000, "--label", f"{big}-reaberto", check=False) == 0
    shutil.rmtree(tmp, ignore_errors=True)

    save_result("corretude", scale, {"ok": all(checks.values()), "checagens": checks,
                                      "idempotencia": idem, "lotes": {"batch_1": da, "batch_1000": dbb}})
    for name, ok in checks.items():
        print(f"  {GREEN + '✔' if ok else RED + '✗'}{RESET} {name}")
    return all(checks.values())


def fase_consultas(scale):
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
    if not os.path.isdir(qdir) or not all(os.path.exists(f) for f in files) or not os.path.exists(os.path.join(qdir, "rank-freq.txt")):
        fase_consultas(scale)
    wiki("bench", "--db", path, "--queries", *files, "--threads", threads, "--label", label, *extra)


def open_time(path, runs=5):
    """Tempo de abrir o banco e fazer uma busca que não acha nada."""
    times = []
    for _ in range(runs):
        t = time.time()
        subprocess.run([FF, "search", path, "xyzzyinexistente"], stdout=subprocess.DEVNULL, check=True)
        times.append(time.time() - t)
    return {"mediana_s": statistics.median(times), "min_s": min(times), "max_s": max(times), "execucoes": runs}


def fase_latencia(scale):
    """Fase 4: latência no banco da escala. Recarrega o banco para medir o merge pendente."""
    path = db(f"ptwiki-{scale}")
    step(f"carga {scale} limpa (para medir antes da compactação)")
    fresh(path)
    wiki("load", "--db", path, "--corpus", corpus(scale), "--batch", LOTE, "--label", f"ptwiki-{scale}")

    step("merge pendente: termos, títulos, frases e ranking antes da compactação")
    pre_open = open_time(path)
    bench(path, scale, ["t-freq", "titulos", "and-freq", "rank-freq", "frase-2", "frase-freq"], f"{scale}-pre-compactacao")

    step("compactação")
    size_before = dir_size(path)
    t = time.time()
    run([FF, "compact", path])
    compact_s = time.time() - t
    save_result("compact", scale, {"segundos": compact_s, "banco_antes": size_before, "banco_depois": dir_size(path),
                                   "abertura_antes": pre_open, "abertura_depois": open_time(path)})
    sizes_of(path, f"ptwiki-{scale}-compactado")

    step("latência de todos os conjuntos (cache quente)")
    bench(path, scale, None, f"{scale}-t1")

    if can_drop_caches():
        step("latência com cache frio")
        bench(path, scale, ["t-raro", "t-medio", "t-freq", "titulos", "misto"], f"{scale}-t1-frio", extra=["--cold"])
    else:
        print(f"{DIM}pulando cache frio: precisa de `sudo -n` (rode `sudo -v` antes, se quiser){RESET}")

    for mb_ in (64, 1024):
        step(f"block cache de {mb_} MB")
        bench(path, scale, ["t-medio", "titulos", "misto", "p-2"], f"{scale}-cache-{mb_}mb", extra=["--cache-mb", mb_])


def fase_concorrencia(scale):
    """Fase 5: escala com threads e leitura durante escrita (os últimos 10% da fatia)."""
    path = ensure_loaded(scale)
    for t in (1, 2, 4, 8, 12):
        step(f"só leitura: misto com {t} thread(s)")
        bench(path, scale, ["misto"], f"{scale}-misto-t{t}", threads=t)

    n = docs_in(scale)
    base_n = n * 9 // 10
    base = db(f"ptwiki-{scale}-90")
    if not usable(base):
        step(f"carga dos primeiros {base_n} artigos (base da leitura durante escrita)")
        fresh(base)
        wiki("load", "--db", base, "--corpus", corpus(scale), "--take", base_n, "--label", f"ptwiki-{scale}-90")
    tmp = db("tmp-concorrencia")
    baseline = 5 if n <= 10000 else 15
    for t in (1, 4, 8):
        step(f"leitura durante escrita: {t} leitor(es) + carga dos {n - base_n} restantes")
        copy_db(base, tmp)
        wiki("concurrent", "--db", tmp, "--corpus", corpus(scale), "--skip", base_n,
             "--queries", os.path.join(queries_dir(scale), "misto.txt"), "--threads", t,
             "--baseline-secs", baseline, "--label", f"{scale}-misto-t{t}")
    shutil.rmtree(tmp, ignore_errors=True)


def fase_pr(scale, salvar_baseline=False):
    """Fase 8: oráculo + carga + bench rápido, comparado com a linha de base salva."""
    small = smaller(scale, "10k")
    baseline_path = os.path.join(BENCH_DIR, f"baseline-pr-{scale}.json")
    tmp_small, tmp_big = db("tmp-pr-oraculo"), db("tmp-pr-carga")
    step(f"pr: oráculo {small}")
    fresh(tmp_small)
    wiki("load", "--db", tmp_small, "--corpus", corpus(small), "--label", f"pr-oraculo-{small}")
    ok = wiki("oracle", "--db", tmp_small, "--corpus", corpus(small), "--label", f"pr-{small}", check=False) == 0
    step(f"pr: carga {scale} + bench")
    load_rounds(tmp_big, scale, f"pr-{scale}")
    bench(tmp_big, scale, PR_SETS, f"pr-{scale}")
    for p in (tmp_small, tmp_big):
        shutil.rmtree(p, ignore_errors=True)

    results = load_results()
    current = latest(results, "bench", f"pr-{scale}")
    load_now = median_load(results, f"pr-{scale}")
    if salvar_baseline or not os.path.exists(baseline_path):
        archive_baseline(baseline_path, current)
        with open(baseline_path, "w") as f:
            json.dump({"bench": current, "load": load_now}, f, indent=2, ensure_ascii=False)
        print(f"linha de base salva em {baseline_path}")
        return ok
    with open(baseline_path) as f:
        base = json.load(f)
    worse = compare_bench(base["bench"], current)
    docs_base, docs_now = base["load"]["docs_por_s"], load_now["docs_por_s"]
    print(f"\ncarga {scale}: {docs_base:.0f} → {docs_now:.0f} docs/s ({pct(docs_now, docs_base):+.1f}%, "
          f"mediana de {load_now.get('_rodadas', 1)})")
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


def fase_frases(scale):
    """Plano de frases e ranking, Fase E: carga com e sem posições, oráculo
    (tf, posições, frases, BM25 e título → artigo) e latência dos conjuntos
    novos, antes e depois de compactar."""
    small = smaller(scale, "10k")
    path = db(f"ptwiki-{scale}")
    step(f"carga {scale} com posições")
    load_rounds(path, scale, f"ptwiki-{scale}", "--batch", LOTE)
    sizes_of(path, f"ptwiki-{scale}")
    tmp = db("tmp-sem-posicoes")
    step(f"carga {scale} sem posições")
    load_rounds(tmp, scale, f"exp-{smaller(scale, '100k')}-sem-posicoes", "--batch", LOTE, "--sem-posicoes")
    sizes_of(tmp, f"exp-{smaller(scale, '100k')}-sem-posicoes")
    compact_and_save(tmp, f"{smaller(scale, '100k')}-sem-posicoes")
    shutil.rmtree(tmp, ignore_errors=True)
    step(f"oráculo {small} (tf, posições, frases, ranking, título → artigo)")
    ok = wiki("oracle", "--db", ensure_loaded(small), "--corpus", corpus(small), "--label", f"{small}-todos", check=False) == 0
    step("latência: frases e ranking antes da compactação")
    bench(path, scale, FRASE_SETS, f"{scale}-pre-compactacao")
    step("compactação")
    pre_open = open_time(path)
    size_before = dir_size(path)
    t = time.time()
    run([FF, "compact", path])
    save_result("compact", scale, {"segundos": time.time() - t, "banco_antes": size_before, "banco_depois": dir_size(path),
                                   "abertura_antes": pre_open, "abertura_depois": open_time(path)})
    sizes_of(path, f"ptwiki-{scale}-compactado")
    step("latência de todos os conjuntos (cache quente)")
    bench(path, scale, None, f"{scale}-t1")
    return ok


# Conjuntos medidos antes da compactação (merge pendente), como na Fase 4.
PRE_SETS = ["t-freq", "titulos", "and-freq", "rank-freq", "frase-2", "frase-freq"]
STEM_MODES = ("nenhum", "pt")


def fase_stemming(scale):
    """Stemming ligado vs. desligado, no mesmo binário: oráculo com stemming,
    carga, tamanho, latência antes e depois de compactar e relevância. As
    consultas são as mesmas (palavras do vocabulário sem stemming): com
    stemming, cada uma casa a união das flexões, que é o custo real dele."""
    small = smaller(scale, "10k")
    oracle_db = db(f"stemming-{small}-oraculo")
    step(f"oráculo {small} com stemming (tf, posições, frases, prefixos, ranking)")
    fresh(oracle_db)
    wiki("load", "--db", oracle_db, "--corpus", corpus(small), "--batch", LOTE, "--stemming", "pt",
         "--label", f"stemming-{small}-oraculo")
    ok = wiki("oracle", "--db", oracle_db, "--corpus", corpus(small), "--label", f"{small}-stemming", check=False) == 0
    shutil.rmtree(oracle_db, ignore_errors=True)

    paths = {mode: db(f"stemming-{scale}-{mode}") for mode in STEM_MODES}
    for mode, path in paths.items():
        step(f"carga {scale}, stemming {mode}")
        load_rounds(path, scale, f"stemming-{scale}-{mode}", "--batch", LOTE, "--stemming", mode)
    for mode, path in paths.items():
        step(f"antes da compactação, stemming {mode}")
        bench(path, scale, PRE_SETS, f"stemming-{scale}-{mode}-pre-compactacao")
    for mode, path in paths.items():
        step(f"compactação, stemming {mode}")
        compact_and_save(path, f"stemming-{scale}-{mode}")
        sizes_of(path, f"stemming-{scale}-{mode}-compactado")
    for mode, path in paths.items():
        step(f"latência de todos os conjuntos, stemming {mode}")
        bench(path, scale, None, f"stemming-{scale}-{mode}-t1")

    rel = smaller(scale, "100k")
    sets = os.path.join(PROJECT, "bench", "relevancia", rel)
    for mode in STEM_MODES:
        path = paths[mode] if rel == scale else db(f"stemming-{rel}-{mode}")
        if rel != scale:
            step(f"carga {rel} para a relevância, stemming {mode}")
            fresh(path)
            wiki("load", "--db", path, "--corpus", corpus(rel), "--batch", LOTE, "--stemming", mode,
                 "--label", f"stemming-{rel}-{mode}-relevancia")
        step(f"relevância {rel}, stemming {mode}")
        wiki("relevancia", "--db", path, "--corpus", corpus(rel), "--sets", sets, "--label", f"{rel}-stemming-{mode}")
    return ok


def stemming_section(R, scale):
    loads = {m: median_load(R, f"stemming-{scale}-{m}") for m in STEM_MODES}
    if not all(loads.values()):
        return []
    sizes = {m: latest(R, "sizes", f"stemming-{scale}-{m}-compactado") for m in STEM_MODES}
    comps = {m: latest(R, "compact", f"stemming-{scale}-{m}") for m in STEM_MODES}
    oracle = latest(R, "oracle", f"{smaller(scale, '10k')}-stemming")
    keys = lambda r, space: next((e["chaves"] for e in (r or {}).get("espacos", []) if e["espaco"] == space), None)
    space_bytes = lambda r, space: next((e["bytes_chaves"] + e["bytes_valores"] for e in (r or {}).get("espacos", [])
                                         if e["espaco"] == space), None)
    off, on = loads["nenhum"], loads["pt"]

    def row(name, a, b, f=fmt, lower_better=True):
        delta = f" ({pct(b, a):+.0f}%)" if a and b else ""
        return f"| {name} | {f(a) if a is not None else '—'} | {f(b) if b is not None else '—'}{delta} |"

    L = [f"## Stemming ({scale})", "",
         "Stemming Snowball (`--stemming pt`) contra o mesmo binário sem stemming, com as mesmas consultas "
         "(palavras do vocabulário sem stemming: com stemming, cada uma casa a união das flexões).", ""]
    if oracle:
        L += [f"- Oráculo com stemming ({oracle.get('docs')} docs, {fmt(oracle.get('termos_oraculo'))} termos): "
              f"**{oracle['divergencias']} divergências**.", ""]
    L += ["| | sem stemming | com stemming |", "|---|---|---|",
          row("Carga (s)", off["segundos"], on["segundos"], lambda v: fmt(v, 1)),
          row("docs/s", off["docs_por_s"], on["docs_por_s"]),
          row("Pico de RSS (MB)", off["pico_rss"], on["pico_rss"], mb),
          row("Termos distintos", keys(sizes["nenhum"], "post"), keys(sizes["pt"], "post")),
          row("Nós da Trie", off.get("nos_trie"), on.get("nos_trie")),
          row("Banco compactado (MB)", (comps["nenhum"] or {}).get("banco_depois"), (comps["pt"] or {}).get("banco_depois"), mb),
          row("`post:` (MB)", space_bytes(sizes["nenhum"], "post"), space_bytes(sizes["pt"], "post"), mb),
          row("`pos:` (MB)", space_bytes(sizes["nenhum"], "pos"), space_bytes(sizes["pt"], "pos"), mb),
          row("Compactação (s)", (comps["nenhum"] or {}).get("segundos"), (comps["pt"] or {}).get("segundos"), lambda v: fmt(v, 1)),
          ""]
    for title, suffix in (("Latência (cache quente, compactado)", "t1"), ("Antes da compactação", "pre-compactacao")):
        a, b = (latest(R, "bench", f"stemming-{scale}-{m}-{suffix}") for m in STEM_MODES)
        if not (a and b):
            continue
        sets_a = {x["conjunto"]: x for x in a["conjuntos"]}
        L += [f"### {title}", "",
              "| Conjunto | p50 sem | p50 com | Δ | p99 sem | p99 com | Δ | resultados sem → com |",
              "|---|---|---|---|---|---|---|---|"]
        for x in b["conjuntos"]:
            o = sets_a.get(x["conjunto"])
            if not o:
                continue
            L.append(f"| `{x['conjunto']}` | {us(o['p50_us'])} | {us(x['p50_us'])} | {pct(x['p50_us'], o['p50_us']):+.0f}% | "
                     f"{us(o['p99_us'])} | {us(x['p99_us'])} | {pct(x['p99_us'], o['p99_us']):+.0f}% | "
                     f"{fmt(o['resultados_media'], 1)} → {fmt(x['resultados_media'], 1)} |")
        L += ["", f"<sub>{a['_arquivo']} · {b['_arquivo']}</sub>", ""]
    rel = smaller(scale, "100k")
    ra, rb = (latest(R, "relevancia", f"{rel}-stemming-{m}") for m in STEM_MODES)
    if ra and rb:
        sets_a = {c["conjunto"]: c for c in ra["conjuntos"]}
        L += [f"### Relevância ({rel})", "",
              "| Conjunto | top-1 sem → com | top-3 sem → com | top-10 sem → com | MRR sem → com |", "|---|---|---|---|---|"]
        for c in rb["conjuntos"]:
            o = sets_a.get(c["conjunto"])
            if not o:
                continue
            cell = lambda k: f"{fmt(o[k] * 100, 1)}% → {fmt(c[k] * 100, 1)}%"
            L.append(f"| `{c['conjunto']}` | {cell('top1')} | {cell('top3')} | {cell('top10')} | "
                     f"{fmt(o['mrr'], 3)} → {fmt(c['mrr'], 3)} |")
        L += ["", f"<sub>{ra['_arquivo']} · {rb['_arquivo']}</sub>", ""]
    return L


def fase_relevancia(scale):
    """Plano de relevância: top-1/top-3/top-10/MRR em conjuntos com resposta
    conhecida, gerados da fatia (até 100k) e versionados em bench/relevancia/."""
    s = smaller(scale, "100k")
    step(f"relevância {s}: título, digitando, hub, corpo e consultas à mão")
    sets = os.path.join(PROJECT, "bench", "relevancia", s)
    wiki("relevancia", "--db", ensure_loaded(s), "--corpus", corpus(s), "--sets", sets, "--label", s)


def relevancia_section(R, scale):
    s = smaller(scale, "100k")
    rel = latest(R, "relevancia", s)
    if not rel:
        return []
    base = latest(R, "relevancia", f"linha-de-base-{s}")
    before = {c["conjunto"]: c for c in base["conjuntos"]} if base else {}
    L = [f"## Relevância ({s})", "",
         f"Consultas com resposta conhecida ([docs/plano-relevancia.md]({DOCS_URL}/plano-relevancia.md)). "
         "Acerto: top-1 em título, digitando e hub; top-10 em corpo; top-3 nas consultas à mão. "
         "Entre parênteses, a linha de base (ranking antes do plano).", "",
         "| Conjunto | Consultas | top-1 | top-3 | top-10 | MRR | fora do top-1000 |", "|---|---|---|---|---|---|---|"]
    for c in rel["conjuntos"]:
        b = before.get(c["conjunto"])
        def cell(key, pctg=True):
            v = c[key]
            txt = f"{fmt(v * 100, 1)}%" if pctg else fmt(v, 3)
            if b:
                txt += f" ({fmt(b[key] * 100, 1)}%)" if pctg else f" ({fmt(b[key], 3)})"
            return txt
        L.append(f"| `{c['conjunto']}` | {c['consultas']} | {cell('top1')} | {cell('top3')} | {cell('top10')} | "
                 f"{cell('mrr', False)} | {c['fora_do_top1000']} |")
    L.append("")
    erros = [(c["conjunto"], e) for c in rel["conjuntos"] for e in c.get("erros", [])[:3]]
    if erros:
        L += ["Exemplos de erro (até 3 por conjunto):", "",
              "| Conjunto | Consulta | Esperado | Posição | Os 3 primeiros |", "|---|---|---|---|---|"]
        for nome, e in erros:
            pos = f"{e['posicao']}º" if e.get("posicao") else "—"
            L.append(f"| `{nome}` | `{e['consulta']}` | {e['esperado']} | {pos} | {' · '.join(e['top3'])} |")
        L.append("")
    L.append(f"<sub>{rel['_arquivo']}</sub>")
    L.append("")
    return L


def fase_carga_longa(scale, reproduzir=False):
    """Cenário do colapso da carga: a velocidade caía de ~5.000 para ~200
    artigos/s em pontos diferentes (200k, 500k, 600k) da Wikipédia inteira.
    Causa: filtros de Bloom de um bloco só, maiores que uma parte do block
    cache (ver `bloom_filters_larger_than_...` em tests/engine.rs). O `wiki
    load` acompanha a carga em janelas de 10k artigos e acusa a primeira cuja
    velocidade (MB/s) cai abaixo de 20% da mediana das anteriores.

    - estresse: 200k artigos com cache de 32 MB (partes de 512 KB), onde o
      problema aparecia já com ~60k artigos;
    - padrão: a fatia da escala, com a config padrão, como o
      `carregar_wikipedia.py` (lote de 2000);
    - com `--reproduzir`, antes, o estresse com filtro inteiro, que tem que
      colapsar (confere que o cenário e o detector funcionam)."""
    tmp = db("tmp-carga-longa")
    runs = [("estresse-cache-32mb", "full", ["--take", 200000, "--cache-mb", 32], False),
            (f"padrao-{scale}", scale, [], False)]
    if reproduzir:
        runs.insert(0, ("estresse-filtro-inteiro", "full", ["--take", 200000, "--cache-mb", 32, "--filtro-inteiro"], True))
    ok, summary = True, []
    for name, slice_name, extra, expect_collapse in runs:
        label = f"carga-longa-{name}"
        step(f"carga longa: {name}" + (" (tem que colapsar)" if expect_collapse else ""))
        fresh(tmp)
        wiki("load", "--db", tmp, "--corpus", corpus(slice_name), "--batch", LOTE, *extra, "--label", label)
        r = latest(load_results(), "load", label)
        collapse = r.get("colapso")
        good = bool(collapse) == expect_collapse
        ok &= good
        summary.append({"cenario": name, "rotulo": label, "esperava_colapso": expect_collapse, "ok": good,
                        "colapso_em": collapse and collapse["docs"], "docs": r["docs"], "segundos": r["segundos"]})
        print(f"  {GREEN + '✔' if good else RED + '✗'}{RESET} {name}: "
              f"{'colapso em ' + fmt(collapse['docs']) + ' docs' if collapse else 'sem colapso'}, "
              f"{fmt(r['docs'])} docs em {dur(r['segundos'])}")
    shutil.rmtree(tmp, ignore_errors=True)
    save_result("carga-longa", scale, {"ok": ok, "cenarios": summary})
    return ok


def carga_longa_section(R, scale):
    res = latest(R, "carga-longa", scale)
    if not res:
        return []
    L = [f"## Carga longa ({scale})", "",
         "Cenário do colapso da carga (filtros de Bloom maiores que uma parte do block cache). Uma janela "
         "colapsa quando o MB/s cai abaixo de 20% da mediana das 10 anteriores.", "",
         "| Cenário | Esperado | Resultado | Docs | Tempo | docs/s |", "|---|---|---|---|---|---|"]
    for c in res["cenarios"]:
        r = latest(R, "load", c["rotulo"]) or {}
        got = f"colapso em {fmt(c['colapso_em'])} docs" if c["colapso_em"] else "sem colapso"
        L.append(f"| {c['cenario']} | {'colapso' if c['esperava_colapso'] else 'sem colapso'} | "
                 f"{'✅' if c['ok'] else '❌'} {got} | {fmt(c['docs'])} | {dur(c['segundos'])} | {fmt(r.get('docs_por_s'))} |")
    L.append("")
    main = next((latest(R, "load", c["rotulo"]) for c in res["cenarios"] if c["cenario"].startswith("padrao")), None)
    windows = (main or {}).get("janelas") or []
    if windows:
        step_ = max(1, len(windows) // 15)
        L += ["### Janelas da carga padrão", "",
              "| Docs | MB/s | docs/s | commit | Gets/doc | filtro relido | stall | L0 | RSS |", "|---|---|---|---|---|---|---|---|---|"]
        for w in windows[step_ - 1::step_]:
            L.append(f"| {fmt(w['docs'])} | {fmt(w['mb_por_s'], 1)} | {fmt(w['docs_por_s'])} | {fmt(w['commit_fracao'] * 100)}% | "
                     f"{fmt(w['gets_por_doc'])} | {fmt(w['filtro_mb'])} MB | {fmt(w['stall_s'], 1)}s | {w['l0']} | {fmt(w['rss_mb'])} MB |")
        L.append("")
    return L


# ------------------------------------------------------------------ comparação

def pct(new, old):
    return (new - old) / old * 100 if old else 0.0


def compare_bench(a, b, limit=10.0):
    """Imprime p50/p99 de dois resultados de bench; devolve os conjuntos cujo p50 piorou além do limite."""
    sets_a = {s["conjunto"]: s for s in a["conjuntos"]}
    worse = []
    print(f"\n{'conjunto':<16} {'p50 antes':>10} {'p50 agora':>10} {'Δ':>8}   {'p99 antes':>10} {'p99 agora':>10} {'Δ':>8}")
    for s in b["conjuntos"]:
        o = sets_a.get(s["conjunto"])
        if not o:
            continue
        d50, d99 = pct(s["p50_us"], o["p50_us"]), pct(s["p99_us"], o["p99_us"])
        flag = f"  {RED}piorou{RESET}" if d50 > limit else ""
        if d50 > limit:
            worse.append(s["conjunto"])
        print(f"{s['conjunto']:<16} {o['p50_us']:>9.0f}µ {s['p50_us']:>9.0f}µ {d50:>+7.1f}%   "
              f"{o['p99_us']:>9.0f}µ {s['p99_us']:>9.0f}µ {d99:>+7.1f}%{flag}")
    print(f"{DIM}commits: {a.get('commit')} → {b.get('commit')}{RESET}")
    return worse


def cmd_comparar(args):
    with open(args.a) as f:
        a = json.load(f)
    with open(args.b) as f:
        b = json.load(f)
    sys.exit(1 if compare_bench(a, b) else 0)


# -------------------------------------------------------------------- relatório

def fmt(n, casas=0):
    if n is None:
        return "—"
    s = f"{n:,.{casas}f}"
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
    for s in r["conjuntos"]:
        rows.append(f"| `{s['conjunto']}` | {us(s['p50_us'])} | {us(s['p95_us'])} | {us(s['p99_us'])} | {us(s['max_us'])} | "
                    f"{fmt(s['qps'])} | {fmt(s['resultados_media'], 1)} | {fmt(s['gets_por_consulta'], 1)} | "
                    f"{fmt(s['block_reads_por_consulta'], 2)} |")
    return rows + ["", f"<sub>{r['_arquivo']}</sub>", ""]


def cmd_relatorio(scale):
    R = load_results()
    if not R:
        sys.exit(f"nenhum resultado em {RESULTS}")
    machine = next((r["maquina"] for r in reversed(R) if "maquina" in r), {})
    L = [
        f"# Resultados de performance (escala {scale})",
        "",
        f"Gerado por `python3 scripts/perf.py relatorio --escala {scale}` a partir de `results/`. "
        "Cada seção mostra o resultado **mais recente** daquele tipo; as cargas mostram a **mediana** das "
        f"últimas rodadas do mesmo commit (até {MEDIANA_DE}), com a faixa mín–máx entre parênteses. O plano está em "
        f"[docs/plano-performance.md]({DOCS_URL}/plano-performance.md).",
        "",
        f"- Último resultado: `{R[-1]['_arquivo']}`",
        f"- Máquina: {machine.get('nucleos', '?')} núcleos, {mb(machine.get('ram_total'))} MB de RAM, "
        f"kernel `{machine.get('kernel', '?')}`",
        "",
    ]

    # Fase 1
    L += ["## Fase 1: corretude", ""]
    corr = latest(R, "corretude", scale)
    if corr:
        L += ["| Checagem | Resultado |", "|---|---|"]
        for k, ok in corr["checagens"].items():
            L.append(f"| `{k}` | {'✅ ok' if ok else '❌ divergiu'} |")
        idem = corr.get("idempotencia", {})
        if idem:
            L += ["", f"Idempotência ({idem.get('fatia')}): banco de {mb(idem.get('banco_1a'))} MB após uma carga, "
                      f"{mb(idem.get('banco_2a'))} MB após duas e {mb(idem.get('banco_2a_compactado'))} MB depois de compactar."]
        L.append("")
    oracles = {}
    for r in R:
        if r.get("tipo") == "oracle" and not r.get("rotulo", "").startswith("pr-"):
            oracles[r["rotulo"]] = r
    for label, r in oracles.items():
        L += [f"**Oráculo `{label}`** ({fmt(r['docs'])} docs, {fmt(r['termos_oraculo'])} termos): "
              f"{'zero divergências' if r['divergencias'] == 0 else str(r['divergencias']) + ' divergências'}", "",
              "| Verificação | Casos | Divergências | Notas |", "|---|---|---|---|"]
        for c in r["verificacoes"]:
            L.append(f"| {c['verificacao']} | {fmt(c['casos'])} | {c['divergencias']} | {'; '.join(c['notas'])} |")
        rel = r.get("relevancia")
        if rel:
            L += ["", f"Relevância (título → artigo, {fmt(rel['consultas'])} consultas): top-1 {pct_txt(rel['top1'])}, "
                      f"top-10 {pct_txt(rel['top10'])}, MRR {fmt(rel['mrr'], 3)}, fora do top-1000: {rel['fora_do_top1000']}."]
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
        terms = next((e["chaves"] for e in sz["espacos"] if e["espaco"] == "post"), None) if sz else None
        w = r.get("escrita") or {}
        L.append(f"| {s} | `{r.get('commit', '?')}` | {r['_rodadas']} | {dur(r['segundos'])} | {docs_s(r)} | {fmt(r['mb_por_s'], 1)} | "
                 f"{mb(r['pico_rss'])} MB | {mb(r['banco_bytes'])} MB | {fmt(r['banco_por_texto'], 2)} | "
                 f"{fmt(r['nos_trie'])} | {fmt(terms)} | {fmt(w.get('write_amp'), 2)} | "
                 f"{fmt((w.get('stall_micros') or 0) / 1e6, 1)}s |")
    L.append("")
    exp_slice = smaller(scale, "100k")
    prefix = f"exp-{exp_slice}-"
    exps = {}
    for r in R:
        if r.get("tipo") == "load" and r.get("rotulo", "").startswith(prefix):
            exps[r["rotulo"][len(prefix):]] = median_load(R, r["rotulo"])
    if exps:
        base = median_load(R, f"ptwiki-{exp_slice}")
        L += [f"### Experimentos ({exp_slice})", "", "| Experimento | docs/s | vs. padrão | Pico RSS | Banco |", "|---|---|---|---|---|"]
        if base:
            L.append(f"| padrão (lote {LOTE}) | {docs_s(base)} | — | {mb(base['pico_rss'])} MB | {mb(base['banco_bytes'])} MB |")
        for label, r in exps.items():
            vs = f"{pct(r['docs_por_s'], base['docs_por_s']):+.0f}%" if base and "streaming" not in label else "—"
            L.append(f"| {label} | {docs_s(r)} | {vs} | {mb(r['pico_rss'])} MB | {mb(r['banco_bytes'])} MB |")
        L.append("")
    sz = latest(R, "sizes", f"ptwiki-{scale}")
    if sz:
        L += [f"### Tamanhos ({scale})", "", "| Espaço | Chaves | Chaves (MB) | Valores (MB) |", "|---|---|---|---|"]
        for e in sz["espacos"]:
            L.append(f"| `{e['espaco']}:` | {fmt(e['chaves'])} | {mb(e['bytes_chaves'])} | {mb(e['bytes_valores'])} |")
        L += ["", f"Raiz (`node:0`): {fmt(sz['raiz_bytes'])} bytes. "
              + "Listas: " + ", ".join(f"\"{t}\" {fmt(b / 1024, 1)} KB" for t, b in sz.get("listas", {}).items()), ""]

    # Fase 4
    post = latest(R, "bench", f"{scale}-t1")
    L += [f"## Fase 4: latência de busca ({scale}, cache quente, 1 thread)", ""] + bench_table(post)
    pre = latest(R, "bench", f"{scale}-pre-compactacao")
    if pre and post:
        post_sets = {s["conjunto"]: s for s in post["conjuntos"]}
        L += ["### Merge pendente vs. compactado", "", "| Conjunto | p50 antes | p50 depois | p99 antes | p99 depois | merges/consulta antes |",
              "|---|---|---|---|---|---|"]
        for s in pre["conjuntos"]:
            o = post_sets.get(s["conjunto"])
            if o:
                L.append(f"| `{s['conjunto']}` | {us(s['p50_us'])} | {us(o['p50_us'])} | {us(s['p99_us'])} | {us(o['p99_us'])} | "
                         f"{fmt(s.get('merges_por_consulta'), 1)} |")
        L.append("")
    comp = latest(R, "compact", scale)
    if comp:
        L += [f"Compactação: {dur(comp['segundos'])}, banco de {mb(comp['banco_antes'])} MB → {mb(comp['banco_depois'])} MB. "
              f"Abrir o banco + 1 busca: {fmt(comp['abertura_antes']['mediana_s'] * 1000)} ms antes, "
              f"{fmt(comp['abertura_depois']['mediana_s'] * 1000)} ms depois (mediana de {comp['abertura_depois']['execucoes']}).", ""]
    c64, c1g = latest(R, "bench", f"{scale}-cache-64mb"), latest(R, "bench", f"{scale}-cache-1024mb")
    if (c64 or c1g) and post:
        post_sets = {s["conjunto"]: s for s in post["conjuntos"]}
        a = {s["conjunto"]: s for s in (c64 or {}).get("conjuntos", [])}
        b = {s["conjunto"]: s for s in (c1g or {}).get("conjuntos", [])}
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
    scaling = [(t, latest(R, "bench", f"{scale}-misto-t{t}")) for t in (1, 2, 4, 8, 12)]
    if any(r for _, r in scaling):
        L += ["## Fase 5: concorrência", "", "### Só leitura (`misto`)", "", "| Threads | qps | ganho vs. 1 | p50 | p99 |", "|---|---|---|---|---|"]
        q1 = next((r["conjuntos"][0]["qps"] for t, r in scaling if t == 1 and r), None)
        for t, r in scaling:
            if r:
                s = r["conjuntos"][0]
                L.append(f"| {t} | {fmt(s['qps'])} | {fmt(s['qps'] / q1, 1) + '×' if q1 else '—'} | {us(s['p50_us'])} | {us(s['p99_us'])} |")
        L.append("")
    conc = [(t, latest(R, "concurrent", f"{scale}-misto-t{t}")) for t in (1, 4, 8)]
    if any(r for _, r in conc):
        L += ["### Leitura durante escrita (carga dos últimos 10% da fatia)", "",
              "| Leitores | p50 sem escrita | p50 com escrita | p99 sem escrita | p99 com escrita | indexação docs/s | stalls |",
              "|---|---|---|---|---|---|---|"]
        for t, r in conc:
            if r:
                a, b = r["leitura_sem_escrita"], r["leitura_com_escrita"]
                stall = ((r["escrita_depois"] or {}).get("stall_micros", 0) - (r["escrita_antes"] or {}).get("stall_micros", 0)) / 1e6
                L.append(f"| {t} | {us(a['p50_us'])} | {us(b['p50_us'])} | {us(a['p99_us'])} | {us(b['p99_us'])} | "
                         f"{fmt(r['indexacao']['docs_por_s'])} | {fmt(stall, 1)}s |")
        L.append("")

    L += frases_section(R, scale, post, oracles)
    L += carga_longa_section(R, scale)

    # Metas
    load_scale = median_load(R, f"ptwiki-{scale}")
    t1 = {s["conjunto"]: s for s in (post or {}).get("conjuntos", [])}
    c4 = latest(R, "concurrent", f"{scale}-misto-t4")
    divs = sum(r["divergencias"] for r in oracles.values()) if oracles else None
    swap_growth = (load_scale or {}).get("swap_aumento")
    swap_txt = "—" if swap_growth is None else ("sim" if swap_growth < 64 << 20 else f"não (+{mb(swap_growth)} MB)")
    L += relevancia_section(R, scale)
    L += stemming_section(R, scale)
    L += [f"## Metas (linha de base em {scale})", "", "| Métrica | Linha de base | Meta |", "|---|---|---|",
          f"| Divergências do oráculo | {divs if divs is not None else '—'} | **0** |",
          f"| Carga sem swap (aumento < 64 MB) | {swap_txt} | **sim** |",
          f"| docs/s na carga de {scale} | {fmt(load_scale['docs_por_s']) if load_scale else '—'} | |",
          f"| Banco ÷ texto bruto | {fmt(load_scale['banco_por_texto'], 2) if load_scale else '—'} | |",
          f"| p99 de `titulos` (quente) | {us(t1.get('titulos', {}).get('p99_us'))} | |",
          f"| p99 de `t-freq` (quente) | {us(t1.get('t-freq', {}).get('p99_us'))} | |",
          f"| p99 de `misto` com indexação em paralelo | {us(c4['leitura_com_escrita']['p99_us']) if c4 else '—'} | |",
          f"| Abrir o banco + 1 busca (após a carga) | {fmt(comp['abertura_antes']['mediana_s'] * 1000) + ' ms' if comp else '—'} | |",
          f"| p50 de `rank-freq` ÷ p50 de `t-freq` | {ratio_txt(t1, 'rank-freq', 't-freq')} | **≤ 2×** |",
          f"| p50 de `frase-2` | {us(t1.get('frase-2', {}).get('p50_us'))} | |",
          ""]

    with open(REPORT, "w") as f:
        f.write("\n".join(L))
    print(f"relatório: {REPORT}")


def ratio_txt(sets, a, b):
    """p50 de `a` ÷ p50 de `b`, como "1,8×"."""
    pa, pb = sets.get(a, {}).get("p50_us"), sets.get(b, {}).get("p50_us")
    return f"{fmt(pa / pb, 2)}×" if pa and pb else "—"


def frases_section(R, scale, post, oracles):
    """Fase E do plano de frases e ranking: custo do formato v2 e latência."""
    exp = smaller(scale, "100k")
    v1 = median_load(R, f"ptwiki-{scale}", fmt=1)
    v2 = median_load(R, f"ptwiki-{scale}", fmt=2)
    nopos = median_load(R, f"exp-{exp}-sem-posicoes", fmt=2)
    if not v2:
        return []
    sets = {s["conjunto"]: s for s in (post or {}).get("conjuntos", [])}

    def delta(r):
        return f" ({pct(r['docs_por_s'], v1['docs_por_s']):+.0f}%)" if r and v1 else ""

    L = ["## Frases e ranking (formato v2)", "",
         f"Plano em [docs/plano-frases-e-ranking.md]({DOCS_URL}/plano-frases-e-ranking.md). A linha de base é a última "
         "carga no formato v1 (sem tf nem posições) com o mesmo rótulo.", "",
         "| Métrica | Linha de base (v1) | Agora | Meta |", "|---|---|---|---|",
         f"| Carga {scale} com posições | {docs_s(v1) + ' docs/s' if v1 else '—'} | {docs_s(v2)} docs/s{delta(v2)} | queda ≤ 30% |"]
    if nopos and exp == scale:
        L.append(f"| Carga {scale} sem posições | {docs_s(v1) + ' docs/s' if v1 else '—'} | {docs_s(nopos)} docs/s{delta(nopos)} | queda ≤ 10% |")
    # Logo após a carga: inclui o WAL ainda não compactado, varia entre rodadas.
    L += [f"| Banco ÷ texto logo após a carga, com posições | {fmt(v1['banco_por_texto'], 2) if v1 else '—'} | {fmt(v2['banco_por_texto'], 2)} | registrar |"]
    if nopos and exp == scale:
        L.append(f"| Banco ÷ texto logo após a carga, sem posições | {fmt(v1['banco_por_texto'], 2) if v1 else '—'} | {fmt(nopos['banco_por_texto'], 2)} | |")
    comp, comp_nopos = latest(R, "compact", scale), latest(R, "compact", f"{exp}-sem-posicoes")
    v1_commits = {r.get("commit") for r in R if r.get("tipo") == "load" and formato(r) == 1}
    comp_v1 = next((r for r in reversed(R) if r.get("tipo") == "compact" and r.get("rotulo") == scale
                    and r.get("commit") in v1_commits), None)
    text = v2.get("bytes_corpus") or 1
    ratio = lambda c: fmt(c["banco_depois"] / text, 2) + f" ({mb(c['banco_depois'])} MB)" if c else "—"
    L += [f"| Banco compactado ÷ texto, com posições | {ratio(comp_v1)} | {ratio(comp)} | registrar |"]
    if comp_nopos and exp == scale:
        L.append(f"| Banco compactado ÷ texto, sem posições | {ratio(comp_v1)} | {ratio(comp_nopos)} | |")
    L += [f"| Pico de RSS na carga, com posições | {mb(v1['pico_rss']) + ' MB' if v1 else '—'} | {mb(v2['pico_rss'])} MB | |",
          f"| p50 de `rank-freq` ÷ p50 de `t-freq` | — | {ratio_txt(sets, 'rank-freq', 't-freq')} | ≤ 2× |",
          f"| p50 de `frase-2` | — | {us(sets.get('frase-2', {}).get('p50_us'))} | registrar |"]
    rel = next((r.get("relevancia") for r in reversed(list(oracles.values())) if r.get("relevancia")), None)
    if rel:
        L.append(f"| Título → artigo (MRR) | — | {fmt(rel['mrr'], 3)} (top-1 {pct_txt(rel['top1'])}, top-10 {pct_txt(rel['top10'])}) | registrar e acompanhar |")
    L.append("")

    with_pos, without = latest(R, "sizes", f"ptwiki-{scale}"), latest(R, "sizes", f"exp-{exp}-sem-posicoes")
    if with_pos and with_pos.get("posicoes") is not None:
        a = {e["espaco"]: e for e in with_pos["espacos"]}
        b = {e["espaco"]: e for e in (without or {}).get("espacos", [])}
        L += [f"### Espaço por tipo de chave ({scale}, valores lógicos)", "",
              "| Espaço | Com posições (MB) | Sem posições (MB) |", "|---|---|---|"]
        for name in sorted(set(a) | set(b)):
            size = lambda e: mb(e["bytes_chaves"] + e["bytes_valores"]) if e else "—"
            L.append(f"| `{name}:` | {size(a.get(name))} | {size(b.get(name))} |")
        L.append("")

    pairs = [("t-freq", "rank-freq"), ("and-2", "rank-and"), ("titulos", "rank-titulos")]
    if any(r in sets for _, r in pairs):
        L += [f"### Ranking: top 10 vs. todos os IDs ({scale}, p50)", "",
              "| Consulta | Sem ranking | Top 10 (BM25) | Razão |", "|---|---|---|---|"]
        for plain, ranked in pairs:
            if ranked in sets:
                L.append(f"| `{plain}` / `{ranked}` | {us(sets.get(plain, {}).get('p50_us'))} | "
                         f"{us(sets[ranked]['p50_us'])} | {ratio_txt(sets, ranked, plain)} |")
        L.append("")
    frase = [s for s in ("frase-2", "frase-3", "frase-stop", "frase-freq", "frase-miss") if s in sets]
    if frase:
        L += [f"### Frases ({scale})", "", "| Conjunto | p50 | p95 | p99 | resultados (média) | Gets/consulta |", "|---|---|---|---|---|---|"]
        for name in frase:
            x = sets[name]
            L.append(f"| `{name}` | {us(x['p50_us'])} | {us(x['p95_us'])} | {us(x['p99_us'])} | "
                     f"{fmt(x['resultados_media'], 1)} | {fmt(x['gets_por_consulta'], 1)} |")
        L.append("")
    return L


# ------------------------------------------------------------------------ main

def main():
    global REPETICOES
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--escala", choices=ORDER, default="10k",
                        help="maior fatia do corpus usada (padrão: 10k)")
    common.add_argument("--repeticoes", type=int, default=REPETICOES,
                        help=f"rodadas de cada carga de até {REPETIR_ATE_DOCS} docs (padrão: {REPETICOES})")
    p = argparse.ArgumentParser(description="Plano de performance do fire-flower")
    sub = p.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("escada", parents=[common], help="Fase 2")
    e.add_argument("--experimentos", action="store_true")
    for name, help_ in (("corretude", "Fase 1"), ("consultas", "Fase 3"), ("latencia", "Fase 4"),
                        ("concorrencia", "Fase 5"), ("frases", "frases e ranking (Fase E do plano)"),
                        ("relevancia", "ordenação: consultas de resposta conhecida"),
                        ("stemming", "stemming ligado vs. desligado (A/B no mesmo binário)"),
                        ("relatorio", "gera RESULTADOS.md no repositório de resultados")):
        sub.add_parser(name, parents=[common], help=help_)
    pr = sub.add_parser("pr", parents=[common], help="Fase 8")
    pr.add_argument("--salvar-baseline", action="store_true")
    cl = sub.add_parser("carga-longa", parents=[common], help="cenário do colapso da carga (padrão: escala full)")
    cl.add_argument("--reproduzir", action="store_true", help="roda antes o cenário com filtro inteiro, que tem que colapsar")
    c = sub.add_parser("comparar", help="compara dois resultados de bench")
    c.add_argument("a")
    c.add_argument("b")
    t = sub.add_parser("tudo", parents=[common], help="todas as fases, na ordem")
    t.add_argument("--sem-concorrencia", action="store_true", help="pula a Fase 5")
    args = p.parse_args()

    if args.cmd == "comparar":
        return cmd_comparar(args)
    if args.cmd == "relatorio":
        return cmd_relatorio(args.escala)
    REPETICOES = max(1, args.repeticoes)
    scale = args.escala
    if args.cmd == "carga-longa" and "--escala" not in sys.argv:
        scale = "full"  # o colapso só aparece em cargas grandes
    build()
    started = time.time()
    ok = True
    if args.cmd == "tudo":
        fase_escada(scale, experimentos=True)
        ok &= fase_corretude(scale)
        fase_relevancia(scale)
        fase_consultas(scale)
        fase_latencia(scale)
        if not args.sem_concorrencia:
            fase_concorrencia(scale)
        ok &= fase_pr(scale, salvar_baseline=True)
    elif args.cmd == "escada":
        fase_escada(scale, args.experimentos)
    elif args.cmd == "pr":
        ok = fase_pr(scale, args.salvar_baseline)
    elif args.cmd == "carga-longa":
        ok = fase_carga_longa(scale, args.reproduzir)
    else:
        result = {"corretude": fase_corretude, "consultas": fase_consultas, "latencia": fase_latencia,
                  "concorrencia": fase_concorrencia, "frases": fase_frases,
                  "relevancia": fase_relevancia, "stemming": fase_stemming}[args.cmd](scale)
        ok = result is not False
    cmd_relatorio(scale)
    status = f"{GREEN}pronto" if ok else f"{RED}terminou com falhas"
    print(f"\n{status} em {dur(time.time() - started)}{RESET}")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
