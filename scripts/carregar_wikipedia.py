#!/usr/bin/env python3
"""Carrega a Wikipédia em português no fire-flower, com um painel de progresso.

Uso:
    python3 scripts/carregar_wikipedia.py                 # carga completa
    python3 scripts/carregar_wikipedia.py --corpus ~/datasets/ptwiki/ptwiki-10000.jsonl --banco /tmp/w10k
    python3 scripts/carregar_wikipedia.py --pular 450000   # retoma uma carga interrompida

O que faz:
  1. compila o fire-flower em release (rápido se já estiver compilado);
  2. apaga o banco de destino, se existir (pede confirmação; --sim pula);
  3. roda `fire-flower index-jsonl` e mostra o progresso: artigos, velocidade,
     tempo restante, memória e tamanho do banco;
  4. grava um resumo em <banco>.carga.json (linha de base para o plano de performance).

Ctrl+C interrompe. Os lotes já gravados ficam no banco e o script mostra o
comando para continuar de onde parou. Reindexar alguns artigos repetidos ao
retomar não tem problema: indexar o mesmo documento duas vezes não duplica nada.

Só usa a biblioteca padrão do Python (3.8+).
"""
import argparse
import collections
import json
import os
import queue
import shutil
import signal
import subprocess
import sys
import threading
import time

PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BINARY = os.path.join(PROJECT, "target", "release", "fire-flower")

ORANGE = "\x1b[38;5;208m"
BOLD = "\x1b[1m"
DIM = "\x1b[2m"
RED = "\x1b[31m"
GREEN = "\x1b[32m"
RESET = "\x1b[0m"
CLEAR_LINE = "\x1b[2K"
HIDE_CURSOR = "\x1b[?25l"
SHOW_CURSOR = "\x1b[?25h"

REDRAW_SECONDS = 0.5
RATE_WINDOW_SECONDS = 30
DB_SIZE_EVERY_SECONDS = 3


# ------------------------------------------------------------------ formatação

def num(n):
    """1234567 -> '1.234.567'"""
    return f"{int(n):,}".replace(",", ".")


def dec(x, casas=1):
    """1234.5 -> '1.234,5'"""
    s = f"{x:,.{casas}f}"
    return s.replace(",", "_").replace(".", ",").replace("_", ".")


def size(n):
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024 or unit == "TB":
            return f"{dec(n, 0 if unit in ('B', 'KB') else 1)} {unit}"
        n /= 1024


def duration(seconds):
    seconds = int(max(0, seconds))
    h, rest = divmod(seconds, 3600)
    m, s = divmod(rest, 60)
    return f"{h}h{m:02d}m{s:02d}s" if h else f"{m:02d}m{s:02d}s"


def bar(fraction, width):
    fraction = min(max(fraction, 0.0), 1.0)
    full = int(fraction * width)
    return f"{ORANGE}{'█' * full}{DIM}{'░' * (width - full)}{RESET}"


def short(text, width):
    return text if len(text) <= width else text[: max(0, width - 1)] + "…"


# ------------------------------------------------------------------- sistema

def rss_bytes(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) * 1024
    except OSError:
        pass
    return 0


def meminfo():
    info = {}
    try:
        with open("/proc/meminfo") as f:
            for line in f:
                key, value = line.split(":", 1)
                info[key] = int(value.split()[0]) * 1024
    except OSError:
        pass
    return info


def dir_size(path):
    total = 0
    try:
        for entry in os.scandir(path):
            if entry.is_file(follow_symlinks=False):
                total += entry.stat().st_size
    except OSError:
        pass
    return total


def count_lines(path):
    lines = 0
    with open(path, "rb") as f:
        while True:
            chunk = f.read(16 << 20)
            if not chunk:
                return lines
            lines += chunk.count(b"\n")


def git_commit():
    try:
        out = subprocess.run(["git", "-C", PROJECT, "rev-parse", "--short", "HEAD"],
                             capture_output=True, text=True, check=True)
        dirty = subprocess.run(["git", "-C", PROJECT, "status", "--porcelain"],
                               capture_output=True, text=True).stdout.strip()
        return out.stdout.strip() + ("-modificado" if dirty else "")
    except (OSError, subprocess.CalledProcessError):
        return "desconhecido"


