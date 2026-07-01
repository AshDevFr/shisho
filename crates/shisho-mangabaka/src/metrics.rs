//! OTLP instruments for the backbone, on the global meter (a no-op unless
//! `serve` installed an exporter): refresh duration, dump row-count gauge,
//! API-fallback hits, negative-cache hits.

use std::sync::OnceLock;

use opentelemetry::metrics::{Counter, Gauge, Histogram};

pub(crate) struct Instruments {
    pub refresh_duration: Histogram<f64>,
    pub dump_series_rows: Gauge<u64>,
    pub api_fallback_hits: Counter<u64>,
    pub negative_cache_hits: Counter<u64>,
}

pub(crate) fn instruments() -> &'static Instruments {
    static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();
    INSTRUMENTS.get_or_init(|| {
        let meter = opentelemetry::global::meter("shisho-mangabaka");
        Instruments {
            refresh_duration: meter
                .f64_histogram("mangabaka.refresh.duration")
                .with_unit("s")
                .build(),
            dump_series_rows: meter.u64_gauge("mangabaka.dump.series_rows").build(),
            api_fallback_hits: meter.u64_counter("mangabaka.api_fallback.requests").build(),
            negative_cache_hits: meter.u64_counter("mangabaka.negative_cache.hits").build(),
        }
    })
}
