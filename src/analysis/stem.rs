//! Stemming: reduz cada termo ao radical (Snowball), para que "correndo",
//! "correu" e "corrida" virem o mesmo termo (`corr`).
//!
//! O radical sai do termo já normalizado (sem acentos), para a busca continuar
//! insensível a acentos: "sao paulo" tem que achar "São Paulo". Só que o
//! Snowball do português foi escrito para texto acentuado e, sem os acentos,
//! erra os sufixos mais comuns ("informacao" vira `informaca`, e
//! "informacoes", `informaco`). Por isso, antes do Snowball, os sufixos que
//! ele precisa ver acentuados ganham o acento de volta ([`reaccent`]):
//! "informacao" → "informação" → `inform`. Medido em 10 mil artigos da
//! Wikipédia, isso dá até menos radicais distintos que o Snowball no texto
//! original, e o radical depende só do termo normalizado.
//!
//! Como as stopwords, o stemming é decidido na criação do banco (ver
//! [`Config::stemming`](crate::Config::stemming)): índice e consulta precisam
//! reduzir os termos do mesmo jeito.

use rustc_hash::FxHashMap;

use crate::analysis::for_each_token;
use crate::analysis::stopwords::Language;

/// Stemming de um banco.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Stemming {
    /// Termos indexados como estão (só normalizados).
    #[default]
    Off,
    /// Snowball do idioma.
    Snowball(Language),
    /// Só o plural vira singular, por regras ("eleicoes" → "eleicao",
    /// "casas" → "casa"): agrupa bem menos que o Snowball.
    Plural(Language),
}

impl Stemming {
    /// Como é gravado no banco: "nenhum", "pt", "en", "pt-plural", "en-plural".
    pub fn code(self) -> &'static str {
        match self {
            Stemming::Off => "nenhum",
            Stemming::Snowball(lang) => lang.code(),
            Stemming::Plural(Language::Portuguese) => "pt-plural",
            Stemming::Plural(Language::English) => "en-plural",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        match code.trim() {
            "nenhum" | "" => Some(Stemming::Off),
            other => match other.strip_suffix("-plural") {
                Some(lang) => Language::from_code(lang).map(Stemming::Plural),
                None => Language::from_code(other).map(Stemming::Snowball),
            },
        }
    }
}

impl std::fmt::Display for Stemming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// Reduz termos ao radical, conforme o [`Stemming`] do banco.
pub struct Stemmer {
    stemming: Stemming,
    snowball: Option<rust_stemmers::Stemmer>,
}

impl Default for Stemmer {
    fn default() -> Self {
        Self::new(Stemming::Off)
    }
}

impl std::fmt::Debug for Stemmer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Stemmer").field(&self.stemming).finish()
    }
}

impl Clone for Stemmer {
    fn clone(&self) -> Self {
        Self::new(self.stemming)
    }
}

impl Stemmer {
    pub fn new(stemming: Stemming) -> Self {
        let snowball = match stemming {
            Stemming::Snowball(lang) => Some(rust_stemmers::Stemmer::create(match lang {
                Language::Portuguese => rust_stemmers::Algorithm::Portuguese,
                Language::English => rust_stemmers::Algorithm::English,
            })),
            _ => None,
        };
        Self { stemming, snowball }
    }

    pub fn stemming(&self) -> Stemming {
        self.stemming
    }

    pub fn is_on(&self) -> bool {
        self.stemming != Stemming::Off
    }