# -------------------------------------------------------------------- painel

class Dashboard:
    def __init__(self, corpus, banco, total_lines, skip):
        self.corpus = corpus
        self.banco = banco
        self.total_lines = total_lines
        self.skip = skip
        self.drawn_lines = 0
        self.history = collections.deque()  # (t, documentos, bytes)
        self.peak_rss = 0
        self.swap_start = None
        self.db_size = 0
        self.db_size_at = 0.0

    def rates(self, now, p):
        self.history.append((now, p["documentos"], p["bytes"]))
        while len(self.history) > 2 and now - self.history[0][0] > RATE_WINDOW_SECONDS:
            self.history.popleft()
        t0, d0, b0 = self.history[0]
        dt = now - t0
        if dt < 1:
            return 0.0, 0.0
        return (p["documentos"] - d0) / dt, (p["bytes"] - b0) / dt

    def draw(self, p, pid, elapsed, final=False):
        now = time.time()
        width = min(shutil.get_terminal_size((100, 24)).columns, 110)
        docs_rate, bytes_rate = self.rates(now, p)

        rss = rss_bytes(pid) if not final else 0
        self.peak_rss = max(self.peak_rss, rss)
        mem = meminfo()
        swap_used = mem.get("SwapTotal", 0) - mem.get("SwapFree", 0)
        if self.swap_start is None:
            self.swap_start = swap_used
        if final or now - self.db_size_at > DB_SIZE_EVERY_SECONDS:
            self.db_size = dir_size(self.banco)
            self.db_size_at = now

        total_bytes = max(p["bytes_total"], 1)
        fraction = p["bytes"] / total_bytes
        remaining = (total_bytes - p["bytes"]) / bytes_rate if bytes_rate > 0 else None

        fase = {
            "indexando": f"{GREEN}● indexando{RESET}",
            "gravando": f"{ORANGE}● gravando lote no RocksDB…{RESET}",
            "fim": f"{GREEN}✔ concluído{RESET}",
        }.get(p["fase"], p["fase"])
        bar_width = max(10, width - 32)
        restante = "—" if remaining is None else f"~{duration(remaining)}"
        available = mem.get("MemAvailable", 0)
        mem_warn = f"  {RED}pouca RAM livre!{RESET}" if available and available < 600 << 20 else ""
        swap_grew = swap_used - (self.swap_start or 0)
        swap_warn = f"  {RED}usando swap (+{size(swap_grew)}){RESET}" if swap_grew > 64 << 20 else ""

        lines = [
            f"{ORANGE}{BOLD} ✿ fire-flower{RESET}{BOLD} · carga da Wikipédia{RESET}   {fase}",
            f"{DIM}   corpus{RESET}  {short(self.corpus, width - 12)}",
            f"{DIM}   banco {RESET}  {short(self.banco, width - 12)}",
            "",
            f"   {bar(fraction, bar_width)} {BOLD}{dec(fraction * 100)}%{RESET}",
            f"   {BOLD}{num(p['linhas'])}{RESET} de {num(self.total_lines)} artigos"
            f"{DIM}  ·  {size(p['bytes'])} de {size(total_bytes)}{RESET}",
            "",
            f"{DIM}   velocidade{RESET}  {BOLD}{num(docs_rate)}{RESET} artigos/s  ·  {size(bytes_rate)}/s"
            f"{DIM}  (últimos {RATE_WINDOW_SECONDS}s){RESET}",
            f"{DIM}   tempo     {RESET}  {duration(elapsed)} decorrido  ·  {restante} restante",
            f"{DIM}   memória   {RESET}  {size(rss) if rss else '—'} (pico {size(self.peak_rss)})"
            f"  ·  RAM livre {size(available)}{mem_warn}{swap_warn}",
            f"{DIM}   banco     {RESET}  {size(self.db_size)} em disco  ·  lotes gravados: {num(p['commits'])}"
            + (f"  ·  {RED}{num(p['invalidas'])} linhas inválidas{RESET}" if p["invalidas"] else ""),
            f"{DIM}   último    {RESET}  {short(p['ultimo_titulo'] or '—', width - 16)}",
            "",
            f"{DIM}   Ctrl+C interrompe; o que já foi gravado fica no banco.{RESET}" if not final else "",
        ]
        out = []
        if self.drawn_lines:
            out.append(f"\x1b[{self.drawn_lines}F")
        for line in lines:
            out.append(CLEAR_LINE + line + "\n")
        sys.stdout.write("".join(out))
        sys.stdout.flush()
        self.drawn_lines = len(lines)


