# Monitoramento com Prometheus e Grafana

Exemplos sobre as métricas do `fire-flower monitor --serve` (a lista e os
nomes estão em [docs/plano-monitoramento.md](../../docs/plano-monitoramento.md)).

1. O processo que grava o banco abre o `Engine` com `Config { metrics: true, .. }`
   (ou `fire-flower index-jsonl --metrics`): uma thread grava o snapshot das
   métricas na pasta do banco a cada segundo.
2. Ao lado dele, `fire-flower monitor <banco> --serve 127.0.0.1:9187` serve o
   `/metrics`. O monitor só lê o snapshot, o `/proc` do processo e a pasta: o
   scrape não custa nada ao motor.
3. No `prometheus.yml`:

   ```yaml
   rule_files: [prometheus-alerts.yml]
   scrape_configs:
     - job_name: fire-flower
       static_configs: [{targets: ["127.0.0.1:9187"]}]
   ```

- [`prometheus-alerts.yml`](prometheus-alerts.yml): motor parado, escrita
  parada, erros do RocksDB e dos commits, L0 e compactação atrasando, disco
  cheio, versões mortas demais (hora de compactar), consultas com erro ou
  lentas.
- [`grafana-dashboard.json`](grafana-dashboard.json): importe no Grafana
  (Dashboards → Import) e escolha o Prometheus. Busca (consultas/s, p50 e p99,
  Gets por consulta, caches), indexação, edição e remoção (substituídos e
  removidos por segundo, versões mortas, edições pendentes, compactações),
  RocksDB, memória e disco.

Para conferir antes de usar:

```bash
promtool check rules contrib/monitoring/prometheus-alerts.yml
```

```bash
curl -s localhost:9187/metrics | promtool check metrics
```
