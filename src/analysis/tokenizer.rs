//! Tokenização: texto → tokens normalizados (sem acentos, minúsculos).

use unicode_normalization::char::is_combining_mark;
use unicode_normalization::char::decompose_compatible;

/// Tokens maiores que isso são descartados (hashes, base64, lixo binário),
/// para não criar caminhos enormes e inúteis na Trie.
pub const MAX_TOKEN_CHARS: usize = 64;

/// Chama `f(token, posição)` para cada termo normalizado do texto:
/// - remove acentos (decomposição NFKD + descarte das marcas combinantes);
/// - converte para minúsculas;
/// - trata qualquer caractere não alfanumérico (pontuação, espaço) como separador.
///
/// A posição conta todo token entregue, stopwords inclusive (quem descarta
/// stopwords é quem chama, e o buraco fica: "casa de papel" → casa@0, de@1,
/// papel@2). Tokens longos demais são descartados e não contam.
///
/// Não aloca por token: `token` é um buffer reaproveitado.
pub fn for_each_token(text: &str, mut f: impl FnMut(&str, u32)) {
    let mut token = String::new();
    let mut chars = 0usize;
    let mut position = 0u32;

    let mut flush = |token: &mut String, chars: &mut usize| {
        if *chars > 0 {
            if *chars <= MAX_TOKEN_CHARS {
                f(token, position);
                position += 1;
            }
            token.clear();
            *chars = 0;
        }
    };

    for c in text.chars() {
        if c.is_ascii() {
            // ASCII não se decompõe nem tem acento: o caminho comum, sem tabelas.
            if c.is_ascii_alphanumeric() {
                token.push(c.to_ascii_lowercase());
                chars += 1;
            } else {
                flush(&mut token, &mut chars);
            }
            continue;
        }
        // NFKD caractere a caractere. A forma normalizada da string inteira
        // só difere disso na ordem das marcas combinantes, que são descartadas
        // (o teste `per_char_nfkd_matches_string_nfkd` confere).
        decompose_compatible(c, |d| {
            if is_combining_mark(d) {
                return;
            }
            if d.is_alphanumeric() {
                for lower in d.to_lowercase() {
                    if !is_combining_mark(lower) {
                        token.push(lower);
                        chars += 1;
                    }
                }
            } else {
                flush(&mut token, &mut chars);
            }
        });
    }
    flush(&mut token, &mut chars);
}

/// Divide o texto em termos normalizados (ver [`for_each_token`]).
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for_each_token(text, |t, _| tokens.push(t.to_string()));
    tokens
}

/// Normaliza um único termo (ex.: prefixo do autocompletar).
/// Retorna `None` se o texto não contiver nenhum caractere indexável.
pub fn normalize_term(text: &str) -> Option<String> {
    let mut tokens = tokenize(text);
    match tokens.len() {
        0 => None,
        1 => tokens.pop(),
        // "e-mail" vira "email": para prefixos, juntamos as partes.
        _ => Some(tokens.concat()),
    }
}
