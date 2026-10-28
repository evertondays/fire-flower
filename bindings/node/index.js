// fire-flower para Node.js, Bun e Deno: a API pública sobre o addon nativo
// (napi-rs). Uma chamada nativa por busca ou por lote; os resultados vêm
// num Float64Array só, fatiado aqui sem cópia. Ver docs/plano-bindings.md.
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

/** Os k melhores de uma busca: ids e notas são visões do mesmo buffer. */
class TopHits {
  constructor(buf, labels) {
    const n = (buf.length - 1) / 2;
    /** Quantos documentos casaram ao todo. */
    this.total = buf[0];
    this.ids = buf.subarray(1, 1 + n);
    this.scores = buf.subarray(1 + n);
    if (labels !== undefined) this.labels = labels;
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
  /** @param {object} handle o NativeEngine */
  constructor(handle) {
    this.#native = handle;
  }

  #native;

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
    let buf;
    try {
      buf = this.#native.searchTopSync(toQuery(query), k);
    } catch (e) {
      rethrow(e);
    }
    return this.#hits(buf, options);
  }

  /** Como searchTopSync, no pool de threads: para consultas pesadas. */
  async searchTop(query, options) {
    const k = options?.k ?? DEFAULT_K;
    let buf;
    try {
      buf = await this.#native.searchTop(toQuery(query), k);
    } catch (e) {
      rethrow(e);
    }
    return this.#hits(buf, options);
  }

  #hits(buf, options) {
    if (!options?.labels) return new TopHits(buf);
    const n = (buf.length - 1) / 2;
    return new TopHits(buf, this.labels(buf.subarray(1, 1 + n)));
  }

  /** Todos os documentos que casam, em ordem de ID. */
  searchSync(query) {
    try {
      return this.#native.searchSync(toQuery(query));
    } catch (e) {
      rethrow(e);
    }
  }

  async search(query) {
    try {
      return await this.#native.search(toQuery(query));
    } catch (e) {
      rethrow(e);
    }
  }

  suggestSync(prefix, limit = 10) {
    try {
      return this.#native.suggestSync(prefix, limit);
    } catch (e) {
      rethrow(e);
    }
  }

  /** Rótulos de vários documentos num MultiGet só (null para quem não tem). */
  labels(ids) {
    try {
      return this.#native.labels(ids instanceof Float64Array ? ids : Float64Array.from(ids));
    } catch (e) {
      rethrow(e);
    }
  }

  /** Adiciona ou substitui documentos num lote, com commit fora do event loop. */
  async index(docs) {
    try {
      return await this.#native.index(docs);
    } catch (e) {
      rethrow(e);
    }
  }

  /** Como index, na hora: FF_BUSY se outro lote estiver aberto. */
  indexSync(docs) {
    try {
      return this.#native.indexSync(docs);
    } catch (e) {
      rethrow(e);
    }
  }

  async delete(ids) {
    try {
      return await this.#native.deleteDocuments(Array.from(ids));
    } catch (e) {
      rethrow(e);
    }
  }

  deleteSync(ids) {
    try {
      return this.#native.deleteDocumentsSync(Array.from(ids));
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
      return await this.#native.compact();
    } catch (e) {
      rethrow(e);
    }
  }

  contains(id) {
    try {
      return this.#native.contains(id);
    } catch (e) {
      rethrow(e);
    }
  }

  stats() {
    try {
      return this.#native.stats();
    } catch (e) {
      rethrow(e);
    }
  }

  /** O snapshot das métricas (com `metrics: true` no open), ou null. */
  metrics() {
    let json;
    try {
      json = this.#native.metricsJson();
    } catch (e) {
      rethrow(e);
    }
    return json == null ? null : JSON.parse(json);
  }

  get readOnly() {
    try {
      return this.#native.readOnly;
    } catch (e) {
      rethrow(e);
    }
  }

  get closed() {
    return this.#native.closed;
  }

  /** Solta o handle; o banco fecha quando o último handle do processo soltar. */
  close() {
    this.#native.close();
  }

  [Symbol.dispose]() {
    this.close();
  }

  /** Mede o custo de uma chamada pela fronteira (harness). */
  noop() {
    this.#native.noop();
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
