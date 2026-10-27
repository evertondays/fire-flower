//! BM25F e os limites superiores da nota usados no top-k.

use crate::codec::DocLens;
use crate::IndexStats;
use crate::Config;

/// Normalização de um campo pelo tamanho: `B(len) = 1 − b + b · len / média`.
#[derive(Clone, Copy)]
pub(crate) struct FieldNorm {
    pub(crate) base: f64,
    pub(crate) slope: f64,
}

impl FieldNorm {
    pub(crate) fn new(b: f64, avg: f64) -> Self {
        Self { base: 1.0 - b, slope: if avg > 0.0 { b / avg } else { 0.0 } }
    }

    #[inline]
    pub(crate) fn of(&self, len: f64) -> f64 {
        self.base + self.slope * len
    }
}

/// Os parâmetros do BM25F para uma busca (seção 1 de `plano-relevancia.md`):
///
/// ```text
/// B_f(d) = 1 − b_f + b_f · len_f(d) / média_f      (f = texto, título)
/// tf~    = tf_texto / B_texto  +  w · tf_título / B_título
/// nota   = idf · tf~ · (k1 + 1) / (tf~ + k1)
/// ```
///
/// O título multiplica o tf antes da saturação: uma ocorrência nele vale
/// `w` no texto, e um título curto vale mais que um longo. Sem título
/// (`tf_título = 0`) é exatamente o BM25 de antes.
pub(crate) struct Bm25 {
    pub(crate) k1: f64,
    pub(crate) n: f64,
    pub(crate) body: FieldNorm,
    pub(crate) title: FieldNorm,
    pub(crate) title_weight: f64,
    /// Maior `1 / B_texto` (documento de tamanho 0): para o limite superior.
    pub(crate) max_body: f64,
    /// Quanto uma ocorrência no título vale, no máximo, em ocorrências do
    /// texto num documento de tamanho 0: `(w / (1 − b_título)) / max_body`.
    pub(crate) title_ratio: f64,
}

/// Os pesos de um documento: `1 / B_texto` e `w / B_título`.
#[derive(Clone, Copy)]
pub(crate) struct DocNorm {
    pub(crate) body: f64,
    pub(crate) title: f64,
}

impl Bm25 {
    pub(crate) fn new(config: &Config, stats: IndexStats) -> Self {
        let body = FieldNorm::new(config.bm25_b, stats.avg_body_len());
        let title = FieldNorm::new(config.title_b, stats.avg_title_len());
        let inv = |x: f64| if x > 0.0 { 1.0 / x } else { f64::INFINITY };
        let max_body = inv(body.base);
        let max_title = if config.title_weight > 0.0 { config.title_weight * inv(title.base) } else { 0.0 };
        let title_ratio = if max_body.is_finite() { max_title / max_body } else { 0.0 };
        Self { k1: config.bm25_k1, n: stats.doc_count as f64, body, title, title_weight: config.title_weight, max_body, title_ratio }
    }

    pub(crate) fn idf(&self, df: usize) -> f64 {
        let df = df as f64;
        (1.0 + (self.n - df + 0.5) / (df + 0.5)).ln()
    }

    /// idf · (k1 + 1): constante por termo.
    pub(crate) fn weight(&self, idf: f64) -> f64 {
        idf * (self.k1 + 1.0)
    }

    /// Os pesos de um documento, uma vez por documento.
    #[inline]
    pub(crate) fn doc_norm(&self, lens: DocLens) -> DocNorm {
        let title_len = f64::from(lens.title_len);
        let body_len = f64::from(lens.len) - title_len;
        // B = 0 só com b = 1 e o campo vazio: aí o tf do campo é 0 também.
        let b_body = self.body.of(body_len);
        let b_title = self.title.of(title_len);
        DocNorm {
            body: if b_body > 0.0 { 1.0 / b_body } else { 0.0 },
            title: if b_title > 0.0 { self.title_weight / b_title } else { 0.0 },
        }
    }

    /// tf~: as ocorrências no texto e no título, cada uma normalizada pelo seu campo.
    #[inline]
    pub(crate) fn pseudo_tf(norm: DocNorm, tf: u32, title_tf: u32) -> f64 {
        f64::from(tf - title_tf) * norm.body + f64::from(title_tf) * norm.title
    }

    /// idf · tf~ · (k1 + 1) / (tf~ + k1).
    #[inline]
    pub(crate) fn score(&self, weight: f64, pseudo_tf: f64) -> f64 {
        weight * pseudo_tf / (pseudo_tf + self.k1)
    }
}

/// Maior nota possível de uma parte, pela chave (ocorrências equivalentes
/// do texto num documento de tamanho 0, arredondadas para cima): sem título
/// a chave é o próprio tf. Tabelada para as chaves comuns: conferir o limite
/// fica sem divisão. A margem relativa cobre o arredondamento de ponto
/// flutuante, então o corte continua exato.
pub(crate) struct Bound {
    pub(crate) weight: f64,
    pub(crate) k1: f64,
    pub(crate) max_body: f64,
    pub(crate) title_ratio: f64,
    pub(crate) table: [f64; BOUND_TF + 1],
}

pub(crate) const BOUND_TF: usize = 32;

pub(crate) const BOUND_MARGIN: f64 = 1.0 + 1e-12;

impl Bound {
    pub(crate) fn new(weight: f64, bm25: &Bm25) -> Self {
        let mut b = Self { weight, k1: bm25.k1, max_body: bm25.max_body, title_ratio: bm25.title_ratio, table: [0.0; BOUND_TF + 1] };
        b.table = std::array::from_fn(|key| b.compute(key as u32));
        b
    }

    pub(crate) fn compute(&self, key: u32) -> f64 {
        if !self.max_body.is_finite() {
            return self.weight * BOUND_MARGIN; // b = 1: sem limite útil (a nota nunca passa do peso)
        }
        let p = self.max_body * f64::from(key);
        self.weight * p / (p + self.k1) * BOUND_MARGIN
    }

    /// A chave de uma entrada: o tf, se não houver título; senão o tf do
    /// texto mais o do título convertido, arredondado para cima.
    #[inline]
    pub(crate) fn key(&self, tf: u32, title_tf: u32) -> u32 {
        if title_tf == 0 {
            return tf;
        }
        let x = f64::from(tf - title_tf) + self.title_ratio * f64::from(title_tf);
        if x.is_finite() && x < f64::from(u32::MAX - 1) { (x + 1e-9).ceil() as u32 } else { u32::MAX }
    }

    #[inline]
    pub(crate) fn get_key(&self, key: u32) -> f64 {
        match self.table.get(key as usize) {
            Some(&b) => b,
            None => self.compute(key),
        }
    }

    #[inline]
    pub(crate) fn get(&self, tf: u32, title_tf: u32) -> f64 {
        self.get_key(self.key(tf, title_tf))
    }

    /// Limite de um grupo de flexões: as chaves da exata e das flexões, as
    /// das flexões com peso `alpha` (cada chave já é um limite do seu tf~).
    #[inline]
    pub(crate) fn get_mixed(&self, exact: (u32, u32), variants: (u32, u32), alpha: f64) -> f64 {
        if !self.max_body.is_finite() {
            return self.weight * BOUND_MARGIN;
        }
        let key = f64::from(self.key(exact.0, exact.1)) + alpha * f64::from(self.key(variants.0, variants.1));
        let p = self.max_body * key;
        self.weight * p / (p + self.k1) * BOUND_MARGIN
    }
}
