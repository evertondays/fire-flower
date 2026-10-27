//! Testes da busca interativa.

use fire_flower::Stemmer;

use super::draw::{describe_phrase, format_count, truncate_left};
use super::preview::highlight;
use super::*;

#[test]
fn highlight_marks_normalized_matches() {
    let terms: Vec<QueryTerm> =
        fire_flower::query::parse_query("coracao do bras*", &fire_flower::Stopwords::default()).iter().flat_map(Clause::terms).collect();
    let off = Stemmer::default();
    let (spans, hit) = highlight("O Coração do Brasil, não do Chile.", &terms, (&off, &off));
    assert!(hit);
    let marked: Vec<&str> =
        spans.iter().filter(|s| s.style.bg == Some(ACCENT)).map(|s| s.content.as_ref()).collect();
    assert_eq!(marked, ["Coração", "Brasil"]);
    let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(joined, "O Coração do Brasil, não do Chile.");
}

/// Desenha a tela num terminal virtual. Rode com `--nocapture` para ver.
#[test]
fn renders_screen() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let dir = tempfile::tempdir().unwrap();
    let doc = dir.path().join("brasil.txt");
    std::fs::write(&doc, "Introdução\nO coração do Brasil bate forte.\nBrasília é a capital.\n").unwrap();
    let engine = Engine::open(dir.path().join("db"), fire_flower::Config::default()).unwrap();
    let mut batch = engine.batch();
    batch.add_labeled(&doc.display().to_string(), &std::fs::read_to_string(&doc).unwrap()).unwrap();
    batch.add_labeled("outro.txt", "brasileiro bravo").unwrap();
    batch.commit().unwrap();

    let mut app = App::new(&engine, "/tmp/db");
    for c in "coração bra".chars() {
        app.on_key(KeyEvent::from(KeyCode::Char(c)));
    }
    app.refresh();
    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
    terminal.draw(|f| app.draw(f)).unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .chunks(90)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>() + "\n")
        .collect();
    println!("{screen}");

    assert_eq!(app.effective, "coração bra*");
    assert_eq!(app.results.len(), 1, "só o primeiro tem coração");
    assert_eq!(app.suggestions, ["bravo", "brasil", "brasilia", "brasileiro"]);
    assert!(screen.contains("✿ fire-flower"));
    assert!(screen.contains("coração bra"));

    // Tab completa a palavra atual com a sugestão escolhida.
    app.on_key(KeyEvent::from(KeyCode::Char('s')));
    app.refresh();
    assert_eq!(app.ghost().as_deref(), Some("il"));
    assert!(app.on_key(KeyEvent::from(KeyCode::Tab)));
    assert_eq!(app.query, "coração brasil ");

    // "brasil" existe e é a sugestão mais curta, mas o Tab deve ir além dela.
    app.query = "brasil".into();
    app.refresh();
    assert_eq!(app.suggestions[0], "brasil");
    assert_eq!(app.ghost().as_deref(), Some("ia"));
}

#[test]
fn phrases_are_one_unit_and_not_prefixes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path().join("db"), fire_flower::Config::default()).unwrap();
    let mut batch = engine.batch();
    batch.add_labeled("a.txt", "a casa de papel pegou fogo").unwrap();
    batch.add_labeled("b.txt", "papel de casa, casa de papel").unwrap();
    batch.add_labeled("c.txt", "papel casa").unwrap();
    batch.commit().unwrap();

    let mut app = App::new(&engine, "/tmp/db");
    // Aspas abertas: a palavra sendo digitada não vira prefixo.
    app.query = "\"casa de pap".into();
    app.refresh();
    assert_eq!(app.effective, "\"casa de pap");
    assert_eq!(app.current_word.as_deref(), Some("pap"));
    assert_eq!(app.suggestions, ["papel"]);
    assert_eq!(app.total, 0);
    app.query = "\"casa de papel\"".into();
    app.refresh();
    assert_eq!(app.effective, "\"casa de papel\"", "aspas fechadas: nada de prefixo");
    assert_eq!(app.total, 2);
    assert_eq!(describe_phrase(&app.clauses[0]), "\"casa _ papel\"");
    // O destaque marca as palavras da frase.
    let (_, hit) = highlight("Casa de Papel", &app.terms, (app.engine.stemmer(), app.engine.inflector()));
    assert!(hit);
    // Fora das aspas, a última palavra continua sendo prefixo.
    app.query = "\"casa de papel\" fo".into();
    app.refresh();
    assert_eq!(app.effective, "\"casa de papel\" fo*");
    assert_eq!(app.total, 1);
    assert!(app.results[0].score > 0.0);
}

#[test]
fn helpers() {
    assert_eq!(format_count(0), "0");
    assert_eq!(format_count(1234567), "1.234.567");
    assert_eq!(truncate_left("abcdef", 4), "…def");
    assert_eq!(truncate_left("abc", 4), "abc");
}
