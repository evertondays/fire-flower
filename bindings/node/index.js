// fire-flower para Node.js, Bun e Deno: a API pública sobre o addon nativo
// (napi-rs). Uma chamada nativa por busca ou por lote. A busca síncrona
// escreve num buffer de trabalho do Engine (criar um Float64Array por busca
// custa ~0,5 µs no Node 24) e os k resultados viram arrays JS comuns (~30 ns).
// Ver docs/plano-bindings.md.
"use strict";

const native = loadNative();

function loadNative() {
  const candidates = [
    process.env.FIRE_FLOWER_NODE_ADDON,
    `./fire-flower.${process.platform}-${process.arch}.node`,
    "./fire-flower.node",
  ].filter(Boolean);
  let first;
  for (const file of candidates) {
    try {
      return require(file);
    } catch (e) {
      first ??= e;
    }
  }
  throw first;
}

/** Erro do fire-flower: `code` é estável (FF_BUSY, FF_OPEN, FF_READ_ONLY…). */
class FireFlowerError extends Error {
  constructor(code, message) {
    super(message);
    this.name = "FireFlowerError";
    this.code = code;
  }
}

const CODED = /^(FF_[A-Z_]+): ([\s\S]*)$/;

/** O addon lança `FF_CÓDIGO: mensagem`; vira um FireFlowerError. */
function rethrow(e) {
  const m = e instanceof Error && CODED.exec(e.message);
  if (m) throw new FireFlowerError(m[1], m[2]);
  throw e;
}

/** Os k melhores de uma busca, da maior nota para a menor. */
class TopHits {
  /** `buf`: `[total, ids…, notas…]` com `n` resultados. */
  constructor(buf, n) {
    /** Quantos documentos casaram ao todo. */
    this.total = buf[0];
    const ids = new Array(n);
    const scores = new Array(n);
    for (let i = 0; i < n; i++) {
      ids[i] = buf[1 + i];
      scores[i] = buf[1 + n + i];
    }
    this.ids = ids;
    this.scores = scores;
  }

  get length() {
    return this.ids.length;
  }

  *[Symbol.iterator]() {
    const { ids, scores, labels } = this;
    for (let i = 0; i < ids.length; i++) {
      yield labels === undefined
        ? { id: ids[i], score: scores[i] }
        : { id: ids[i], score: scores[i], label: labels[i] };
    }
  }
}

const DEFAULT_K = 10;

function toQuery(query) {
  return typeof query === "string" ? query : String(query);
}

class Engine {
  /** @param {object} handle o handle nativo (External) */
  constructor(handle) {
    this.#h = handle;
  }

  #h;
  /** O buffer de trabalho da busca síncrona e dos rótulos (cresce com k). */
  #scratch = new Float64Array(1 + 2 * DEFAULT_K);

  #scratchFor(size) {
    if (this.#scratch.length < size) this.#scratch = new Float64Array(size);
    return this.#scratch;
  }

  /** Abre fora do event loop. O mesmo caminho devolve o mesmo banco nativo. */
  static async open(path, options) {
    try {
      return new Engine(await native.openEngine(path, options));
    } catch (e) {
      rethrow(e);
    }
  }

  static openSync(path, options) {
    try {
      return new Engine(native.openEngineSync(path, options));
    } catch (e) {
      rethrow(e);
    }
  }

