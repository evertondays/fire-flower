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
    python3 scripts/perf.py relatorio         # escreve bench/RESULTADOS.md
    python3 scripts/perf.py comparar A.json B.json

`--escala` (1k, 10k, 100k ou full; padrão 10k) define a maior fatia do corpus
usada. A Wikipédia inteira (full) leva mais de uma hora; comece pequeno.

Bancos ficam em ~/bench-db/perf/ (FF_PERF_DB para mudar); o corpus em
~/datasets/ptwiki/ (FF_CORPUS_DIR). Resultados em bench/results/, com a escala
no rótulo, então escalas diferentes não se misturam.

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
RESULTS = os.path.join(PROJECT, "bench", "results")
REPORT = os.path.join(PROJECT, "bench", "RESULTADOS.md")
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


def ensure_loaded(slice_name):
    """Banco da fatia; carrega se ainda não existir."""
    path = db(f"ptwiki-{slice_name}")
    if not os.path.exists(path):
        step(f"carga {slice_name} (banco não existia)")
        fresh(path)
        wiki("load", "--db", path, "--corpus", corpus(slice_name), "--batch", 1000, "--label", f"ptwiki-{slice_name}")
    return path


def git_commit():
    def git(*a):
        return subprocess.run(["git", "-C", PROJECT, *a], capture_output=True, text=True).stdout.strip()
    h = git("rev-parse", "--short", "HEAD") or "desconhecido"
    return h + ("-modificado" if git("status", "--porcelain", "--untracked-files=no") else "")


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


def can_drop_caches():
    return subprocess.run(["sudo", "-n", "true"], capture_output=True).returncode == 0


# ---------------------------------------------------------------------- fases

def fase_escada(scale, experimentos=False):
    """Fase 2: carga de cada fatia, até a escala, num banco novo; mais tamanhos."""
    for s in ORDER[: ORDER.index(scale) + 1]:
        step(f"escada: carga {s}")
        path = db(f"ptwiki-{s}")
        fresh(path)
        wiki("load", "--db", path, "--corpus", corpus(s), "--batch", 1000, "--label", f"ptwiki-{s}")
        wiki("sizes", "--db", path)
    if experimentos:
        fase_experimentos(scale)


