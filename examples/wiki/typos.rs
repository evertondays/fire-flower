//! Erros de digitação gerados (conjuntos `rel-typo` e `t-typo`, oráculo) e a
//! distância de Damerau-Levenshtein restrita, escrita de novo aqui (não usa a
//! do motor).

use crate::rng::Rng;

/// Linhas do teclado (sem acentos: a normalização os tira, e o `ç` vira `c`).
const ROWS: [&[u8]; 3] = [b"qwertyuiop", b"asdfghjkl", b"zxcvbnm"];

/// Uma letra vizinha de `c` no teclado: ao lado na mesma linha ou na mesma
/// coluna das linhas de cima e de baixo.
fn neighbor(c: u8, rng: &mut Rng) -> u8 {
    for (r, row) in ROWS.iter().enumerate() {
        let Some(i) = row.iter().position(|&x| x == c) else { continue };
        let mut options = Vec::new();
        if i > 0 {
            options.push(row[i - 1]);
        }
        if let Some(&next) = row.get(i + 1) {
            options.push(next);
        }
        for other in [r.checked_sub(1), Some(r + 1)].into_iter().flatten().filter_map(|r| ROWS.get(r)) {
            if let Some(&below) = other.get(i) {
                options.push(below);
            }
        }
        return *rng.pick(&options);
    }
    c
}

/// Um erro de digitação em `word` (só letras de a a z, 3 ou mais), fora da
/// primeira letra: vizinha no teclado, letra faltando, letra repetida ou duas
/// vizinhas invertidas. `None` se a palavra não serve ou o erro não mudou nada.
pub(crate) fn typo(word: &str, rng: &mut Rng) -> Option<String> {
    let mut w = word.as_bytes().to_vec();
    if w.len() < 3 || !w.iter().all(u8::is_ascii_lowercase) {
        return None;
    }
    let i = 1 + rng.below(w.len() - 1);
    match rng.below(4) {
        0 => w[i] = neighbor(w[i], rng),
        1 => {
            w.remove(i);
        }
        2 => w.insert(i, w[i]),
        _ if i + 1 < w.len() => w.swap(i, i + 1),
        _ => w.swap(i - 1, i),
    }
    let out = String::from_utf8(w).expect("só ASCII");
    (out != word && out.as_bytes()[0] == word.as_bytes()[0]).then_some(out)
}

/// Distância de Damerau-Levenshtein restrita (uma transposição de vizinhas
/// vale 1), pela matriz inteira.
pub(crate) fn osa(a: &[char], b: &[char]) -> usize {
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

/// Se `a` e `b` estão a exatamente um erro (com o atalho do tamanho).
pub(crate) fn one_edit(a: &[char], b: &[char]) -> bool {
    a.len().abs_diff(b.len()) <= 1 && osa(a, b) == 1
}
