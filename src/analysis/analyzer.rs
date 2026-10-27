//! Análise de um documento: os termos indexados (sem stopwords, com stemming)
//! e as posições de cada um.

use rustc_hash::FxHashMap;

use crate::analysis::{for_each_token, tokenize};
use crate::analysis::stem::{StemCache, Stemmer};
use crate::analysis::stopwords::Stopwords;

/// Chama `f(termo, posição)` para cada token de `text` que não é stopword,
/// com o termo já reduzido ao radical se o `stemmer` estiver ligado (as
/// stopwords são conferidas antes, no token inteiro). As posições são as de
/// [`for_each_token`]: stopwords deixam buraco.
pub fn for_each_term(text: &str, stopwords: &Stopwords, stemmer: &Stemmer, mut f: impl FnMut(&str, u32)) {
    let mut stem = String::new();
    for_each_token(text, |t, p| {
        if stopwords.contains(t) {
            return;
        }
        if stemmer.is_on() {
            stemmer.stem_into(t, &mut stem);
            f(&stem, p);
        } else {
            f(t, p);
        }
    });
}

/// Termos que a indexação grava para `text`: tokenizados, sem stopwords,
/// ordenados e sem repetição.
pub fn index_terms(text: &str, stopwords: &Stopwords) -> Vec<String> {
    let mut terms = tokenize(text);
    terms.retain(|t| !stopwords.contains(t));
    terms.sort_unstable();
    terms.dedup();
    terms
}

/// Um documento analisado: cada termo indexado com suas posições, e o
/// tamanho do documento (tokens indexados, sem stopwords).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    /// Termos em ordem alfabética; posições em ordem crescente.
    pub terms: Vec<(String, Vec<u32>)>,
    pub len: u32,
}

/// Analisa `text` como a indexação faz. Para muitos documentos, o
/// [`Analyzer`] reaproveita a memória entre um e outro.
pub fn analyze(text: &str, stopwords: &Stopwords) -> Analysis {
    let mut analyzer = Analyzer::default();
    let len = analyzer.run(text, stopwords);
    let mut terms: Vec<(String, Vec<u32>)> = analyzer.terms().map(|(t, p)| (t.to_string(), p.to_vec())).collect();
    terms.sort_unstable();
    Analysis { terms, len }
}

/// Agrupa os tokens de um documento por termo, guardando as posições.
///
/// Feito para o laço quente da indexação: os buffers são reaproveitados
/// entre documentos e nenhum token vira `String` (o texto dos termos fica
/// num buffer só).
#[derive(Default)]
pub struct Analyzer {
    /// Texto dos tokens indexados, um atrás do outro.
    text: String,
    /// (início, fim, posição) de cada token indexado.
    tokens: Vec<(u32, u32, u32)>,
    /// Grupo (termo) de cada token.
    group_of: Vec<u32>,
    /// (primeiro token, quantas ocorrências) de cada termo distinto.
    groups: Vec<(u32, u32)>,
    /// Onde começam as posições de cada grupo em `positions`.
    starts: Vec<u32>,
    /// Posições agrupadas por termo, cada grupo em ordem crescente.
    positions: Vec<u32>,
    /// Posições do título: 0…`title_span` − 1 (stopwords contam).
    title_span: u32,
    /// Radicais já calculados (só com stemming).
    stems: StemCache,
}

/// Vão entre o título e o texto nas posições: uma frase não casa
/// atravessando o fim do título e o começo do texto (o Elasticsearch usa o
/// mesmo valor no `position_increment_gap`).
pub const TITLE_GAP: u32 = 100;

impl Analyzer {
    /// Analisa `text` e devolve o tamanho do documento (tokens sem stopwords).
    /// Os termos ficam disponíveis em [`Analyzer::terms`] até a próxima chamada.
    pub fn run(&mut self, text: &str, stopwords: &Stopwords) -> u32 {
        self.run_fields("", text, stopwords, &Stemmer::default()).0
    }

    /// Analisa um documento com título. O título ocupa as posições
    /// 0…T − 1 e o texto começa em T + [`TITLE_GAP`] (ou em 0, sem título).
    /// Devolve (tamanho do documento inteiro, tamanho do título), sem stopwords.
    ///
    /// Com o `stemmer` ligado, os termos são os radicais (as stopwords são
    /// conferidas antes, no token inteiro).
    pub fn run_fields(&mut self, title: &str, text: &str, stopwords: &Stopwords, stemmer: &Stemmer) -> (u32, u32) {
        let Self { text: buf, tokens, group_of, groups, starts, positions, title_span, stems } = self;
        buf.clear();
        tokens.clear();
        group_of.clear();
        groups.clear();
        let (mut span, mut title_len) = (0u32, 0u32);
        // Devolve se o token entrou (não é stopword).
        let mut push = |t: &str, p: u32| {
            let kept = !stopwords.contains(t);
            if kept {
                let start = buf.len() as u32;
                buf.push_str(stems.get(stemmer, t));
                tokens.push((start, buf.len() as u32, p));
            }
            kept
        };
        for_each_token(title, |t, p| {
            span = p + 1;
            title_len += u32::from(push(t, p));
        });
        let offset = if span > 0 { span + TITLE_GAP } else { 0 };
        for_each_token(text, |t, p| {
            push(t, offset + p);
        });
        *title_span = span;

        // Termo -> grupo. O mapa empresta `buf`, então nasce a cada documento
        // (uma alocação só, já no tamanho certo).
        let mut ids: FxHashMap<&str, u32> = FxHashMap::with_capacity_and_hasher(tokens.len(), Default::default());
        for (i, &(s, e, _)) in tokens.iter().enumerate() {
            let next = groups.len() as u32;
            let g = *ids.entry(&buf[s as usize..e as usize]).or_insert(next);
            if g == next {
                groups.push((i as u32, 0));
            }
            groups[g as usize].1 += 1;
            group_of.push(g);
        }

        // Ordenação por contagem: as posições de cada termo ficam juntas e,
        // como os tokens vêm em ordem, crescentes dentro do grupo.
        starts.clear();
        let mut acc = 0u32;
        for &(_, count) in groups.iter() {
            starts.push(acc);
            acc += count;
        }
        positions.clear();
        positions.resize(tokens.len(), 0);
        let mut cursor = starts.clone();
        for (&(_, _, p), &g) in tokens.iter().zip(group_of.iter()) {
            positions[cursor[g as usize] as usize] = p;
            cursor[g as usize] += 1;
        }
        (tokens.len() as u32, title_len)
    }

    /// Posições ocupadas pelo título do último documento (stopwords inclusive).
    pub fn title_span(&self) -> u32 {
        self.title_span
    }

    /// Quantas das `positions` (de um termo do último documento) estão no título.
    pub fn title_tf(&self, positions: &[u32]) -> u32 {
        positions.partition_point(|&p| p < self.title_span) as u32
    }

    /// Termos distintos do último documento, na ordem da primeira ocorrência.
    pub fn terms(&self) -> impl Iterator<Item = (&str, &[u32])> + '_ {
        self.groups.iter().zip(self.starts.iter()).map(|(&(first, count), &start)| {
            let (s, e, _) = self.tokens[first as usize];
            (&self.text[s as usize..e as usize], &self.positions[start as usize..(start + count) as usize])
        })
    }
}
