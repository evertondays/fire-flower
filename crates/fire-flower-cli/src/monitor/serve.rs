//! O `/metrics` por HTTP, no processo monitor: um servidor mínimo, uma
//! conexão por vez, sem dependência nova. O motor não ganha socket nem thread.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use super::Source;
use super::openmetrics::{Format, render};

/// Um cliente lento não segura o monitor mais que isto.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn serve(addr: &str, mut source: Source) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(addr)?;
    eprintln!("monitor: http://{}/metrics (snapshot em {})", listener.local_addr()?, source.file().display());
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        // Um cliente com erro não derruba o monitor.
        let _ = answer(stream, &mut source);
    }
    Ok(())
}

fn answer(mut stream: TcpStream, source: &mut Source) -> std::io::Result<()> {
    stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;
    let request = read_request(&mut BufReader::new(&stream))?;
    let (status, content_type, body) = respond(&request, source);
    let head = if request.method == "HEAD" { "" } else { body.as_str() };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{head}",
        body.len()
    )?;
    stream.flush()
}

pub(super) struct Request {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) accept: String,
}

/// A linha do pedido e o `Accept`; o resto dos cabeçalhos é ignorado.
pub(super) fn read_request(reader: &mut impl BufRead) -> std::io::Result<Request> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut accept = String::new();
    // Cabeçalhos até a linha vazia, no máximo 100 (um pedido de scrape tem uns 5).
    for _ in 0..100 {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("accept")
        {
            accept = value.trim().to_string();
        }
    }
    Ok(Request { method, path, accept })
}

pub(super) fn respond(request: &Request, source: &mut Source) -> (&'static str, &'static str, String) {
    let text = "text/plain; charset=utf-8";
    if request.method != "GET" && request.method != "HEAD" {
        return ("405 Method Not Allowed", text, "só GET\n".into());
    }
    match request.path.split('?').next().unwrap_or("") {
        "/metrics" => {
            let format = Format::from_accept(&request.accept);
            ("200 OK", format.content_type(), render(&source.read(), format))
        }
        "/" => ("200 OK", text, "fire-flower monitor: as métricas estão em /metrics\n".into()),
        _ => ("404 Not Found", text, "não encontrado; as métricas estão em /metrics\n".into()),
    }
}
