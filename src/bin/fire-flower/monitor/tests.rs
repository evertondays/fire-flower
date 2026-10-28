//! Testes do monitor: `/proc`, disco, formato do `/metrics` e HTTP.

use fire_flower::metrics::{OpCounters, QUERY_BOUNDS, Snapshot};

use super::*;
use super::openmetrics::{Format, go_float, render};
use super::serve::{Request, read_request, respond};

#[test]
fn floats_are_written_like_prometheus() {
    let cases = [(5e-6, "5e-06"), (1e-5, "1e-05"), (2.5e-5, "2.5e-05"), (1e-4, "0.0001"), (0.25, "0.25")];
    for (v, want) in cases {
        assert_eq!(go_float(v), want);
    }
    assert_eq!(go_float(1.0), "1");
    assert_eq!(go_float(100.0), "100");
    assert_eq!(go_float(f64::INFINITY), "+Inf");
}

/// Um `/proc` falso com o processo 4242.
fn fake_proc(dir: &Path) {
    let p = dir.join("4242");
    std::fs::create_dir_all(p.join("fd")).unwrap();
    for fd in ["0", "1", "2"] {
        std::fs::write(p.join("fd").join(fd), "").unwrap();
    }
    std::fs::write(
        p.join("stat"),
        "4242 (fire flower (x)) S 1 4242 4242 0 -1 4194304 1200 0 0 0 4310 813 0 0 20 0 17 0 123456 \
         8589934592 156250 18446744073709551615 1 1 0 0 0 0 0 4096 0 0 0 0 17 3 0 0 0 0 0",
    )
    .unwrap();
    std::fs::write(p.join("status"), "Name:\tfire-flower\nVmSize:\t 8388608 kB\nVmRSS:\t  626000 kB\n").unwrap();
    std::fs::write(p.join("io"), "rchar: 1\nwchar: 2\nread_bytes: 4096\nwrite_bytes: 8192\n").unwrap();
    std::fs::write(
        p.join("limits"),
        "Limit                     Soft Limit           Hard Limit           Units\n\
         Max open files            1024                 524288               files\n",
    )
    .unwrap();
    std::fs::write(dir.join("stat"), "cpu  1 2 3\nbtime 1761600000\nprocesses 9\n").unwrap();
}

#[test]
fn process_comes_from_proc() {
    let dir = tempfile::tempdir().unwrap();
    fake_proc(dir.path());
    let p = process::read(dir.path(), 4242).unwrap();
    assert_eq!(p.cpu_secs, 51.23);
    assert_eq!((p.threads, p.virtual_bytes), (17, 8_589_934_592));
    assert_eq!(p.resident_bytes, 626_000 * 1024, "VmRSS vale mais que o rss do stat");
    assert_eq!(p.start_unix, Some(1_761_600_000.0 + 1234.56));
    assert_eq!((p.open_fds, p.max_fds), (Some(3), Some(1024)));
    assert_eq!((p.io_read_bytes, p.io_write_bytes), (Some(4096), Some(8192)));
    assert!(process::read(dir.path(), 9999).is_none());
}

#[test]
fn df_output() {
    let text = "Filesystem     1-blocks      Used  Available Capacity Mounted on\n\
                /dev/sdd   1081101176832 151161004032 876942614528  15% /\n";
    assert_eq!(disk::parse_df(text), Some((876_942_614_528, 1_081_101_176_832)));
    assert_eq!(disk::parse_df(""), None);
}

fn reading() -> Reading {
    let mut buckets = vec![0; QUERY_BOUNDS.len() + 1];
    buckets[2] = 3;
    buckets[QUERY_BOUNDS.len()] = 1;
    let query = |op: &str, count: u64| OpCounters {
        op: op.into(),
        count,
        buckets: if count > 0 { buckets.clone() } else { vec![0; buckets.len()] },
        sum_nanos: 2_000_000_000,
        reads: 40,
        ..OpCounters::default()
    };
    let mut s = Snapshot {
        version: "0.1.0".into(),
        format_version: 7,
        queries: vec![query("search", 4), query("suggest", 0)],
        ..Snapshot::default()
    };
    s.writer.commit_buckets = vec![0; 17];
    s.writer.phase_nanos = vec![1_500_000_000; 7];
    s.writer.deleted_documents = 12;
    s.writer.dead_versions = 30;
    s.rocksdb = Some(fire_flower::metrics::RocksdbProps { sst_files: vec![2, 4, 0, 0, 0, 0, 0], ..Default::default() });
    Reading { snapshot: Some(s), snapshot_age_secs: Some(0.4), ..Reading::default() }
}

