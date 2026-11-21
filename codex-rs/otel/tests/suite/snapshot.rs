use crate::harness::attributes_to_map;
use crate::harness::find_metric;
use codex_otel::OtelManager;
use codex_otel::TelemetryAuthMode;
use codex_otel::metrics::MetricsClient;
use codex_otel::metrics::MetricsConfig;
use codex_otel::metrics::Result;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::MetricData;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[test]
fn completed_responses_record_bounded_replay_reports() -> Result<()> {
    use codex_api::sse::responses::process_responses_event;
    use serde_json::json;

    let metrics = MetricsClient::new(
        MetricsConfig::in_memory(
            "test",
            "codex-cli",
            codex_otel::CODEX_VERSION,
            InMemoryMetricExporter::default(),
        )
        .with_runtime_reader(),
    )?;
    let manager = OtelManager::new(
        ThreadId::new(),
        "m",
        "m",
        None,
        None,
        None,
        false,
        "test".into(),
        SessionSource::Cli,
    )
    .with_metrics(metrics.clone());
    for report in [
        None,
        Some(json!(null)),
        Some(json!("malformed")),
        Some(json!([])),
        Some(json!([
            {"type":"reasoning","action":"removed","reason":"prefix_mismatch"},
            {"type":"reasoning","action":"removed","reason":"unknown"},
            {"type":"reasoning","action":"removed"},
            {"type":"message","action":"removed","reason":"prefix_mismatch"},
            {"type":"reasoning","action":"changed","reason":"prefix_mismatch"}
        ])),
        Some(json!([{"type":"reasoning","action":"removed","reason":"turn_boundary"}])),
    ] {
        let mut response = json!({"id":"r"});
        if let Some(report) = report {
            response["input_transformations"] = report;
        }
        let wire = serde_json::from_value(json!({"type":"response.completed","response":response}))
            .unwrap();
        let event = process_responses_event(wire).unwrap().unwrap();
        manager.record_responses(&tracing::Span::none(), &event);
    }
    let snapshot = metrics.snapshot()?;
    let counter = find_metric(&snapshot, "codex.responses.reasoning_replay").unwrap();
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = counter.data() else {
        panic!("expected a counter")
    };
    let reports = sum
        .data_points()
        .map(|p| {
            (
                attributes_to_map(p.attributes())["report"].clone(),
                p.value(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        reports,
        BTreeMap::from([
            ("unavailable".into(), 3),
            ("no_removals_reported".into(), 1),
            ("prefix_mismatch_reported".into(), 1),
            ("removals_reported".into(), 1),
        ])
    );
    let histogram = find_metric(&snapshot, "codex.responses.reasoning_items_removed").unwrap();
    let AggregatedMetrics::F64(MetricData::Histogram(hist)) = histogram.data() else {
        panic!("expected a histogram")
    };
    let removed = hist
        .data_points()
        .map(|p| {
            (
                attributes_to_map(p.attributes())["reason"].clone(),
                (p.count(), p.sum()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        removed,
        BTreeMap::from([
            ("prefix_mismatch".into(), (3, 1.)),
            ("other".into(), (3, 2.)),
            ("turn_boundary".into(), (3, 1.)),
            ("model_mismatch".into(), (3, 0.)),
            ("organization_mismatch".into(), (3, 0.)),
            ("end_user_mismatch".into(), (3, 0.)),
        ])
    );
    Ok(())
}

#[test]
fn snapshot_collects_metrics_without_shutdown() -> Result<()> {
    let exporter = InMemoryMetricExporter::default();
    let config = MetricsConfig::in_memory(
        "test",
        "codex-cli",
        codex_otel::CODEX_VERSION,
        exporter.clone(),
    )
    .with_tag("service", "codex-cli")?
    .with_runtime_reader();
    let metrics = MetricsClient::new(config)?;

    metrics.counter(
        "codex.tool.call",
        1,
        &[("tool", "shell"), ("success", "true")],
    )?;

    let snapshot = metrics.snapshot()?;

    let metric = find_metric(&snapshot, "codex.tool.call").expect("counter metric missing");
    let attrs = match metric.data() {
        AggregatedMetrics::U64(data) => match data {
            MetricData::Sum(sum) => {
                let points: Vec<_> = sum.data_points().collect();
                assert_eq!(points.len(), 1);
                attributes_to_map(points[0].attributes())
            }
            _ => panic!("unexpected counter aggregation"),
        },
        _ => panic!("unexpected counter data type"),
    };

    let expected = BTreeMap::from([
        ("service".to_string(), "codex-cli".to_string()),
        ("success".to_string(), "true".to_string()),
        ("tool".to_string(), "shell".to_string()),
    ]);
    assert_eq!(attrs, expected);

    let finished = exporter
        .get_finished_metrics()
        .expect("finished metrics should be readable");
    assert!(finished.is_empty(), "expected no periodic exports yet");

    Ok(())
}

#[test]
fn manager_snapshot_metrics_collects_without_shutdown() -> Result<()> {
    let exporter = InMemoryMetricExporter::default();
    let config = MetricsConfig::in_memory("test", "codex-cli", codex_otel::CODEX_VERSION, exporter)
        .with_tag("service", "codex-cli")?
        .with_runtime_reader();
    let metrics = MetricsClient::new(config)?;
    let manager = OtelManager::new(
        ThreadId::new(),
        "gpt-5.1",
        "gpt-5.1",
        Some("account-id".to_string()),
        None,
        Some(TelemetryAuthMode::ApiKey),
        true,
        "tty".to_string(),
        SessionSource::Cli,
    )
    .with_metrics(metrics);

    manager.counter(
        "codex.tool.call",
        1,
        &[("tool", "shell"), ("success", "true")],
    );

    let snapshot = manager.snapshot_metrics()?;
    let metric = find_metric(&snapshot, "codex.tool.call").expect("counter metric missing");
    let attrs = match metric.data() {
        AggregatedMetrics::U64(data) => match data {
            MetricData::Sum(sum) => {
                let points: Vec<_> = sum.data_points().collect();
                assert_eq!(points.len(), 1);
                attributes_to_map(points[0].attributes())
            }
            _ => panic!("unexpected counter aggregation"),
        },
        _ => panic!("unexpected counter data type"),
    };

    let expected = BTreeMap::from([
        (
            "app.version".to_string(),
            codex_otel::CODEX_VERSION.to_string(),
        ),
        (
            "auth_mode".to_string(),
            TelemetryAuthMode::ApiKey.to_string(),
        ),
        ("model".to_string(), "gpt-5.1".to_string()),
        ("service".to_string(), "codex-cli".to_string()),
        ("session_source".to_string(), "cli".to_string()),
        ("success".to_string(), "true".to_string()),
        ("tool".to_string(), "shell".to_string()),
    ]);
    assert_eq!(attrs, expected);

    Ok(())
}