# ---------------------------------------------------------------------- carga

def build():
    print(f"{DIM}compilando o fire-flower (release)…{RESET}", flush=True)
    result = subprocess.run(["cargo", "build", "--release", "-j", "4"], cwd=PROJECT,
                            capture_output=True, text=True)
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        sys.exit("falha ao compilar")


def confirm_delete(banco, assume_yes):
    if not os.path.exists(banco):
        return
    print(f"o banco {BOLD}{banco}{RESET} já existe ({size(dir_size(banco))}) e será apagado.")
    if not assume_yes:
        answer = input("apagar e começar do zero? [s/N] ").strip().lower()
        if answer not in ("s", "sim", "y", "yes"):
            sys.exit("cancelado; use --pular N para retomar uma carga em vez de apagar")
    shutil.rmtree(banco)
    print(f"{DIM}banco apagado{RESET}")


def reader_thread(stream, events):
    for line in stream:
        try:
            events.put(json.loads(line))
        except ValueError:
            pass
    events.put(None)


def main():
    parser = argparse.ArgumentParser(description="Carrega a Wikipédia no fire-flower com painel de progresso.")
    parser.add_argument("--corpus", default="~/datasets/ptwiki/ptwiki.jsonl", help="arquivo .jsonl")
    parser.add_argument("--banco", default="~/bench-db/ptwiki-full", help="pasta do banco")
    parser.add_argument("--lote", type=int, default=10000,
                        help="artigos por commit (padrão: 10000; ~1,4 GB de pico na Wikipédia inteira)")
    parser.add_argument("--pular", type=int, default=0,
                        help="retoma a partir desta linha, sem apagar o banco")
    parser.add_argument("--sem-compactar", action="store_true",
                        help="não compacta no fim (compactar leva ~10 s e deixa as buscas 30–50%% mais rápidas)")
    parser.add_argument("--sim", action="store_true", help="não pede confirmação para apagar o banco")
    parser.add_argument("--sem-build", action="store_true", help="não roda o cargo build")
    parser.add_argument("--idiomas", help="idiomas das stopwords de um banco novo: pt,en (padrão), en, nenhum…")
    parser.add_argument("--sem-posicoes", action="store_true",
                        help="cria o banco sem posições: menor e mais rápido, mas sem busca por frase")
    parser.add_argument("--stemming", choices=["pt", "en", "nenhum"],
                        help="stemming de um banco novo (padrão: nenhum); \"correndo\" acha \"corrida\"")
    args = parser.parse_args()

    corpus = os.path.expanduser(args.corpus)
    banco = os.path.expanduser(args.banco)
    if not os.path.isfile(corpus):
        sys.exit(f"corpus não encontrado: {corpus}")

    if not args.sem_build:
        build()
    if args.pular:
        print(f"retomando a partir da linha {num(args.pular)}; o banco existente será mantido")
    else:
        confirm_delete(banco, args.sim)
    os.makedirs(os.path.dirname(banco) or ".", exist_ok=True)

    print(f"{DIM}contando artigos…{RESET}", flush=True)
    total_lines = count_lines(corpus)
    log_path = banco.rstrip("/") + ".carga.log"

    cmd = [BINARY, "index-jsonl", banco, corpus, "--lote", str(args.lote),
           "--pular", str(args.pular), "--progresso-json"]
    if args.idiomas:
        cmd += ["--idiomas", args.idiomas]
    if args.sem_posicoes:
        cmd += ["--sem-posicoes"]
    if args.stemming:
        cmd += ["--stemming", args.stemming]
    log = open(log_path, "w")
    # Processo próprio: o Ctrl+C chega só ao script, que encerra o motor com
    # SIGINT e ainda consegue mostrar o resumo.
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=log, text=True,
                            start_new_session=True)
    events = queue.Queue()
    threading.Thread(target=reader_thread, args=(proc.stdout, events), daemon=True).start()

    dashboard = Dashboard(corpus, banco, total_lines, args.pular)
    started = time.time()
    last = {"fase": "indexando", "linhas": args.pular, "documentos": 0, "invalidas": 0, "bytes": 0,
            "bytes_total": os.path.getsize(corpus), "linhas_gravadas": args.pular, "commits": 0,
            "segundos": 0.0, "ultimo_titulo": ""}
    interrupted = False
    sys.stdout.write(HIDE_CURSOR)
    try:
        finished = False
        while not finished:
            deadline = time.time() + REDRAW_SECONDS
            while True:
                try:
                    event = events.get(timeout=max(0.0, deadline - time.time()))
                except queue.Empty:
                    break
                if event is None:
                    finished = True
                    break
                last = event
            dashboard.draw(last, proc.pid, time.time() - started)
    except KeyboardInterrupt:
        interrupted = True
        proc.send_signal(signal.SIGINT)
    finally:
        code = proc.wait()
        log.close()
        sys.stdout.write(SHOW_CURSOR)
        sys.stdout.flush()

    elapsed = time.time() - started
    if interrupted or code != 0:
        print()
        if interrupted:
            print(f"{ORANGE}interrompido.{RESET} {num(last['linhas_gravadas'])} linhas já estão gravadas no banco.")
        else:
            print(f"{RED}o fire-flower terminou com erro (código {code}).{RESET} Veja {log_path}")
        print("para continuar de onde parou:")
        print(f"  python3 {os.path.relpath(__file__)} --banco {banco} --corpus {corpus} "
              f"--lote {args.lote} --pular {last['linhas_gravadas']}")
        sys.exit(1)

    dashboard.draw(last, proc.pid, elapsed, final=True)
    summary = {
        "commit": git_commit(),
        "corpus": corpus,
        "banco": banco,
        "lote": args.lote,
        "pular": args.pular,
        "artigos": last["documentos"],
        "linhas_invalidas": last["invalidas"],
        "segundos": round(elapsed, 1),
        "artigos_por_segundo": round(last["documentos"] / max(elapsed, 0.001), 1),
        "mb_por_segundo": round((last["bytes"] / (1 << 20)) / max(elapsed, 0.001), 2),
        "pico_rss_bytes": dashboard.peak_rss,
        "banco_bytes": dashboard.db_size,
        "corpus_bytes": last["bytes_total"],
    }

    if not args.sem_compactar:
        print(f"{DIM}compactando…{RESET}", flush=True)
        t = time.time()
        subprocess.run([BINARY, "compact", banco], check=True, stdout=subprocess.DEVNULL)
        summary["compactacao_segundos"] = round(time.time() - t, 1)
        summary["banco_bytes_compactado"] = dir_size(banco)
        print(f"compactado em {duration(summary['compactacao_segundos'])}: "
              f"{size(summary['banco_bytes'])} → {size(summary['banco_bytes_compactado'])}")

    summary_path = banco.rstrip("/") + ".carga.json"
    with open(summary_path, "w") as f:
        json.dump(summary, f, indent=2, ensure_ascii=False)

    print(f"{GREEN}pronto:{RESET} {num(last['documentos'])} artigos em {duration(elapsed)} "
          f"({num(summary['artigos_por_segundo'])} artigos/s). Resumo em {summary_path}")
    print("para buscar:")
    print(f"  {BINARY} tui {banco}")


if __name__ == "__main__":
    main()
