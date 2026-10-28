# Plano: painel de monitoramento sem impacto

Origem: o documento "fire-flower: painel de monitoramento sem impacto"
(auditoria da TUI, métricas de custo desprezível, exportação OpenMetrics fora
do processo do motor e prova de custo zero pelo A/B do `perf.py`), escrito
sobre o commit `a03cca2`. Este plano o adapta à `v0.2-release`, que já tem a
edição e a remoção de documentos (formato v7, [análise](analise-edicao.md)
plano A), e acrescenta as métricas e as metas de edição e remoção.

## Resumo

- **Hoje a TUI trava o banco.** `fire-flower tui` abre o RocksDB como
  instância primária (leitura e escrita) e segura o `LOCK` enquanto está
  aberta: nenhum indexador consegue abrir o mesmo banco ao mesmo tempo. O
  mesmo vale para `search` e `suggest`.
- **Correção imediata:** TUI, `search` e `suggest` abrem só para leitura, sem
  criar banco e com cache menor.
- **Só entram métricas de custo desprezível:** contadores e histogramas
  atômicos por thread no motor, `/proc` do processo, propriedades baratas do
  RocksDB (sem `enable_statistics`) e o tamanho da pasta.
- **Tudo que tem custo mensurável fica de fora**, numa lista explícita.
- **OpenMetrics por outro processo:** `fire-flower monitor --serve` lê o
  arquivo de snapshot que o motor grava; o scrape nunca toca o RocksDB nem o
  processo do motor.
- **Prova:** A/B do `perf.py` com as métricas desligadas, ligadas e com
  scrape a cada 1 s, mais uma feature de compilação que tira tudo. Inclui a
  fase de edição: editar e remover não podem ficar mais lentos.

## Auditoria da TUI atual

| Achado | Onde | Risco |
|---|---|---|
| Abre como primário, com `LOCK` exclusivo | `main.rs`: `Command::Tui` → `open()` → `Engine::open` → `DB::open_cf_descriptors` | Alto: trava o banco |
| `create_if_missing(true)` | `Engine::open` | Médio: caminho errado cria banco vazio |
| Instância primária pode gravar | recuperação do WAL, flush, compactação em segundo plano com `increase_parallelism(cpus)`, grava `LOG`/`OPTIONS`/`MANIFEST` | Médio: CPU e disco sem a TUI pedir |
| Memória própria | `Config::default()`: block cache de 256 MiB | Médio: disputa RAM com a aplicação |
| Busca a cada tecla | `App::refresh`: `suggest(8)` + `search_ranked(.., 1000)`, com a última palavra virando prefixo | Médio: prefixo curto é a consulta mais cara (`p-2`) |
| Rótulos e prévia | `App::label` (um `Get` por linha visível, com cache) e `load_preview` | Baixo |
| Sem polling | `event_loop` bloqueia em `event::read()` | Bom: parada, não gasta CPU |
| Mesmo problema em `search`, `suggest`, `delete` e `compact` | todos passam por `open()` em `main.rs` | Alto para `search`/`suggest` |

### Fase 0: TUI segura

1. **Somente leitura:** `Engine::open_read_only` com
   `DB::open_cf_descriptors_read_only`. Várias instâncias convivem com o
   primário, nunca criam arquivos e aceitam leitura por snapshot (o `View` usa
   snapshot em toda busca). Um lote ou uma compactação num banco aberto assim
   dá erro.
2. **Nunca criar banco** fora de `index`/`index-jsonl`
   (`Config::create_if_missing`).
3. **Config enxuta** para a TUI, `search` e `suggest`: block cache de 32 MB,
   sem threads extras do RocksDB.
4. **Dados novos:** a visão somente leitura é estática. A TUI reabre o banco
   com Ctrl+R e sozinha quando a visão tem mais de 10 s e chega uma tecla
   (abrir + 1 busca custa ~28 ms no 100k).
5. **Educação na busca por tecla:** espera curta (debounce) quando a última
   palavra tem 1 ou 2 letras.

A instância secundária do RocksDB não serve: acompanha o primário com
`TryCatchUpWithPrimary`, mas não aceita leitura por snapshot.

## Métricas incluídas

Critério: no caminho quente, só incrementos atômicos relaxados num shard da
própria thread, mais duas leituras de relógio por consulta (para o
histograma). Todo o resto é lido fora dele: pela thread amostradora a cada
1–5 s ou pelo processo monitor na hora do scrape. Nomes nas convenções do
OpenMetrics: unidade base no nome, `_total` em contadores, `info` para
metadados.