#[test]
fn openmetrics_text() {
    let text = render(&reading(), Format::OpenMetrics);
    assert!(text.ends_with("# EOF\n"));
    for line in [
        "# TYPE fireflower_queries counter",
        "fireflower_queries_total{op=\"search\"} 4",
        "# TYPE fireflower_query_duration_seconds histogram",
        "# UNIT fireflower_query_duration_seconds seconds",
        "fireflower_query_duration_seconds_bucket{op=\"search\",le=\"5e-06\"} 0",
        "fireflower_query_duration_seconds_bucket{op=\"search\",le=\"2.5e-05\"} 3",
        "fireflower_query_duration_seconds_bucket{op=\"search\",le=\"1\"} 3",
        "fireflower_query_duration_seconds_bucket{op=\"search\",le=\"+Inf\"} 4",
        "fireflower_query_duration_seconds_count{op=\"search\"} 4",
        "fireflower_query_duration_seconds_sum{op=\"search\"} 2",
        "fireflower_commit_phase_seconds_total{phase=\"write\"} 1.5",
        "fireflower_deleted_documents_total 12",
        "fireflower_dead_versions 30",
        "fireflower_rocksdb_sst_files{level=\"1\"} 4",
        "# TYPE fireflower_build info",
        "fireflower_build_info{version=\"0.1.0\",format_version=\"7\"} 1",
        "fireflower_exporter_snapshot_age_seconds 0.4",
    ] {
        assert!(text.lines().any(|l| l == line), "falta {line:?} em:\n{text}");
    }
    // Toda amostra pertence à família declarada logo antes dela.
    let mut family = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            family = rest.split(' ').next().unwrap().to_string();
        } else if !line.starts_with('#') {
            assert!(line.starts_with(&family), "{line:?} fora da família {family:?}");
        }
    }
}

#[test]
fn prometheus_text() {
    let text = render(&reading(), Format::Prometheus);
    assert!(!text.contains("# UNIT") && !text.contains("# EOF"));
    assert!(text.contains("# TYPE fireflower_queries_total counter\n"));
    assert!(text.contains("# TYPE fireflower_build_info gauge\n"));
    assert!(text.contains("# TYPE fireflower_query_duration_seconds histogram\n"));
}

#[test]
fn http_routes_and_content_negotiation() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = Source::new(dir.path(), None);
    let get = |path: &str, accept: &str| Request { method: "GET".into(), path: path.into(), accept: accept.into() };
    let scraper = get("/metrics", "application/openmetrics-text;version=1.0.0,*/*;q=0.1");
    let (status, content_type, _) = respond(&scraper, &mut source);
    assert_eq!((status, content_type), ("200 OK", Format::OpenMetrics.content_type()));
    let (_, content_type, body) = respond(&get("/metrics?x=1", "*/*"), &mut source);
    assert_eq!(content_type, Format::Prometheus.content_type());
    assert!(!body.contains("fireflower_queries"), "sem snapshot, só o que o monitor sabe sozinho");
    assert_eq!(respond(&get("/nada", ""), &mut source).0, "404 Not Found");
    let post = Request { method: "POST".into(), path: "/metrics".into(), accept: String::new() };
    assert_eq!(respond(&post, &mut source).0, "405 Method Not Allowed");

    let raw = "GET /metrics HTTP/1.1\r\nHost: x\r\nAccept: application/openmetrics-text\r\n\r\n";
    let request = read_request(&mut raw.as_bytes()).unwrap();
    assert_eq!((request.method.as_str(), request.path.as_str()), ("GET", "/metrics"));
    assert_eq!(request.accept, "application/openmetrics-text");
}

/// Um motor com métricas no próprio processo de teste: o monitor lê o
/// snapshot dele, o `/proc` dele e a pasta, e responde por um socket de verdade.
#[cfg(feature = "metrics")]
#[test]
fn scrape_of_a_live_engine() {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    let dir = tempfile::tempdir().unwrap();
    let config = fire_flower::Config { metrics: true, ..fire_flower::Config::default() };
    let engine = fire_flower::Engine::open(dir.path(), config).unwrap();
    engine.index_document(1, "monitor sem impacto").unwrap();
    engine.delete_document(1).unwrap();
    drop(engine); // grava o último snapshot

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut source = Source::new(dir.path(), None);
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = std::io::BufReader::new(&stream);
        let request = read_request(&mut reader).unwrap();
        let (status, content_type, body) = respond(&request, &mut source);
        let mut stream = &stream;
        write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n{body}").unwrap();
    });
    let mut client = TcpStream::connect(addr).unwrap();
    client.write_all(b"GET /metrics HTTP/1.1\r\nAccept: application/openmetrics-text\r\n\r\n").unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    server.join().unwrap();

    assert!(response.starts_with("HTTP/1.1 200 OK"));
    for line in [
        "fireflower_indexed_documents_total 1",
        "fireflower_deleted_documents_total 1",
        "fireflower_dead_versions 1",
        "fireflower_documents 0",
    ] {
        assert!(response.lines().any(|l| l == line), "falta {line:?}");
    }
    if cfg!(target_os = "linux") {
        assert!(response.contains("process_cpu_seconds_total "), "o /proc do próprio teste");
        assert!(response.contains("fireflower_disk_index_bytes{kind=\"sst\"}"));
    }
}
