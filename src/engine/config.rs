//! Configuração do motor ([`Config`]).

use std::path::PathBuf;
use std::time::Duration;

use crate::{Error, Result};
use crate::analysis::stem::Stemming;
use crate::analysis::stopwords::Language;

#[derive(Debug, Clone)]
pub struct Config {
    /// Memória do block cache. Mantém a raiz e os primeiros níveis da Trie,
    /// que todas as buscas atravessam, sempre em RAM.
    pub block_cache_bytes: usize,
    /// Bits por chave do filtro de Bloom. 10 ≈ 1% de falsos positivos.
    pub bloom_bits_per_key: f64,
    /// Quantos termos uma busca por prefixo (`comp*`) expande no máximo.
    /// Termos além disso são ignorados; prefixos curtos podem casar milhares.
    pub max_prefix_terms: usize,
    /// Liga as estatísticas internas do RocksDB ([`Engine::rocksdb_stats`]).
    /// Custa um pouco de CPU; use em medições, não em produção.
    pub statistics: bool,
    /// Quantos nós da Trie o escritor mantém em memória entre um lote e
    /// outro (~130 bytes cada; o cache só cresce até o tamanho da Trie).
    /// Sem isso, todo lote relê do banco os nós dos termos que aparecem.
    /// Passando do limite, ficam os usados mais recentemente. 0 desliga.
    ///
    /// 3 milhões (~400 MB) cobrem 40% da Trie da Wikipédia inteira (7,3 M
    /// nós): a carga cai de 139 s para 115 s com lote de 10 mil, e as
    /// leituras do escritor, de 7,5 para 1,7 por artigo.
    pub writer_cache_nodes: usize,
    /// Até que profundidade da Trie as buscas guardam os nós já decodificados,
    /// compartilhados entre elas (0 = só a raiz). Toda busca começa pela raiz,
    /// que tem um filho por caractere inicial e cresce com o índice (37 KB na
    /// Wikipédia inteira): sem o cache, cada busca a decodificava inteira.
    /// `None` desliga.
    ///
    /// O cache não descarta: guarda no máximo os nós até essa profundidade.
    /// Na Wikipédia inteira são 365 mil nós (~48 MB) até a profundidade 4 (o
    /// padrão; autocompletar de 2 letras de 44 para 11 µs), ~125 MB até a 5 e
    /// ~260 MB até a 6 (prefixos curtos ~30% mais rápidos).
    pub reader_cache_depth: Option<usize>,
    /// Idiomas das stopwords (palavras comuns que não entram no índice).
    ///
    /// Ficam gravados no banco quando ele é criado, porque índice e consulta
    /// precisam descartar as mesmas palavras. `None` usa o que está no banco,
    /// ou [`Language::DEFAULT`] (português e inglês) num banco novo. `Some`
    /// diferente do que está gravado faz o `open` falhar. `Some(vec![])`
    /// cria um banco sem stopwords.
    pub languages: Option<Vec<Language>>,
    /// Grava as posições de cada termo (`pos:`), que a busca por frase usa.
    ///
    /// Como os idiomas, é decidido na criação do banco: `None` usa o que
    /// está gravado (ou liga, num banco novo), e `Some` diferente do gravado
    /// faz o `open` falhar. Sem posições o banco fica menor e a carga mais
    /// rápida, mas frases dão erro.
    pub positions: Option<bool>,
    /// Reduz as palavras ao radical (Snowball) na indexação e na busca:
    /// "correndo" acha "corrida". Ver [`crate::analysis::stem`].
    ///
    /// Como os idiomas, é decidido na criação do banco: `None` usa o que está
    /// gravado (ou [`Stemming::Off`], num banco novo; bancos de antes do
    /// stemming são `Off`), e `Some` diferente do gravado faz o `open` falhar.
    pub stemming: Option<Stemming>,
    /// Flexões na consulta (`docs/plano-stemming.md`, Fase 3): o índice guarda
    /// as palavras como estão e, para cada radical, as palavras que o têm
    /// (`stem:<radical>`). A busca soma à palavra exata as outras do mesmo
    /// radical, com peso [`Config::inflection_weight`]: "planetas" acha o
    /// artigo "Planeta", mas "Austral" continua acima de "Austrália".
    ///
    /// Decidido na criação do banco, como o stemming (o mapa precisa existir
    /// desde o começo); não combina com `stemming`. `None` usa o gravado (ou
    /// [`Stemming::Off`], num banco novo).
    pub inflections: Option<Stemming>,
    /// Quanto as flexões valem perto da palavra exata (α, de 0 a 1), como um
    /// campo do BM25F: `tf~ = tf~_exata + α · tf~_flexões`, com o idf da
    /// união (α = 1 é o stemming na indexação). Vale por busca (não fica
    /// gravado).
    pub inflection_weight: f64,
    /// Quando somar as flexões (vale por busca).
    pub inflection_mode: InflectionMode,
    /// Filtros de Bloom e índices dos SSTs em partições de 4 KB (ligado por
    /// padrão). Desligar volta ao filtro inteiro, que num banco grande passa
    /// do tamanho de uma parte do block cache e é relido a cada `Get`; existe
    /// só para medir esse efeito (`wiki load --whole-filter`).
    pub partition_filters: bool,
    /// Parâmetro `k1` do BM25: quanto a nota cresce com a frequência do termo.
    pub bm25_k1: f64,
    /// Parâmetro `b` do BM25: quanto o tamanho do texto pesa (0 a 1).
    pub bm25_b: f64,
    /// Quanto uma ocorrência no título vale em relação a uma no texto, no
    /// BM25F (ver `docs/plano-relevancia.md`). 0 ignora o título.
    pub title_weight: f64,
    /// `b` do título: quanto um título longo perde para um curto (0 a 1).
    pub title_b: f64,
    /// Cria o banco se a pasta não tiver um (ligado por padrão). Desligado, o
    /// `open` de um caminho errado dá erro em vez de criar um banco vazio.
    /// [`Engine::open_read_only`](crate::Engine::open_read_only) nunca cria.
    pub create_if_missing: bool,
    /// Conta consultas, commits, edições, remoções e compactações (ver
    /// `docs/plano-monitoramento.md`). No caminho quente custa só somas
    /// atômicas num contador da própria thread e duas leituras de relógio
    /// por consulta. Desligado por padrão; sem a feature `metrics`, ligar dá
    /// erro no `open`.
    pub metrics: bool,
    /// Onde a thread amostradora grava o snapshot das métricas, que o
    /// `fire-flower monitor` lê. `None`: `fire-flower.metrics.json` na pasta
    /// do banco. Um banco aberto só para leitura só grava com um arquivo
    /// próprio aqui (o da pasta é de quem grava o banco).
    pub metrics_file: Option<PathBuf>,
    /// De quanto em quanto tempo a amostradora grava o snapshot; as
    /// propriedades do RocksDB são relidas a cada 5 snapshots.
    pub metrics_interval: Duration,
}