### Motor (caminho quente)

| Nome | Tipo | Labels |
|---|---|---|
| `fireflower_queries_total` | counter | `op` |
| `fireflower_query_errors_total` | counter | `op` |
| `fireflower_query_duration_seconds` | histogram | `op` |
| `fireflower_engine_reads_total` | counter | `op` |
| `fireflower_epoch_cache_lookups_total` | counter | `cache` = node\|norm, `result` = hit\|miss |

`op` é o ponto de entrada da API (`search`, `search_top`, `search_ranked`,
`suggest`, `postings`, `term_postings`), conhecido de graça na chamada.

### Indexação, edição e remoção (uma vez por commit)

| Nome | Tipo | Labels |
|---|---|---|
| `fireflower_commits_total` | counter | — |
| `fireflower_commit_errors_total` | counter | — |
| `fireflower_commit_duration_seconds` | histogram | — |
| `fireflower_commit_phase_seconds_total` | counter | `phase` = sort\|trie\|encode\|documents\|nodes\|write\|evict |
| `fireflower_indexed_documents_total` | counter | — |
| `fireflower_replaced_documents_total` | counter | — |
| `fireflower_deleted_documents_total` | counter | — |
| `fireflower_killed_versions_total` | counter | — |
| `fireflower_commit_batch_bytes_total` | counter | — |
| `fireflower_writer_lock_wait_seconds_total` | counter | — |
| `fireflower_writer_cache_nodes` | gauge | — |
| `fireflower_documents` | gauge | — |
| `fireflower_dead_versions` | gauge | — |
| `fireflower_pending_edits` | gauge | — |

As cinco de edição e remoção (`replaced`, `deleted`, `killed`, `dead`,
`pending_edits`) mostram quanto o banco acumulou de versões mortas e quando
compactar. Vêm do `CommitStats` que o commit já calcula: são somas no fim do
commit, sem nada no caminho de cada documento.

### Compactação

| Nome | Tipo |
|---|---|
| `fireflower_compactions_total` | counter |
| `fireflower_compaction_seconds_total` | counter |
| `fireflower_purged_versions_total` | counter |
| `fireflower_emptied_words_total` | counter |

### RocksDB (propriedades, sem estatísticas)

Lidas pela thread amostradora a cada 5 s, fora do caminho quente:

| Nome | Tipo | Labels |
|---|---|---|
| `fireflower_rocksdb_sst_files` | gauge | `level` |
| `fireflower_rocksdb_sst_size_bytes` | gauge | `state` = live\|total |
| `fireflower_rocksdb_pending_compaction_bytes` | gauge | — |
| `fireflower_rocksdb_running_compactions` | gauge | — |
| `fireflower_rocksdb_running_flushes` | gauge | — |
| `fireflower_rocksdb_memtable_flush_pending` | gauge | — |
| `fireflower_rocksdb_write_stopped` | gauge | — |
| `fireflower_rocksdb_delayed_write_rate_bytes_per_second` | gauge | — |
| `fireflower_rocksdb_background_errors_total` | counter | — |
| `fireflower_rocksdb_memory_bytes` | gauge | `component` = memtables\|table_readers\|block_cache\|block_cache_pinned |
| `fireflower_rocksdb_block_cache_capacity_bytes` | gauge | — |
| `fireflower_rocksdb_snapshots` | gauge | — |
| `fireflower_rocksdb_oldest_snapshot_age_seconds` | gauge | — |

Se alguma dessas propriedades aparecer no A/B (algumas seguram o mutex do DB
por microssegundos), ela sai da lista.

### Processo e disco (lidos pelo monitor)

| Nome | Tipo |
|---|---|
| `process_cpu_seconds_total` | counter |
| `process_resident_memory_bytes` | gauge |
| `process_virtual_memory_bytes` | gauge |
| `process_open_fds`, `process_max_fds` | gauge |
| `process_start_time_seconds` | gauge |
| `process_threads` (não padrão) | gauge |
| `process_io_read_bytes_total`, `process_io_write_bytes_total` (não padrão) | counter |
| `fireflower_disk_index_bytes` | gauge, `kind` = sst\|wal\|other |
| `fireflower_filesystem_free_bytes`, `fireflower_filesystem_size_bytes` | gauge |
| `fireflower_build_info` | info: `version`, `format_version` |
| `fireflower_exporter_snapshot_age_seconds` | gauge |

