//! Limpeza de HTML antes da indexação.

/// Remove tags HTML e decodifica as entidades mais comuns.
/// Conteúdo de `<script>` e `<style>` é descartado por inteiro.
pub fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find('>') else {
            // '<' sem fechamento: trata como texto.
            out.push_str(rest);
            return decode_entities(&out);
        };
        let tag = rest[1..end].trim_start().to_ascii_lowercase();
        rest = &rest[end + 1..];

        let name = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        if (name == "script" || name == "style") && !tag.ends_with('/') {
            let closing = format!("</{name}");
            rest = match rest.to_ascii_lowercase().find(&closing) {
                Some(pos) => rest[pos..].find('>').map_or("", |e| &rest[pos + e + 1..]),
                None => "",
            };
        }
        // Tags viram separadores, senão "<p>a</p><p>b</p>" viraria "ab".
        out.push(' ');
    }
    out.push_str(rest);
    decode_entities(&out)
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}
