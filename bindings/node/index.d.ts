// Tipos do @fire-flower/node.

export type Language = "pt" | "en";
export type StemmingCode = "off" | "none" | "pt" | "pt-plural" | "en" | "en-plural";

export interface OpenOptions {
  /** Stopwords de um banco novo (padrão: pt e en). */
  languages?: Language[];
  /** Grava posições (busca por frase). Padrão: sim, num banco novo. */
  positions?: boolean;
  stemming?: StemmingCode;
  inflections?: StemmingCode;
  blockCacheBytes?: number;
  /** Abre só para leitura, ao lado do processo que grava. */
  readOnly?: boolean;
  /**
   * Com readOnly: confere a pasta a cada `refreshMs` e reabre se o banco
   * mudou (outro processo gravou), no pool de threads.
   */
  refreshMs?: number;
  /** Cria o banco se a pasta não tiver um (padrão: sim; nunca com readOnly). */
  createIfMissing?: boolean;
  /** Liga as métricas do motor (ver `engine.metrics()`). */
  metrics?: boolean;
}

export interface Doc {
  /** Inteiro de 0 a 2^40 − 1. Um ID que já existe é substituído. */
  id: number;
  title?: string;
  /** Texto, ou um Buffer/Uint8Array em UTF-8 (sem conversão para string). */
  text: string | Uint8Array;
  label?: string;
}

export interface SearchOptions {
  /** Quantos resultados (padrão 10). */
  k?: number;
  /** Lê os rótulos dos resultados (um MultiGet só). */
  labels?: boolean;
}

export interface Hit {
  id: number;
  score: number;
  label?: string | null;
}

export declare class TopHits implements Iterable<Hit> {
  /** Quantos documentos casaram ao todo. */
  readonly total: number;
  /** IDs, da maior nota para a menor. */
  readonly ids: number[];
  readonly scores: number[];
  /** Com `labels: true`. */
  readonly labels?: (string | null)[];
  readonly length: number;
  [Symbol.iterator](): Iterator<Hit>;
}

export interface IndexResult {
  /** Documentos adicionados ou substituídos. */
  docs: number;
  replaced: number;
  deleted: number;
  seconds: number;
}

export interface CompactResult {
  purged: number;
  emptiedWords: number;
}

export interface IndexStats {
  docCount: number;
  totalLen: number;
  totalTitleLen: number;
}

export type ErrorCode =
  | "FF_DB"
  | "FF_CORRUPT"
  | "FF_CONFIG"
  | "FF_OPEN"
  | "FF_READ_ONLY"
  | "FF_BUSY"
  | "FF_CLOSED"
  | "FF_ARGUMENT"
  | "FF_PANIC";

export declare class FireFlowerError extends Error {
  readonly code: ErrorCode;
}

export declare class Engine implements Disposable {
  static open(path: string, options?: OpenOptions): Promise<Engine>;
  static openSync(path: string, options?: OpenOptions): Engine;

  searchTopSync(query: string, options?: SearchOptions): TopHits;
  searchTop(query: string, options?: SearchOptions): Promise<TopHits>;
  searchSync(query: string): Float64Array;
  search(query: string): Promise<Float64Array>;
  suggestSync(prefix: string, limit?: number): string[];
  labels(ids: ArrayLike<number>): (string | null)[];

  index(docs: Doc[]): Promise<IndexResult>;
  indexSync(docs: Doc[]): IndexResult;
  delete(ids: Iterable<number>): Promise<IndexResult>;
  deleteSync(ids: Iterable<number>): IndexResult;
  writer(options?: { batchSize?: number }): Writer;
  compact(): Promise<CompactResult>;
  /** Reabre já se o banco mudou (readOnly com refreshMs); resolve se reabriu. */
  refresh(): Promise<boolean>;

  contains(id: number): boolean;
  stats(): IndexStats;
  metrics(): object | null;
  readonly readOnly: boolean;
  readonly closed: boolean;
  close(): void;
  [Symbol.dispose](): void;
  noop(): void;
}

export declare class Writer implements AsyncDisposable {
  add(doc: Doc): Promise<void>;
  flush(): Promise<IndexResult | null>;
  [Symbol.asyncDispose](): Promise<void>;
}

export declare const q: {
  term(word: string): string;
  prefix(start: string): string;
  phrase(text: string): string;
  and(...parts: string[]): string;
};