/// Quando a busca soma as flexões de uma palavra ([`Config::inflections`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InflectionMode {
    /// Em toda busca.
    Always,
    /// Só quando a busca exata acha menos que tantos documentos: as consultas
    /// que já acham bastante ficam como sem flexões.
    IfFewer(usize),
}

impl Config {
    /// Para quem só lê e convive com o processo que grava (a TUI, `search` e
    /// `suggest` da CLI): block cache de 32 MB e nada de criar banco.
    pub fn reader() -> Self {
        Self { block_cache_bytes: 32 << 20, create_if_missing: false, ..Self::default() }
    }

    pub(super) fn validate(&self) -> Result<()> {
        let in_unit = |v: f64| (0.0..=1.0).contains(&v);
        let ok = self.bm25_k1.is_finite()
            && self.bm25_k1 >= 0.0
            && in_unit(self.bm25_b)
            && in_unit(self.title_b)
            && self.title_weight.is_finite()
            && self.title_weight >= 0.0
            && in_unit(self.inflection_weight);
        if self.metrics_interval.is_zero() {
            return Err(Error::Config("Config::metrics_interval precisa ser maior que zero".into()));
        }
        if ok {
            Ok(())
        } else {
            Err(Error::Config(format!(
                "parâmetros de ranking inválidos: k1 = {}, b = {}, peso do título = {}, b do título = {}, \
                 peso das flexões = {} (k1 e peso ≥ 0; b e peso das flexões entre 0 e 1)",
                self.bm25_k1, self.bm25_b, self.title_weight, self.title_b, self.inflection_weight
            )))
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            block_cache_bytes: 256 << 20,
            bloom_bits_per_key: 10.0,
            max_prefix_terms: 256,
            statistics: false,
            writer_cache_nodes: 3_000_000,
            reader_cache_depth: Some(4),
            languages: None,
            positions: None,
            stemming: None,
            inflections: None,
            // Escolhidos pela grade da Fase 4 do plano de stemming (100k da
            // Wikipédia, com `pt-plural`): com α = 0,5 título e hub ficam como
            // sem flexões e o plural escrito à mão acha o artigo em 93% (top-3).
            // "Só se faltar" não ajuda: "planetas" já acha documentos demais.
            inflection_weight: 0.5,
            inflection_mode: InflectionMode::Always,
            partition_filters: true,
            // Os padrões do Lucene e do Elasticsearch.
            bm25_k1: 1.2,
            bm25_b: 0.75,
            // Escolhidos pela grade da Fase 4 do plano de relevância (100k da
            // Wikipédia, confirmada com outra amostra): a qualidade sobe até
            // ~50 e depois quase não muda; b 0,9 mantém o limite do corte finito.
            title_weight: 50.0,
            title_b: 0.9,
            create_if_missing: true,
            metrics: false,
            metrics_file: None,
            metrics_interval: Duration::from_secs(1),
        }
    }
}