## Excluídas e por quê

| Fora | Por quê |
|---|---|
| Estatísticas do RocksDB em qualquer nível | incrementos dentro de cada `Get`/`MultiGet` e no block cache; a wiki do RocksDB fala em 5% a 10% de custo |
| Hit rate do block cache, bloom útil, bytes lidos/escritos pelo RocksDB, write amp, stalls | são tickers: só existem com estatísticas ligadas |
| Histogramas do RocksDB (`db.get.micros`…) | timers no caminho de leitura |
| `PerfContext` e `IOStatsContext` | timers por thread em cada leitura |
| `rocksdb.stats`, `cfstats`, `levelstats`, `block-cache-entry-stats` | geram strings grandes ou varrem o cache |
| `Engine::stats()` no amostrador | 3 `Get`s por snapshot; o número de documentos vem do commit |
| Consultas de sonda ou canário | gastam CPU, cache e disco do motor só para medir |
| Log de consultas lentas com o texto | copiar a string vai além de um incremento |
| Labels de alta cardinalidade (consulta, documento) | memória e custo no Prometheus |
| `/proc/<pid>/smaps` | o kernel segura o `mmap_lock` do processo do motor |
| `du` recursivo, `mincore` nos SSTs | I/O e CPU proporcionais ao índice |
| Abrir o RocksDB no monitor, mesmo só para leitura | replay do WAL, memória e page cache só para medir |
| Contar os documentos mortos na busca | um contador por entrada pulada: a busca não conta nada por documento |

## Arquitetura sem impacto

1. **Caminho quente:** só `fetch_add(Relaxed)` num shard da própria thread
   (alinhado a 128 B, para não haver ping-pong de linha de cache entre
   núcleos) e duas leituras de `Instant` por consulta. Os acertos do cache de
   época são contados em `Cell`s da própria busca e somados ao shard uma vez,
   no fim dela. Sem lock, alocação nem syscall.
2. **Thread amostradora no processo dono**, opt-in (`Config::metrics`): soma
   os shards a cada 1 s e lê as propriedades do RocksDB a cada 5 s.
3. **Arquivo de snapshot:** JSON gravado num arquivo temporário e renomeado
   sobre o anterior a cada segundo. *Desvio do documento original*, que pedia
   um arquivo mapeado em memória com seqlock: a troca por `rename` dá a mesma
   garantia (o leitor nunca vê uma escrita pela metade) sem `unsafe` nem
   dependência nova; custa uma escrita de poucos KB por segundo na thread
   amostradora. Ninguém fala com o processo do motor.
4. **Processo monitor:** lê o snapshot, o `/proc` do pid gravado nele e a
   pasta do banco. Nunca abre o RocksDB.
5. **Desligar de verdade:** feature de Cargo `metrics` (ligada por padrão).
   Sem ela, os ganchos somem na compilação e `Config::metrics` dá erro.

## Exportação OpenMetrics

`fire-flower monitor <banco> --serve 127.0.0.1:9187`: o HTTP roda no processo
monitor e só lê o snapshot e o `/proc`. O processo do motor não ganha socket,
thread de HTTP nem dependência nova.

1. O scrape chega ao monitor.
2. Ele lê o snapshot (poucos KB).
3. Lê `/proc/<pid>/stat`, `status`, `io`, `limits` e conta os fds; o tamanho
   da pasta e o espaço livre ficam em cache por até 30 s.
4. Responde `application/openmetrics-text; version=1.0.0`, ou texto do
   Prometheus 0.0.4 se o `Accept` não pedir OpenMetrics.
5. Com o motor parado, continua respondendo e
   `fireflower_exporter_snapshot_age_seconds` cresce.

## O painel

`fire-flower monitor <banco>`: mesma stack (ratatui) e paleta da TUI de
busca, os mesmos dados do `/metrics`. Painéis de processo, busca (qps, p50 e
p99 por `op`, Gets por consulta, acerto dos caches), indexação (docs/s,
commits, espera do lote), edição e remoção (substituídos/s, removidos/s,
versões mortas e edições pendentes), RocksDB, memória e disco. Atualiza no
ritmo do snapshot; p50 e p99 são estimados pelos baldes do histograma;
sparklines guardadas no painel (o motor não guarda histórico). Alertas:
escrita parada, erros de fundo, L0 alto, compactação pendente crescendo,
disco quase cheio, snapshot velho, muitas versões mortas. `--json` imprime
uma leitura e sai.