    /// Escreve em `out` o radical de `term`, um token já normalizado (ver
    /// [`for_each_token`]). Sem stemming, o próprio `term`.
    pub fn stem_into(&self, term: &str, out: &mut String) {
        out.clear();
        if let Stemming::Plural(lang) = self.stemming {
            out.push_str(&singular(term, lang));
            return;
        }
        let Some(snowball) = &self.snowball else {
            out.push_str(term);
            return;
        };
        // Português: o plural irregular vira singular (o Snowball deixa
        // "eleicoes" e "eleicao" separados) e o sufixo ganha o acento de volta.
        let portuguese = self.stemming == Stemming::Snowball(Language::Portuguese);
        let singular_form = if portuguese { plural_suffix(term) } else { None };
        let term_in = singular_form.as_deref().unwrap_or(term);
        let accented = if portuguese { reaccent(term_in) } else { None };
        let stem = snowball.stem(accented.as_deref().unwrap_or(term_in));
        if stem.is_empty() {
            out.push_str(term);
        } else if stem.is_ascii() {
            out.push_str(&stem);
        } else {
            // Sobrou um acento do sufixo ("coraçã"): normaliza como o token.
            for_each_token(&stem, |piece, _| out.push_str(piece));
        }
    }

    /// O radical de `term`, alocando (para consultas; a indexação usa o
    /// [`StemCache`]).
    pub fn stem(&self, term: &str) -> String {
        let mut out = String::new();
        self.stem_into(term, &mut out);
        out
    }
}

/// Sufixos do português que o Snowball só reconhece acentuados, na forma sem
/// acento e com acento. Os mais longos vêm antes dos que terminam igual.
const SUFFIXES: &[(&str, &str)] = &[
    ("coes", "ções"),
    ("cao", "ção"),
    ("oes", "ões"),
    ("aes", "ães"),
    ("ao", "ão"),
    ("aveis", "áveis"),
    ("iveis", "íveis"),
    ("avel", "ável"),
    ("ivel", "ível"),
    ("ancias", "âncias"),
    ("ancia", "ância"),
    ("encias", "ências"),
    ("encia", "ência"),
    ("ugues", "uguês"),
    ("ances", "ancês"),
];

/// O termo com o acento do sufixo de volta, se ele termina num de
/// [`SUFFIXES`] e sobra ao menos duas letras antes ("sao" fica como está).
/// Errar o acento de uma palavra que não o tinha só muda qual radical ela
/// ganha, igual no índice e na consulta.
fn reaccent(term: &str) -> Option<String> {
    let (from, to) = SUFFIXES.iter().find(|(from, _)| term.len() >= from.len() + 2 && term.ends_with(from))?;
    Some(format!("{}{to}", &term[..term.len() - from.len()]))
}

/// Os plurais irregulares mais comuns do português, que o Snowball não
/// junta ao singular (`-ções`/`-ção`, `-ões`/`-ão`, `-ães`/`-ão`), já sem
/// acentos. `None` se o termo não termina num deles.
fn plural_suffix(term: &str) -> Option<String> {
    let rules: &[(&str, &str)] = &[("coes", "cao"), ("oes", "ao"), ("aes", "ao")];
    rules
        .iter()
        .find(|(from, _)| term.len() >= from.len() + 2 && term.ends_with(from))
        .map(|(from, to)| format!("{}{to}", &term[..term.len() - from.len()]))
}

/// Singular de um termo sem acentos, por regras conservadoras (`pt-plural`):
/// os plurais irregulares de [`plural_suffix`], `-ais` → `-al`, `-eis` →
/// `-el`, `-ns` → `-m` e o `s` final, menos depois de `s`, `u` e `i`
/// ("virus", "lapis", "pais" ficam). Erra alguns ("chineses" ≠ "chines"),
/// mas sempre do mesmo jeito no índice e na consulta.
fn singular(term: &str, lang: Language) -> String {
    match plural_cut(term, lang) {
        Singular::Prefix(n) => term[..n].to_string(),
        Singular::Owned(s) => s,
    }
}

/// O singular de [`singular`]: quase sempre um começo da própria palavra
/// ("casas" → "casa", "livre" → "livr"), sem alocar; só as regras que trocam
/// o sufixo ("eleicoes" → "eleicao") montam um texto novo.
enum Singular {
    Prefix(usize),
    Owned(String),
}

