"""Converte a Wikipédia em português (parquet do Hugging Face) em JSONL.

Uso:  python -I ptwiki_corpus.py <dir_dos_parquets> <saida.jsonl>

Cada linha: {"id": <u64>, "title": str, "url": str, "text": str}
O id é o page id da Wikipédia (único e estável). Artigos sem texto são pulados.
Precisa de pyarrow.
"""
import glob
import json
import os
import sys
import time

import pyarrow.parquet as pq


def main(src_dir, out_path):
    files = sorted(glob.glob(os.path.join(src_dir, "*.parquet")))
    if not files:
        sys.exit(f"nenhum .parquet em {src_dir}")
    docs = skipped = text_bytes = 0
    start = time.time()
    tmp = out_path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as out:
        for path in files:
            for batch in pq.ParquetFile(path).iter_batches(batch_size=5000):
                for row in batch.to_pylist():
                    text = (row["text"] or "").strip()
                    if not text:
                        skipped += 1
                        continue
                    doc = {"id": int(row["id"]), "title": row["title"], "url": row["url"], "text": text}
                    out.write(json.dumps(doc, ensure_ascii=False))
                    out.write("\n")
                    docs += 1
                    text_bytes += len(text.encode("utf-8"))
            print(f"{os.path.basename(path)}: {docs:,} docs até agora ({time.time() - start:.0f}s)", file=sys.stderr)
    os.replace(tmp, out_path)
    print(f"total: {docs:,} docs, {skipped:,} vazios, {text_bytes / 1e9:.2f} GB de texto", file=sys.stderr)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