## Como provar impacto zero

| Medida | Meta |
|---|---|
| Custo do registro por consulta (microbench, 1 e 12 threads) | ≤ 50 ns |
| p50 de `t-miss` e `t-mid` (A/B) | diferença ≤ 1% |
| Todos os conjuntos do `perf.py all` | nenhum pior além do ruído de ±5% |
| Indexação (docs/s, lote 10k) | ≥ 99,5% da base |
| p99 de `mixed` com 4 leitores e indexação | sem piora |
| CPU da thread amostradora | ≤ 0,1% de um núcleo |
| Com scrape a cada 1 s e o painel aberto | igual a sem scrape |

### Metas de edição e remoção

Medidas pela fase `edits` do `perf.py` (100k), nas mesmas variantes:

| Medida | Meta |
|---|---|
| Vazão das edições, lote de 10 mil (substituir 50%, remover 10%, readicionar 1%) | ≥ 99,5% da base |
| Vazão das edições, lotes de 100 e de 1 | sem piora além do ruído |
| Commit p50 e p99 de edições de 1 documento | sem piora além do ruído |
| Latência das buscas com 1.000 edições, antes de compactar | sem piora além do ruído |
| Latência no banco editado, antes e depois de compactar | sem piora além do ruído |
| Tempo da compactação com versões mortas | sem piora além do ruído |
| Buscas com edições em paralelo (p99 e qps, `mixed`, 4 leitores) | sem piora |
| Custo do registro por commit | ≤ 1 µs (o commit de 1 documento leva ~200 µs) |
| `fire-flower delete` de 1.000 IDs | sem piora além do ruído |

Como provar:

1. Microbench no `benches/micro.rs`: o registro de uma consulta sozinho e com
   12 threads, e o de um commit.
2. A/B (seção 11 das diretrizes) em quatro variantes: a `v0.2-release`, a
   branch sem a feature, com a feature e as métricas desligadas, e com as
   métricas ligadas, o scrape a cada 1 s e o painel aberto.
3. Repetir alternando os binários antes de aceitar qualquer diferença.
4. Uma métrica incluída que aparecer no A/B sai para a lista de excluídas.
5. Teste de integração: indexação rodando com TUI e monitor abertos ao mesmo
   tempo, sem erro de `LOCK`.
6. Resultado e análise no `fire-flower-bench`.

## Roadmap

1. **Fase 0: TUI segura.** `open_read_only` para `tui`, `search` e `suggest`;
   sem `create_if_missing` fora da indexação; cache menor; teste de leitura
   com indexação junto.
2. **Fase 1: métricas no motor.** Módulo `metrics` com shards e histograma de
   baldes fixos; ganchos nas consultas, no commit (inclusive edição e
   remoção), no `batch()`, no cache de época e na compactação; feature
   `metrics`; microbench.
3. **Fase 2: amostrador e snapshot.** Thread opt-in, propriedades baratas do
   RocksDB a cada 5 s, arquivo de snapshot versionado.
4. **Fase 3: `monitor --serve` (OpenMetrics).** Nomes estáveis da lista
   acima, `/proc` e disco, negociação de formato, teste com
   `promtool check metrics`.
5. **Fase 4: painel TUI.** `fire-flower monitor` com painéis, sparklines,
   alertas e `--json`.
6. **Fase 5: exemplos.** Dashboard do Grafana e regras de alerta do
   Prometheus sobre as mesmas métricas.

Fora do roadmap: estatísticas do RocksDB em qualquer nível. Só voltam com um
A/B próprio e decisão explícita.

## Decisões sobre as perguntas em aberto

1. **Quem é dono do banco:** quem abre o `Engine` para escrita com
   `Config::metrics`. A amostradora vive nesse processo.
2. **Container:** o monitor lê o `/proc` do pid gravado no snapshot; sem
   acesso a ele (outro PID namespace), as métricas de processo somem e o
   resto continua.
3. **Vários bancos no mesmo processo:** um snapshot por banco (por padrão,
   dentro da pasta dele); um monitor por banco.
4. **Só Linux:** fora do Linux ficam só as métricas do motor e do RocksDB.
5. **Baldes do histograma das consultas:** 16, de 5 µs a 1 s (a TUI de
   prefixo chega a 30 ms no p99). Commits têm baldes próprios, de 1 ms a
   100 s.
