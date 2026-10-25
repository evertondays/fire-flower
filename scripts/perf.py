#!/usr/bin/env python3
"""Roda o plano de performance (docs/plano-performance.md) e gera o relatório.

    python3 scripts/perf.py tudo            # todas as fases, na ordem (~1h com a carga total)
    python3 scripts/perf.py escada          # Fase 2: carga 1k → 10k → 100k → 1,1M  [--ate 100k] [--experimentos]
    python3 scripts/perf.py corretude       # Fase 1: oráculo, idempotência, lotes vs. único, reabrir
    python3 scripts/perf.py consultas       # Fase 3: vocabulário do corpus inteiro + conjuntos de consulta
    python3 scripts/perf.py latencia        # Fase 4: latência por conjunto, merge pendente vs. compactado
    python3 scripts/perf.py concorrencia    # Fase 5: escala com threads e leitura durante escrita
    python3 scripts/perf.py pr              # Fase 8: checagem rápida (~3 min); falha se o p50 piorar >10%
    python3 scripts/perf.py relatorio       # escreve bench/RESULTADOS.md com os resultados mais recentes
    python3 scripts/perf.py comparar A.json B.json

Bancos ficam em ~/bench-db/perf/ (FF_PERF_DB para mudar); o corpus em
~/datasets/ptwiki/ (FF_CORPUS_DIR). Resultados em bench/results/.

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
QUERIES = os.path.join(PROJECT, "bench", "queries")
REPORT = os.path.join(PROJECT, "bench", "RESULTADOS.md")
BASELINE_PR = os.path.join(PROJECT, "bench", "baseline-pr.json")
WIKI = os.path.join(PROJECT, "target", "release", "examples", "wiki")
FF = os.path.join(PROJECT, "target", "release", "fire-flower")

CORPUS_DIR = os.path.expanduser(os.environ.get("FF_CORPUS_DIR", "~/datasets/ptwiki"))
DB_ROOT = os.path.expanduser(os.environ.get("FF_PERF_DB", "~/bench-db/perf"))
SLICES = {
    "1k": "ptwiki-1000.jsonl",
    "10k": "ptwiki-10000.jsonl",
    "100k": "ptwiki-100000.jsonl",
    "full": "ptwiki.jsonl",
}
ORDER = list(SLICES)

ORANGE, BOLD, DIM, RED, GREEN, RESET = "\x1b[38;5;208m", "\x1b[1m", "\x1b[2m", "\x1b[31m", "\x1b[32m", "\x1b[0m"


# ------------------------------------------------------------------ utilidades

def corpus(slice_name):
    return os.path.join(CORPUS_DIR, SLICES[slice_name])


def db(name):
    return os.path.join(DB_ROOT, name)


def step(title):
    print(f"\n{ORANGE}{BOLD}▶ {title}{RESET}", flush=True)


def run(cmd, capture=False, check=True):
    print(f"{DIM}$ {' '.join(cmd)}{RESET}", flush=True)
    if capture:
        out = subprocess.run(cmd, stdout=subprocess.PIPE, text=True, check=check)
        return out.stdout
    return subprocess.run(cmd, check=check).returncode


def wiki(*args, capture=False, check=True):
    return run([WIKI, *map(str, args)], capture=capture, check=check)


def build():
    step("compilando (release)")
    run(["cargo", "build", "--release", "--bin", "fire-flower", "--example", "wiki"], check=True)


def fresh(path):
    if os.path.exists(path):
        shutil.rmtree(path)
    os.makedirs(os.path.dirname(path), exist_ok=True)


def dir_size(path):
    return sum(e.stat().st_size for e in os.scandir(path) if e.is_file()) if os.path.isdir(path) else 0


def digest(path):
    return json.loads(wiki("digest", "--db", path, capture=True))


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

def fase_escada(args):
    """Fase 2: carga de cada fatia num banco novo, mais tamanhos."""
    ate = ORDER.index(args.ate) + 1
    for s in ORDER[:ate]:
        step(f"escada: carga {s}")
        path = db(f"ptwiki-{s}")
        fresh(path)
        wiki("load", "--db", path, "--corpus", corpus(s), "--batch", 1000, "--label", f"ptwiki-{s}")
        wiki("sizes", "--db", path)
    if args.experimentos:
        experimentos_100k()


def experimentos_100k():
    """Experimentos da Fase 2 na fatia de 100k, cada um num banco temporário."""
    tmp = db("tmp-exp")
    runs = [
        ("lote-100", ["--batch", 100]),
        ("lote-10000", ["--batch", 10000]),
        ("lote-50000", ["--batch", 50000]),
        ("ids-remapeados", ["--batch", 1000, "--remap-ids"]),
        ("cache-64mb", ["--batch", 1000, "--cache-mb", 64]),
        ("cache-1024mb", ["--batch", 1000, "--cache-mb", 1024]),
    ]
    for label, extra in runs:
        step(f"experimento 100k: {label}")
        fresh(tmp)
        wiki("load", "--db", tmp, "--corpus", corpus("100k"), *extra, "--label", f"exp-{label}")
        if label == "ids-remapeados":
            wiki("sizes", "--db", tmp)
    step("experimento: streaming (1 commit por documento, 10k docs)")
    fresh(tmp)
    wiki("load", "--db", tmp, "--corpus", corpus("10k"), "--streaming", "--label", "exp-streaming-10k")
    shutil.rmtree(tmp, ignore_errors=True)


def fase_corretude(args):
    """Fase 1: oráculo e checagens de conteúdo lógico."""
    checks = {}
    tmp = db("tmp-corretude")

    step("oráculo: 10k, todos os termos")
    path10k = db("ptwiki-10k")
    if not os.path.exists(path10k):
        fresh(path10k)
        wiki("load", "--db", path10k, "--corpus", corpus("10k"), "--label", "ptwiki-10k")
    checks["oraculo_10k"] = wiki("oracle", "--db", path10k, "--corpus", corpus("10k"), "--label", "10k-todos", check=False) == 0

    step("idempotência: indexar a fatia de 10k duas vezes")
    fresh(tmp)
    shutil.copytree(path10k, tmp, dirs_exist_ok=True)
    before = digest(tmp)
    size_before = dir_size(tmp)
    wiki("load", "--db", tmp, "--corpus", corpus("10k"), "--label", "idempotencia-2a-carga")
    after = digest(tmp)
    size_twice = dir_size(tmp)
    run([FF, "compact", tmp])
    size_compacted = dir_size(tmp)
    checks["idempotencia"] = before["digest"] == after["digest"]
    idem = {"digest_1a": before, "digest_2a": after, "banco_1a": size_before,
            "banco_2a": size_twice, "banco_2a_compactado": size_compacted}

    step("lotes vs. único: 1k com --batch 1 e com --batch 1000")
    a, b = db("tmp-lote-1"), db("tmp-lote-1000")
    for path, batch in ((a, 1), (b, 1000)):
        fresh(path)
        wiki("load", "--db", path, "--corpus", corpus("1k"), "--batch", batch, "--label", f"lote-{batch}-1k")
    da, dbb = digest(a), digest(b)
    checks["lotes_vs_unico"] = da["digest"] == dbb["digest"]
    for p in (a, b):
        shutil.rmtree(p, ignore_errors=True)

    step("oráculo: 100k, amostra de 50 mil termos")
    path100k = db("ptwiki-100k")
    if not os.path.exists(path100k):
        fresh(path100k)
        wiki("load", "--db", path100k, "--corpus", corpus("100k"), "--label", "ptwiki-100k")
    checks["oraculo_100k"] = wiki("oracle", "--db", path100k, "--corpus", corpus("100k"),
                                  "--sample-terms", 50000, "--label", "100k-amostra", check=False) == 0

    step("reabrir: copiar 100k, carregar mais 1k e conferir 101k")
    fresh(tmp)
    shutil.copytree(path100k, tmp, dirs_exist_ok=True)
    wiki("load", "--db", tmp, "--corpus", corpus("full"), "--skip", 100000, "--take", 1000, "--label", "reabrir-mais-1k")
    checks["reabrir_101k"] = wiki("oracle", "--db", tmp, "--corpus", corpus("full"), "--take", 101000,
                                  "--sample-terms", 20000, "--titles", 2000, "--label", "101k-reaberto", check=False) == 0
    shutil.rmtree(tmp, ignore_errors=True)

    save_result("corretude", "fase1", {"ok": all(checks.values()), "checagens": checks,
                                        "idempotencia": idem, "lotes": {"batch_1": da, "batch_1000": dbb}})
    for name, ok in checks.items():
        print(f"  {GREEN + '✔' if ok else RED + '✗'}{RESET} {name}")
    return all(checks.values())


def fase_consultas(args):
    """Fase 3: vocabulário do corpus inteiro e conjuntos de consulta."""
    step("vocabulário do corpus inteiro")
    vocab = os.path.join(PROJECT, "bench", "data", "vocab-ptwiki.tsv")
    wiki("vocab", "--corpus", corpus("full"), "--out", vocab)
    step("conjuntos de consulta (seed 42)")
    wiki("queries", "--vocab", vocab, "--corpus", corpus("full"), "--seed", 42, "--out", QUERIES)


def bench(path, sets, label, threads=1, extra=()):
    files = [os.path.join(QUERIES, f"{s}.txt") for s in sets] if sets else [QUERIES]
    wiki("bench", "--db", path, "--queries", *files, "--threads", threads, "--label", label, *extra)


def open_time(path, runs=5):
    """Tempo de abrir o banco e fazer uma busca que não acha nada."""
    times = []
    for _ in range(runs):
        t = time.time()
        subprocess.run([FF, "search", path, "xyzzyinexistente"], stdout=subprocess.DEVNULL, check=True)
        times.append(time.time() - t)
    return {"mediana_s": statistics.median(times), "min_s": min(times), "max_s": max(times), "execucoes": runs}


def fase_latencia(args):
    """Fase 4: latência no banco de 1,1M."""
    path = db("ptwiki-full")
    if not os.path.exists(path):
        sys.exit(f"{path} não existe: rode `perf.py escada` antes")
    if not os.path.isdir(QUERIES):
        sys.exit("bench/queries/ não existe: rode `perf.py consultas` antes")

    step("merge pendente: t-freq e títulos antes da compactação")
    pre_open = open_time(path)
    bench(path, ["t-freq", "titulos", "and-freq"], "full-pre-compactacao")

    step("compactação")
    size_before = dir_size(path)
    t = time.time()
    run([FF, "compact", path])
    compact_s = time.time() - t
    size_after = dir_size(path)
    post_open = open_time(path)
    save_result("compact", "ptwiki-full", {"segundos": compact_s, "banco_antes": size_before, "banco_depois": size_after,
                                           "abertura_antes": pre_open, "abertura_depois": post_open})
    wiki("sizes", "--db", path)

    step("latência de todos os conjuntos (cache quente)")
    bench(path, None, "full-t1")

    if can_drop_caches():
        step("latência com cache frio")
        bench(path, ["t-raro", "t-medio", "t-freq", "titulos", "misto"], "full-t1-frio", extra=["--cold"])
    else:
        print(f"{DIM}pulando cache frio: precisa de `sudo -n` (rode `sudo -v` antes, se quiser){RESET}")

    for mb in (64, 1024):
        step(f"block cache de {mb} MB")
        bench(path, ["t-medio", "titulos", "misto", "p-2"], f"full-cache-{mb}mb", extra=["--cache-mb", mb])


def fase_concorrencia(args):
    """Fase 5: escala com threads e leitura durante escrita."""
    path = db("ptwiki-full")
    for t in (1, 2, 4, 8, 12):
        step(f"só leitura: misto com {t} thread(s)")
        bench(path, ["misto"], f"full-misto-t{t}", threads=t)

    base = db("ptwiki-1m")
    if not os.path.exists(base):
        step("carga dos primeiros 1.000.000 artigos (base da leitura durante escrita)")
        fresh(base)
        wiki("load", "--db", base, "--corpus", corpus("full"), "--take", 1000000, "--label", "ptwiki-1m")
    tmp = db("tmp-concorrencia")
    for t in (1, 4, 8):
        step(f"leitura durante escrita: {t} leitor(es) + carga dos 112k restantes")
        fresh(tmp)
        shutil.copytree(base, tmp, dirs_exist_ok=True)
        wiki("concurrent", "--db", tmp, "--corpus", corpus("full"), "--skip", 1000000,
             "--queries", os.path.join(QUERIES, "misto.txt"), "--threads", t, "--label", f"misto-t{t}")
    shutil.rmtree(tmp, ignore_errors=True)


def fase_pr(args):
    """Fase 8: oráculo 10k + carga 100k + bench rápido, comparado com a linha de base salva."""
    tmp10, tmp100 = db("tmp-pr-10k"), db("tmp-pr-100k")
    step("pr: oráculo 10k")
    fresh(tmp10)
    wiki("load", "--db", tmp10, "--corpus", corpus("10k"), "--label", "pr-10k")
    ok = wiki("oracle", "--db", tmp10, "--corpus", corpus("10k"), "--label", "pr-10k", check=False) == 0
    step("pr: carga 100k + bench")
    fresh(tmp100)
    wiki("load", "--db", tmp100, "--corpus", corpus("100k"), "--label", "pr-100k")
    bench(tmp100, ["t-medio", "t-freq", "p-2"], "pr-100k")
    for p in (tmp10, tmp100):
        shutil.rmtree(p, ignore_errors=True)

    results = load_results()
    current = latest(results, "bench", "pr-100k")
    load_now = latest(results, "load", "pr-100k")
    if args.salvar_baseline or not os.path.exists(BASELINE_PR):
        with open(BASELINE_PR, "w") as f:
            json.dump({"bench": current, "load": load_now}, f, indent=2, ensure_ascii=False)
        print(f"linha de base salva em {BASELINE_PR}")
        return ok
    with open(BASELINE_PR) as f:
        base = json.load(f)
    worse = compare_bench(base["bench"], current)
    docs_base, docs_now = base["load"]["docs_por_s"], load_now["docs_por_s"]
    print(f"\ncarga 100k: {docs_base:.0f} → {docs_now:.0f} docs/s ({pct(docs_now, docs_base):+.1f}%)")
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
    worse = compare_bench(a, b)
    sys.exit(1 if worse else 0)


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


def bench_table(r, sets=None):
    if not r:
        return ["_sem resultado_", ""]
    rows = ["| Conjunto | p50 | p95 | p99 | máx | qps | resultados (média) | Gets/consulta | block reads/consulta |",
            "|---|---|---|---|---|---|---|---|---|"]
    for s in r["conjuntos"]:
        if sets and s["conjunto"] not in sets:
            continue
        rows.append(f"| `{s['conjunto']}` | {us(s['p50_us'])} | {us(s['p95_us'])} | {us(s['p99_us'])} | {us(s['max_us'])} | "
                    f"{fmt(s['qps'])} | {fmt(s['resultados_media'], 1)} | {fmt(s['gets_por_consulta'], 1)} | "
                    f"{fmt(s['block_reads_por_consulta'], 2)} |")
    rows.append("")
    rows.append(f"<sub>{r['_arquivo']}</sub>")
    rows.append("")
    return rows


def us(v):
    if v is None:
        return "—"
    if v >= 1000:
        return f"{fmt(v / 1000, 1)} ms"
    return f"{fmt(v)} µs"


def cmd_relatorio(args):
    R = load_results()
    if not R:
        sys.exit("nenhum resultado em bench/results/")
    last = R[-1]
    machine = next((r["maquina"] for r in reversed(R) if "maquina" in r), {})
    L = [
        "# Resultados de performance",
        "",
        "Gerado por `python3 scripts/perf.py relatorio` a partir de `bench/results/`. "
        "Cada seção mostra o resultado **mais recente** daquele tipo. O plano está em "
        "[docs/plano-performance.md](../docs/plano-performance.md).",
        "",
        f"- Último resultado: `{last['_arquivo']}`",
        f"- Máquina: {machine.get('nucleos', '?')} núcleos, {mb(machine.get('ram_total'))} MB de RAM, "
        f"kernel `{machine.get('kernel', '?')}`",
        "",
    ]

    # Fase 1
    L += ["## Fase 1: corretude", ""]
    corr = latest(R, "corretude")
    if corr:
        L += ["| Checagem | Resultado |", "|---|---|"]
        names = {"oraculo_10k": "Oráculo 10k (todos os termos)", "idempotencia": "Indexar a fatia duas vezes",
                 "lotes_vs_unico": "`--batch 1` vs. `--batch 1000`", "oraculo_100k": "Oráculo 100k (amostra)",
                 "reabrir_101k": "Reabrir + 1k docs (oráculo 101k)"}
        for k, ok in corr["checagens"].items():
            L.append(f"| {names.get(k, k)} | {'✅ ok' if ok else '❌ divergiu'} |")
        idem = corr.get("idempotencia", {})
        if idem:
            L += ["", f"Tamanho na idempotência: {mb(idem.get('banco_1a'))} MB após uma carga, "
                      f"{mb(idem.get('banco_2a'))} MB após duas, {mb(idem.get('banco_2a_compactado'))} MB depois de compactar."]
        L.append("")
    oracles = [r for r in R if r.get("tipo") == "oracle" and not r.get("rotulo", "").startswith("pr-")]
    seen = {}
    for r in oracles:
        seen[r["rotulo"]] = r
    for label, r in seen.items():
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
    exps = [r for r in R if r.get("tipo") == "load" and r.get("rotulo", "").startswith("exp-")]
    if exps:
        by_label = {}
        for r in exps:
            by_label[r["rotulo"]] = r
        base = latest(R, "load", "ptwiki-100k")
        L += ["### Experimentos (100k)", "", "| Experimento | docs/s | vs. padrão | Pico RSS | Banco |", "|---|---|---|---|---|"]
        if base:
            L.append(f"| padrão (lote 1000) | {fmt(base['docs_por_s'])} | — | {mb(base['pico_rss'])} MB | {mb(base['banco_bytes'])} MB |")
        for label, r in by_label.items():
            vs = f"{pct(r['docs_por_s'], base['docs_por_s']):+.0f}%" if base and "streaming" not in label else "—"
            L.append(f"| {label[4:]} | {fmt(r['docs_por_s'])} | {vs} | {mb(r['pico_rss'])} MB | {mb(r['banco_bytes'])} MB |")
        L.append("")
    sz = latest(R, "sizes", "ptwiki-full") or latest(R, "sizes", "ptwiki-100k")
    if sz:
        L += [f"### Tamanhos (`{sz['rotulo']}`)", "", "| Espaço | Chaves | Chaves (MB) | Valores (MB) |", "|---|---|---|---|"]
        for e in sz["espacos"]:
            L.append(f"| `{e['espaco']}:` | {fmt(e['chaves'])} | {mb(e['bytes_chaves'])} | {mb(e['bytes_valores'])} |")
        L += ["", f"Raiz (`node:0`): {fmt(sz['raiz_bytes'])} bytes. "
              + "Listas: " + ", ".join(f"\"{t}\" {fmt(b / 1024, 0)} KB" for t, b in sz.get("listas", {}).items()), ""]

    # Fase 4
    L += ["## Fase 4: latência de busca (1,1M, cache quente, 1 thread)", ""]
    L += bench_table(latest(R, "bench", "full-t1"))
    pre, post = latest(R, "bench", "full-pre-compactacao"), latest(R, "bench", "full-t1")
    comp = latest(R, "compact", "ptwiki-full")
    if pre and post:
        L += ["### Merge pendente vs. compactado", "", "| Conjunto | p50 antes | p50 depois | p99 antes | p99 depois |", "|---|---|---|---|---|"]
        post_sets = {s["conjunto"]: s for s in post["conjuntos"]}
        for s in pre["conjuntos"]:
            o = post_sets.get(s["conjunto"])
            if o:
                L.append(f"| `{s['conjunto']}` | {us(s['p50_us'])} | {us(o['p50_us'])} | {us(s['p99_us'])} | {us(o['p99_us'])} |")
        L.append("")
    if comp:
        L += [f"Compactação: {dur(comp['segundos'])}, banco de {mb(comp['banco_antes'])} MB → {mb(comp['banco_depois'])} MB. "
              f"Abrir o banco + 1 busca: {fmt(comp['abertura_antes']['mediana_s'] * 1000)} ms antes, "
              f"{fmt(comp['abertura_depois']['mediana_s'] * 1000)} ms depois (mediana de {comp['abertura_depois']['execucoes']}).", ""]
    caches = [(mb_, latest(R, "bench", f"full-cache-{mb_}mb")) for mb_ in (64, 1024)]
    if any(c for _, c in caches) and post:
        L += ["### Block cache", "", "| Conjunto | 64 MB p50 | 256 MB p50 | 1024 MB p50 |", "|---|---|---|---|"]
        post_sets = {s["conjunto"]: s for s in post["conjuntos"]}
        c64 = {s["conjunto"]: s for s in (caches[0][1] or {}).get("conjuntos", [])}
        c1g = {s["conjunto"]: s for s in (caches[1][1] or {}).get("conjuntos", [])}
        for name in c64 or c1g:
            L.append(f"| `{name}` | {us(c64.get(name, {}).get('p50_us'))} | {us(post_sets.get(name, {}).get('p50_us'))} | "
                     f"{us(c1g.get(name, {}).get('p50_us'))} |")
        L.append("")
    cold = latest(R, "bench", "full-t1-frio")
    if cold:
        L += ["### Cache frio", ""] + bench_table(cold)

    # Fase 5
    scaling = [(t, latest(R, "bench", f"full-misto-t{t}")) for t in (1, 2, 4, 8, 12)]
    if any(r for _, r in scaling):
        L += ["## Fase 5: concorrência", "", "### Só leitura (`misto`)", "", "| Threads | qps | ganho vs. 1 | p50 | p99 |", "|---|---|---|---|---|"]
        q1 = next((r["conjuntos"][0]["qps"] for t, r in scaling if t == 1 and r), None)
        for t, r in scaling:
            if r:
                s = r["conjuntos"][0]
                L.append(f"| {t} | {fmt(s['qps'])} | {fmt(s['qps'] / q1, 1) + '×' if q1 else '—'} | {us(s['p50_us'])} | {us(s['p99_us'])} |")
        L.append("")
    conc = [(t, latest(R, "concurrent", f"misto-t{t}")) for t in (1, 4, 8)]
    if any(r for _, r in conc):
        L += ["### Leitura durante escrita (carga dos 112k restantes sobre 1M)", "",
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
    full = latest(R, "load", "ptwiki-full")
    t1 = {s["conjunto"]: s for s in (post or {}).get("conjuntos", [])}
    c4 = latest(R, "concurrent", "misto-t4")
    oracle_divs = sum(r["divergencias"] for r in seen.values()) if seen else None
    swap = machine.get("swap_usado")
    L += ["## Metas (linha de base)", "", "| Métrica | Linha de base | Meta |", "|---|---|---|",
          f"| Divergências do oráculo | {oracle_divs if oracle_divs is not None else '—'} | **0** |",
          f"| Carga total sem swap | {'sim' if full and not swap else ('—' if not full else 'não')} | **sim** |",
          f"| docs/s na carga total | {fmt(full['docs_por_s']) if full else '—'} | |",
          f"| Banco ÷ texto bruto | {fmt(full['banco_por_texto'], 2) if full else '—'} | |",
          f"| p99 de `titulos` (quente) | {us(t1.get('titulos', {}).get('p99_us'))} | |",
          f"| p99 de `t-freq` (quente) | {us(t1.get('t-freq', {}).get('p99_us'))} | |",
          f"| p99 de `misto` com indexação em paralelo | {us(c4['leitura_com_escrita']['p99_us']) if c4 else '—'} | |",
          f"| Tempo de abertura após a carga total | {fmt(comp['abertura_antes']['mediana_s'] * 1000) + ' ms' if comp else '—'} | |",
          ""]

    with open(REPORT, "w") as f:
        f.write("\n".join(L))
    print(f"relatório: {REPORT}")


# ------------------------------------------------------------------------ main

def main():
    p = argparse.ArgumentParser(description="Plano de performance do fire-flower")
    sub = p.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("escada", help="Fase 2")
    e.add_argument("--ate", choices=ORDER, default="full")
    e.add_argument("--experimentos", action="store_true")
    sub.add_parser("corretude", help="Fase 1")
    sub.add_parser("consultas", help="Fase 3")
    sub.add_parser("latencia", help="Fase 4")
    sub.add_parser("concorrencia", help="Fase 5")
    pr = sub.add_parser("pr", help="Fase 8")
    pr.add_argument("--salvar-baseline", action="store_true")
    sub.add_parser("relatorio")
    c = sub.add_parser("comparar")
    c.add_argument("a")
    c.add_argument("b")
    t = sub.add_parser("tudo", help="todas as fases, na ordem")
    t.add_argument("--sem-concorrencia", action="store_true", help="pula a Fase 5 (economiza ~20 min)")
    args = p.parse_args()

    if args.cmd == "comparar":
        return cmd_comparar(args)
    if args.cmd == "relatorio":
        return cmd_relatorio(args)
    build()
    started = time.time()
    ok = True
    if args.cmd == "tudo":
        fase_escada(argparse.Namespace(ate="full", experimentos=True))
        ok &= fase_corretude(args)
        fase_consultas(args)
        fase_latencia(args)
        if not args.sem_concorrencia:
            fase_concorrencia(args)
        ok &= fase_pr(argparse.Namespace(salvar_baseline=True))
    else:
        result = {"escada": fase_escada, "corretude": fase_corretude, "consultas": fase_consultas,
                  "latencia": fase_latencia, "concorrencia": fase_concorrencia, "pr": fase_pr}[args.cmd](args)
        ok = result is not False
    cmd_relatorio(args)
    status = f"{GREEN}pronto" if ok else f"{RED}terminou com falhas"
    print(f"\n{status} em {dur(time.time() - started)}{RESET}")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