fn plural_cut(term: &str, lang: Language) -> Singular {
    let n = term.len();
    if n <= 3 || !term.is_ascii() {
        return Singular::Prefix(n);
    }
    let mut len = if !term.ends_with('s') {
        n
    } else if let Some(s) = (lang == Language::Portuguese).then(|| plural_rule(term)).flatten() {
        return Singular::Owned(s);
    } else {
        match term.as_bytes()[n - 2] {
            b's' | b'u' | b'i' => n,
            _ => n - 1,
        }
    };
    // "mercadores" → "mercadore" e "mercador" têm que dar o mesmo termo: o
    // "e" final depois de r ou z sai de toda palavra ("livre" e "livres"
    // viram "livr", "vez" e "vezes", "vez").
    let cut = &term[..len];
    if lang == Language::Portuguese && len > 3 && (cut.ends_with("re") || cut.ends_with("ze")) {
        len -= 1;
    }
    Singular::Prefix(len)
}

/// As regras de plural do português que trocam o fim da palavra.
fn plural_rule(term: &str) -> Option<String> {
    if let Some(s) = plural_suffix(term) {
        return Some(s);
    }
    let rules: &[(&str, &str, usize)] = &[("ais", "al", 3), ("eis", "el", 3), ("ns", "m", 2)];
    rules.iter().find_map(|(from, to, min)| {
        let stem = term.strip_suffix(from)?;
        (stem.len() >= *min).then(|| format!("{stem}{to}"))
    })
}

/// Radicais já calculados, por termo (só para o Snowball). O Snowball custa ~200 ns por termo,
/// mais que o resto da tokenização; como poucos termos formam quase todo o
/// texto, o cache acerta quase sempre (~40 ns por termo na Wikipédia).
#[derive(Default)]
pub struct StemCache {
    map: FxHashMap<Box<str>, Box<str>>,
    scratch: String,
}

impl StemCache {
    /// Termos guardados no máximo; passando disso, o cache recomeça vazio
    /// (~60 MB no limite).
    const LIMIT: usize = 1 << 20;