6. **Onde fica o snapshot:** `fire-flower.metrics.json` dentro da pasta do
   banco, ou `Config::metrics_file`.
7. **Exposição:** `127.0.0.1` por padrão; outro endereço só se pedido
   (autenticação fica com quem estiver na frente).
8. **TUI e dados novos:** reabrir com Ctrl+R ou sozinha depois de 10 s.

## Resultado (28/10/2026)

As seis fases estão implementadas. A/B completo (`perf.py all --scale full` e
`perf.py edits --scale 100k`) em cinco variantes, uma depois da outra: a
`v0.2-release`, a branch sem a feature `metrics`, com a feature e as métricas
desligadas (o padrão), ligadas, e ligadas com scrape do `/metrics` e
`monitor --json` a cada segundo (o painel não roda sem terminal; ele lê os
mesmos dados). Zero divergências nos cinco oráculos das cinco variantes.

As diferenças acima de 1% foram repetidas alternando os binários, com bisect
pelos commits e, por fim, com builds de alinhamento fixo
(`-C llvm-args=-align-all-functions=6 -align-all-nofallthru-blocks=6`), que
tiram o efeito do alinhamento do código.

| Medida | Meta | Resultado |
|---|---|---|
| Registro de uma consulta (microbench) | ≤ 50 ns | 6,9 ns; 8,0 ns com 12 threads; 38 ns numa consulta de verdade (relógio, leituras, registro) |
| p50 de `t-miss` e `t-mid` | ≤ 1% | alinhamento fixo, ligada contra sem a feature: −1,0% e +1,6% (+240 ns), com o padrão desligado em −12,8% e −1,9%: abaixo do ruído |
| Todos os conjuntos | sem piora além de ±5% | alinhamento fixo: entre −5,6% e +1,6% nos 12 conjuntos; o +5% do `t-freq` no A/B é alinhamento (aparece na Fase 2 sem a feature e some com o alinhamento fixo) |
| Indexação (docs/s, lote de 10 mil) | ≥ 99,5% | alternando: −1,3% (inteira) e −0,7% (100k) ligada; a variante sem a feature, sem código de métricas, ficou em −1,9% e −2,6% numa rodada e −0,2% na seguinte: o ruído entre rodadas é de ±2%, maior que a meta |
| p99 de `mixed` com 4 leitores e indexação | sem piora | −4,0% ligada, −1,3% com scrape |
| CPU da amostradora | ≤ 0,1% de um núcleo | 0,016% (0,02 s em 123 s de carga da Wikipédia inteira) |
| Scrape a cada 1 s e monitor lendo | igual a sem scrape | carga inteira −0,3% contra a ligada; latência e concorrência dentro do ruído |

### Edição e remoção

| Medida | Meta | Ligada | Ligada + scrape |
|---|---|---|---|
| Vazão, lote de 10 mil | ≥ 99,5% | +2,8% | +0,7% |
| Vazão, lotes de 100 e de 1 | sem piora além do ruído | −0,2% e +1,2% | −0,3% e +0,2% |
| Commit p50/p99 de 1 documento | sem piora além do ruído | 0,29/6,05 ms (base 0,29/6,00) | 0,29/5,93 ms |
| Latência com 1.000 edições, antes de compactar | sem piora além do ruído | −0,1% a +7,1% (`s-1`, 7 µs) | −1,2% a +8,4% (`rank-freq`; a variante sem a feature dá +6,2%) |
| Banco editado, antes e depois de compactar | sem piora além do ruído | dentro de ±5%, menos conjuntos de µs que oscilam igual nas variantes sem métricas | idem |
| Compactação com versões mortas | sem piora além do ruído | +2,6% | +1,0% |
| Buscas com edições em paralelo (p99, `mixed`, 4 leitores) | sem piora | −0,1% | −1,9% |
| Registro por commit | ≤ 1 µs | 26 ns | — |
| `fire-flower delete` de 1.000 IDs | sem piora além do ruído | 31 ms (base 31–33 ms) | — |

**Fora do previsto:**

- a TUI e o painel não desenham num pseudo-terminal sem resposta (`script`):
  o painel foi testado com o terminal virtual do ratatui;
- `promtool check metrics` e `check rules` não rodaram aqui; o formato é
  conferido pelos testes do monitor.