def fase_experimentos(scale):
    """Experimentos da Fase 2, cada um num banco temporário (o plano usa 100k)."""
    s = smaller(scale, "100k")
    n = docs_in(s)
    tmp = db("tmp-exp")
    runs = [(f"lote-{b}", ["--batch", b]) for b in (100, 5000, 10000, 50000) if b <= n]
    runs += [("ids-remapeados", ["--batch", 1000, "--remap-ids"]),
             ("cache-64mb", ["--batch", 1000, "--cache-mb", 64]),
             ("cache-1024mb", ["--batch", 1000, "--cache-mb", 1024])]
    for label, extra in runs:
        step(f"experimento {s}: {label}")
        fresh(tmp)
        wiki("load", "--db", tmp, "--corpus", corpus(s), *extra, "--label", f"exp-{s}-{label}")
        if label == "ids-remapeados":
            wiki("sizes", "--db", tmp)
    stream = smaller(s, "10k") if s != "10k" else "1k"
    step(f"experimento: streaming (1 commit por documento, {stream})")
    fresh(tmp)
    wiki("load", "--db", tmp, "--corpus", corpus(stream), "--streaming", "--label", f"exp-{s}-streaming-{stream}")
    shutil.rmtree(tmp, ignore_errors=True)


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
    if not os.path.isdir(qdir):
        fase_consultas(scale)
    files = [os.path.join(qdir, f"{s}.txt") for s in sets] if sets else [qdir]
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
    wiki("load", "--db", path, "--corpus", corpus(scale), "--batch", 1000, "--label", f"ptwiki-{scale}")

    step("merge pendente: t-freq, títulos e and-freq antes da compactação")
    pre_open = open_time(path)
    bench(path, scale, ["t-freq", "titulos", "and-freq"], f"{scale}-pre-compactacao")

    step("compactação")
    size_before = dir_size(path)
    t = time.time()
    run([FF, "compact", path])
    compact_s = time.time() - t
    save_result("compact", scale, {"segundos": compact_s, "banco_antes": size_before, "banco_depois": dir_size(path),
                                   "abertura_antes": pre_open, "abertura_depois": open_time(path)})
    wiki("sizes", "--db", path)

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
    if not os.path.exists(base):
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
    baseline_path = os.path.join(PROJECT, "bench", f"baseline-pr-{scale}.json")
    tmp_small, tmp_big = db("tmp-pr-oraculo"), db("tmp-pr-carga")
    step(f"pr: oráculo {small}")
    fresh(tmp_small)
    wiki("load", "--db", tmp_small, "--corpus", corpus(small), "--label", f"pr-oraculo-{small}")
    ok = wiki("oracle", "--db", tmp_small, "--corpus", corpus(small), "--label", f"pr-{small}", check=False) == 0
    step(f"pr: carga {scale} + bench")
    fresh(tmp_big)
    wiki("load", "--db", tmp_big, "--corpus", corpus(scale), "--label", f"pr-{scale}")
    bench(tmp_big, scale, ["t-medio", "t-freq", "p-2"], f"pr-{scale}")
    for p in (tmp_small, tmp_big):
        shutil.rmtree(p, ignore_errors=True)

    results = load_results()
    current = latest(results, "bench", f"pr-{scale}")
    load_now = latest(results, "load", f"pr-{scale}")
    if salvar_baseline or not os.path.exists(baseline_path):
        with open(baseline_path, "w") as f:
            json.dump({"bench": current, "load": load_now}, f, indent=2, ensure_ascii=False)
        print(f"linha de base salva em {baseline_path}")
        return ok
    with open(baseline_path) as f:
        base = json.load(f)
    worse = compare_bench(base["bench"], current)
    docs_base, docs_now = base["load"]["docs_por_s"], load_now["docs_por_s"]
    print(f"\ncarga {scale}: {docs_base:.0f} → {docs_now:.0f} docs/s ({pct(docs_now, docs_base):+.1f}%)")
    if not ok:
        print(f"{RED}✗ oráculo divergiu{RESET}")
    if worse:
        print(f"{RED}✗ p50 piorou mais de 10% em: {', '.join(worse)}{RESET}")
    return ok and not worse


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
        sys.exit("nenhum resultado em bench/results/")
    machine = next((r["maquina"] for r in reversed(R) if "maquina" in r), {})
    L = [
        f"# Resultados de performance (escala {scale})",
        "",
        f"Gerado por `python3 scripts/perf.py relatorio --escala {scale}` a partir de `bench/results/`. "
        "Cada seção mostra o resultado **mais recente** daquele tipo. O plano está em "
        "[docs/plano-performance.md](../docs/plano-performance.md).",
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
        L.append("")

    # Fase 2
    L += ["## Fase 2: escada de indexação", "",
          "| Fatia | Tempo | docs/s | MB/s | Pico RSS | Banco | Banco ÷ texto | Nós da Trie | Termos | Write amp | Stalls |",
          "|---|---|---|---|---|---|---|---|---|---|---|"]
    for s in ORDER:
        r = latest(R, "load", f"ptwiki-{s}")
        if not r:
            continue
        sz = latest(R, "sizes", f"ptwiki-{s}")
        terms = next((e["chaves"] for e in sz["espacos"] if e["espaco"] == "post"), None) if sz else None
        w = r.get("escrita") or {}
        L.append(f"| {s} | {dur(r['segundos'])} | {fmt(r['docs_por_s'])} | {fmt(r['mb_por_s'], 1)} | "
                 f"{mb(r['pico_rss'])} MB | {mb(r['banco_bytes'])} MB | {fmt(r['banco_por_texto'], 2)} | "
                 f"{fmt(r['nos_trie'])} | {fmt(terms)} | {fmt(w.get('write_amp'), 2)} | "
                 f"{fmt((w.get('stall_micros') or 0) / 1e6, 1)}s |")
    L.append("")
    exp_slice = smaller(scale, "100k")
    prefix = f"exp-{exp_slice}-"
    exps = {}
    for r in R:
        if r.get("tipo") == "load" and r.get("rotulo", "").startswith(prefix):
            exps[r["rotulo"][len(prefix):]] = r
    if exps:
        base = latest(R, "load", f"ptwiki-{exp_slice}")
        L += [f"### Experimentos ({exp_slice})", "", "| Experimento | docs/s | vs. padrão | Pico RSS | Banco |", "|---|---|---|---|---|"]
        if base:
            L.append(f"| padrão (lote 1000) | {fmt(base['docs_por_s'])} | — | {mb(base['pico_rss'])} MB | {mb(base['banco_bytes'])} MB |")
        for label, r in exps.items():
            vs = f"{pct(r['docs_por_s'], base['docs_por_s']):+.0f}%" if base and "streaming" not in label else "—"
            L.append(f"| {label} | {fmt(r['docs_por_s'])} | {vs} | {mb(r['pico_rss'])} MB | {mb(r['banco_bytes'])} MB |")
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

    # Metas
    load_scale = latest(R, "load", f"ptwiki-{scale}")
    t1 = {s["conjunto"]: s for s in (post or {}).get("conjuntos", [])}
    c4 = latest(R, "concurrent", f"{scale}-misto-t4")
    divs = sum(r["divergencias"] for r in oracles.values()) if oracles else None
    swap_growth = (load_scale or {}).get("swap_aumento")
    swap_txt = "—" if swap_growth is None else ("sim" if swap_growth < 64 << 20 else f"não (+{mb(swap_growth)} MB)")
    L += [f"## Metas (linha de base em {scale})", "", "| Métrica | Linha de base | Meta |", "|---|---|---|",
          f"| Divergências do oráculo | {divs if divs is not None else '—'} | **0** |",
          f"| Carga sem swap (aumento < 64 MB) | {swap_txt} | **sim** |",
          f"| docs/s na carga de {scale} | {fmt(load_scale['docs_por_s']) if load_scale else '—'} | |",
          f"| Banco ÷ texto bruto | {fmt(load_scale['banco_por_texto'], 2) if load_scale else '—'} | |",
          f"| p99 de `titulos` (quente) | {us(t1.get('titulos', {}).get('p99_us'))} | |",
          f"| p99 de `t-freq` (quente) | {us(t1.get('t-freq', {}).get('p99_us'))} | |",
          f"| p99 de `misto` com indexação em paralelo | {us(c4['leitura_com_escrita']['p99_us']) if c4 else '—'} | |",
          f"| Abrir o banco + 1 busca (após a carga) | {fmt(comp['abertura_antes']['mediana_s'] * 1000) + ' ms' if comp else '—'} | |",
          ""]

    with open(REPORT, "w") as f:
        f.write("\n".join(L))
    print(f"relatório: {REPORT}")


# ------------------------------------------------------------------------ main

def main():
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--escala", choices=ORDER, default="10k",
                        help="maior fatia do corpus usada (padrão: 10k)")
    p = argparse.ArgumentParser(description="Plano de performance do fire-flower")
    sub = p.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("escada", parents=[common], help="Fase 2")
    e.add_argument("--experimentos", action="store_true")
    for name, help_ in (("corretude", "Fase 1"), ("consultas", "Fase 3"), ("latencia", "Fase 4"),
                        ("concorrencia", "Fase 5"), ("relatorio", "gera bench/RESULTADOS.md")):
        sub.add_parser(name, parents=[common], help=help_)
    pr = sub.add_parser("pr", parents=[common], help="Fase 8")
    pr.add_argument("--salvar-baseline", action="store_true")
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
    scale = args.escala
    build()
    started = time.time()
    ok = True
    if args.cmd == "tudo":
        fase_escada(scale, experimentos=True)
        ok &= fase_corretude(scale)
        fase_consultas(scale)
        fase_latencia(scale)
        if not args.sem_concorrencia:
            fase_concorrencia(scale)
        ok &= fase_pr(scale, salvar_baseline=True)
    elif args.cmd == "escada":
        fase_escada(scale, args.experimentos)
    elif args.cmd == "pr":
        ok = fase_pr(scale, args.salvar_baseline)
    else:
        result = {"corretude": fase_corretude, "consultas": fase_consultas, "latencia": fase_latencia,
                  "concorrencia": fase_concorrencia}[args.cmd](scale)
        ok = result is not False
    cmd_relatorio(scale)
    status = f"{GREEN}pronto" if ok else f"{RED}terminou com falhas"
    print(f"\n{status} em {dur(time.time() - started)}{RESET}")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