    /// O radical de `term` (ver [`Stemmer::stem_into`]).
    pub fn get<'a>(&'a mut self, stemmer: &Stemmer, term: &'a str) -> &'a str {
        if !stemmer.is_on() {
            return term;
        }
        // O de plural é só comparar sufixos: mais barato calcular que procurar no mapa.
        if let Stemming::Plural(lang) = stemmer.stemming() {
            return match plural_cut(term, lang) {
                Singular::Prefix(n) => &term[..n],
                Singular::Owned(s) => {
                    self.scratch = s;
                    &self.scratch
                }
            };
        }
        if !self.map.contains_key(term) {
            if self.map.len() >= Self::LIMIT {
                self.map.clear();
            }
            stemmer.stem_into(term, &mut self.scratch);
            self.map.insert(term.into(), self.scratch.as_str().into());
        }
        &self.map[term]
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::tokenize;

    fn stems(text: &str) -> Vec<String> {
        let stemmer = Stemmer::new(Stemming::Snowball(Language::Portuguese));
        tokenize(text).iter().map(|t| stemmer.stem(t)).collect()
    }

    #[test]
    fn conflates_inflections() {
        assert_eq!(stems("correndo correu corrida correr"), ["corr"; 4]);
        assert_eq!(stems("brasileiro brasileira brasileiros"), ["brasileir"; 3]);
        assert_eq!(stems("Países país paises"), ["pais"; 3]);
        assert_eq!(stems("2014 rust"), ["2014", "rust"]);
    }

    /// Com ou sem acento, o mesmo radical: o stemming vê o termo normalizado.
    #[test]
    fn accents_do_not_matter() {
        for (with, without) in [
            ("Informação informações", "informacao informacoes"),
            ("português portuguesa", "portugues portuguesa"),
            ("São Paulo", "sao paulo"),
            ("população educação", "populacao educacao"),
            ("importância independência", "importancia independencia"),
            ("mãe mães irmão irmãos", "mae maes irmao irmaos"),
        ] {
            assert_eq!(stems(with), stems(without), "{with}");
        }
        assert_eq!(stems("informação informações"), ["inform", "inform"]);
        assert_eq!(stems("português portuguesa"), ["portugues", "portugues"]);
        assert_eq!(stems("francês francesa"), ["frances", "frances"]);
    }

    /// O Snowball sozinho deixa singular e plural de -ção/-ão separados.
    #[test]
    fn irregular_plurals_join_the_singular() {
        assert_eq!(stems("eleição eleições"), ["eleica", "eleica"]);
        assert_eq!(stems("coração corações região regiões"), ["coraca", "coraca", "regia", "regia"]);
    }

    fn plural(text: &str) -> Vec<String> {
        let stemmer = Stemmer::new(Stemming::Plural(Language::Portuguese));
        tokenize(text).iter().map(|t| stemmer.stem(t)).collect()
    }

    #[test]
    fn plural_stemmer_only_removes_the_plural() {
        assert_eq!(plural("casas casa eleições eleição"), ["casa", "casa", "eleicao", "eleicao"]);
        assert_eq!(plural("jornais jornal papéis papel homens homem"), ["jornal", "jornal", "papel", "papel", "homem", "homem"]);
        assert_eq!(plural("mercadores mercador vezes vez livres livre"), ["mercador", "mercador", "vez", "vez", "livr", "livr"]);
        // Verbos e derivados ficam: só o plural sai.
        assert_eq!(plural("correndo corrida"), ["correndo", "corrida"]);
        // Singulares terminados em s (depois de s, u, i) ficam.
        assert_eq!(plural("vírus lápis país"), ["virus", "lapis", "pais"]);
        assert_eq!(plural("cão"), ["cao"], "três letras ou menos: como está");
        let stemmer = Stemmer::new(Stemming::Plural(Language::Portuguese));
        assert!(stemmer.is_on());
        assert_eq!(Stemming::from_code("pt-plural"), Some(Stemming::Plural(Language::Portuguese)));
    }

    #[test]
    fn reaccents_only_known_suffixes() {
        assert_eq!(reaccent("informacoes").as_deref(), Some("informações"));
        assert_eq!(reaccent("notavel").as_deref(), Some("notável"));
        assert_eq!(reaccent("sao"), None, "sobram menos de duas letras");
        assert_eq!(reaccent("casa"), None);
    }

    #[test]
    fn off_keeps_the_term() {
        let stemmer = Stemmer::default();
        assert!(!stemmer.is_on());
        assert_eq!(stemmer.stem("acao"), "acao");
        let mut cache = StemCache::default();
        assert_eq!(cache.get(&stemmer, "acao"), "acao");
        assert!(cache.is_empty(), "sem stemming o cache não guarda nada");
    }

    #[test]
    fn cache_matches_the_stemmer() {
        let stemmer = Stemmer::new(Stemming::Snowball(Language::Portuguese));
        let mut cache = StemCache::default();
        for _ in 0..2 {
            for t in tokenize("Ações e informações dos países, correndo") {
                assert_eq!(cache.get(&stemmer, &t), stemmer.stem(&t));
            }
        }
        assert_eq!(cache.len(), 6);
    }

    #[test]
    fn english_has_no_reaccent() {
        let stemmer = Stemmer::new(Stemming::Snowball(Language::English));
        assert_eq!(stemmer.stem("running"), "run");
        assert_eq!(stemmer.stem("cao"), "cao");
    }

    #[test]
    fn codes_round_trip() {
        for s in [
            Stemming::Off,
            Stemming::Snowball(Language::Portuguese),
            Stemming::Snowball(Language::English),
            Stemming::Plural(Language::Portuguese),
            Stemming::Plural(Language::English),
        ] {
            assert_eq!(Stemming::from_code(s.code()), Some(s));
        }
        assert_eq!(Stemming::from_code("xx"), None);
    }
}