  /** Busca ranqueada (BM25) na hora: o caminho rápido. */
  searchTopSync(query, options) {
    const k = options?.k ?? DEFAULT_K;
    const buf = this.#scratchFor(1 + 2 * k);
    let n;
    try {
      n = native.searchTopInto(this.#h, toQuery(query), k, buf);
    } catch (e) {
      rethrow(e);
    }
    return this.#hits(new TopHits(buf, n), options);
  }

  /** Como searchTopSync, no pool de threads: para consultas pesadas. */
  async searchTop(query, options) {
    const k = options?.k ?? DEFAULT_K;
    let buf;
    try {
      buf = await native.searchTop(this.#h, toQuery(query), k);
    } catch (e) {
      rethrow(e);
    }
    return this.#hits(new TopHits(buf, (buf.length - 1) / 2), options);
  }

  #hits(top, options) {
    if (options?.labels) top.labels = this.labels(top.ids);
    return top;
  }

  /** Todos os documentos que casam, em ordem de ID. */
  searchSync(query) {
    try {
      return native.searchSync(this.#h, toQuery(query));
    } catch (e) {
      rethrow(e);
    }
  }

  async search(query) {
    try {
      return await native.search(this.#h, toQuery(query));
    } catch (e) {
      rethrow(e);
    }
  }

  suggestSync(prefix, limit = 10) {
    try {
      return native.suggestSync(this.#h, prefix, limit);
    } catch (e) {
      rethrow(e);
    }
  }

  /** Rótulos de vários documentos num MultiGet só (null para quem não tem). */
  labels(ids) {
    const n = ids.length;
    if (n === 0) return [];
    const buf = this.#scratchFor(n);
    for (let i = 0; i < n; i++) buf[i] = ids[i];
    let out;
    try {
      out = native.labels(this.#h, buf, n);
    } catch (e) {
      rethrow(e);
    }
    if (typeof out !== "string") return out;
    // Uma string só (ver o addon): "\0" separa, "\u0001" é quem não tem rótulo.
    const labels = out.split("\0");
    for (let i = 0; i < labels.length; i++) if (labels[i] === "\u0001") labels[i] = null;
    return labels;
  }

  /** Adiciona ou substitui documentos num lote, com commit fora do event loop. */
  async index(docs) {
    try {
      return await native.index(this.#h, docs);
    } catch (e) {
      rethrow(e);
    }
  }

  /** Como index, na hora: FF_BUSY se outro lote estiver aberto. */
  indexSync(docs) {
    try {
      return native.indexSync(this.#h, docs);
    } catch (e) {
      rethrow(e);
    }
  }

  async delete(ids) {
    try {
      return await native.deleteDocuments(this.#h, Array.from(ids));
    } catch (e) {
      rethrow(e);
    }
  }

  deleteSync(ids) {
    try {
      return native.deleteDocumentsSync(this.#h, Array.from(ids));
    } catch (e) {
      rethrow(e);
    }
  }

  /** Grava em lotes de `batchSize` documentos (padrão 10.000). */
  writer(options) {
    return new Writer(this, options?.batchSize ?? 10_000);
  }

  async compact() {
    try {
      return await native.compact(this.#h);
    } catch (e) {
      rethrow(e);
    }
  }

  contains(id) {
    try {
      return native.contains(this.#h, id);
    } catch (e) {
      rethrow(e);
    }
  }

  stats() {
    try {
      return native.stats(this.#h);
    } catch (e) {
      rethrow(e);
    }
  }

  /** O snapshot das métricas (com `metrics: true` no open), ou null. */
  metrics() {
    let json;
    try {
      json = native.metricsJson(this.#h);
    } catch (e) {
      rethrow(e);
    }
    return json == null ? null : JSON.parse(json);
  }

  get readOnly() {
    try {
      return native.isReadOnly(this.#h);
    } catch (e) {
      rethrow(e);
    }
  }

  get closed() {
    return native.isClosed(this.#h);
  }

  /** Solta o handle; o banco fecha quando o último handle do processo soltar. */
  close() {
    native.close(this.#h);
  }

  [Symbol.dispose]() {
    this.close();
  }

  /** Mede o custo de uma chamada pela fronteira (harness). */
  noop() {
    native.noop(this.#h);
  }
}

/** Acumula documentos e grava a cada `batchSize`, esperando o commit. */
class Writer {
  #engine;
  #batchSize;
  #docs = [];

  constructor(engine, batchSize) {
    this.#engine = engine;
    this.#batchSize = batchSize;
  }

  /** Acrescenta um documento; grava (e espera) quando o lote enche. */
  async add(doc) {
    this.#docs.push(doc);
    if (this.#docs.length >= this.#batchSize) await this.flush();
  }

  /** Grava o que estiver acumulado. */
  async flush() {
    if (this.#docs.length === 0) return null;
    const docs = this.#docs;
    this.#docs = [];
    return this.#engine.index(docs);
  }

  async [Symbol.asyncDispose]() {
    await this.flush();
  }
}

/** Monta consultas sem concatenar strings à mão. */
const q = {
  /** Uma palavra exata. */
  term: (word) => clean(word),
  /** Palavras que começam com `start`. */
  prefix: (start) => `${clean(start)}*`,
  /** As palavras em sequência. */
  phrase: (text) => `"${clean(text)}"`,
  /** Todas as partes (E). */
  and: (...parts) => parts.join(" "),
};

/** Aspas e asteriscos são a sintaxe da consulta: num termo, viram espaço. */
function clean(text) {
  return String(text).replace(/["*]/g, " ").trim();
}

module.exports = { Engine, FireFlowerError, TopHits, Writer, q };
