// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! logging stuffs, inspired by databend
use std::env;
use std::sync::{Arc, Mutex, Once};

use once_cell::sync::Lazy;
use opentelemetry::global;
use opentelemetry::sdk::propagation::TraceContextPropagator;
use serde::{Deserialize, Serialize};
pub use tracing::{event, span, Level};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_log::LogTracer;
use tracing_subscriber::fmt::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{filter, EnvFilter, Registry};

pub use crate::{debug, error, info, log, trace, warn};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingOptions {
    pub dir: String,
    pub level: String,
    pub enable_jaeger_tracing: bool,
}

impl Default for LoggingOptions {
    fn default() -> Self {
        Self {
            dir: "/tmp/greptimedb/logs".to_string(),
            level: "info".to_string(),
            enable_jaeger_tracing: false,
        }
    }
}

/// Init tracing for unittest.
/// Write logs to file `unittest`.
pub fn init_default_ut_logging() {
    static START: Once = Once::new();

    START.call_once(|| {
        let mut g = GLOBAL_UT_LOG_GUARD.as_ref().lock().unwrap();

        // When running in Github's actions, env "UNITTEST_LOG_DIR" is set to a directory other
        // than "/tmp".
        // This is to fix the problem that the "/tmp" disk space of action runner's is small,
        // if we write testing logs in it, actions would fail due to disk out of space error.
        let dir =
            env::var("UNITTEST_LOG_DIR").unwrap_or_else(|_| "/tmp/__unittest_logs".to_string());

        let level = env::var("UNITTEST_LOG_LEVEL").unwrap_or_else(|_| "DEBUG".to_string());
        let opts = LoggingOptions {
            dir: dir.clone(),
            level,
            ..Default::default()
        };
        *g = Some(init_global_logging("unittest", &opts));

        info!("logs dir = {}", dir);
    });
}

static GLOBAL_UT_LOG_GUARD: Lazy<Arc<Mutex<Option<Vec<WorkerGuard>>>>> =
    Lazy::new(|| Arc::new(Mutex::new(None)));

pub fn init_global_logging(app_name: &str, opts: &LoggingOptions) -> Vec<WorkerGuard> {
    let mut guards = vec![];
    let dir = &opts.dir;
    let level = &opts.level;
    let enable_jaeger_tracing = opts.enable_jaeger_tracing;

    // Enable log compatible layer to convert log record to tracing span.
    LogTracer::init().expect("log tracer must be valid");

    // Stdout layer.
    let (stdout_writer, stdout_guard) = tracing_appender::non_blocking(std::io::stdout());
    let stdout_logging_layer = Layer::new().with_writer(stdout_writer);
    guards.push(stdout_guard);

    // JSON log layer.
    let rolling_appender = RollingFileAppender::new(Rotation::HOURLY, dir, app_name);
    let (rolling_writer, rolling_writer_guard) = tracing_appender::non_blocking(rolling_appender);
    let file_logging_layer = BunyanFormattingLayer::new(app_name.to_string(), rolling_writer);
    guards.push(rolling_writer_guard);

    // error JSON log layer.
    let err_rolling_appender =
        RollingFileAppender::new(Rotation::HOURLY, dir, format!("{}-{}", app_name, "err"));
    let (err_rolling_writer, err_rolling_writer_guard) =
        tracing_appender::non_blocking(err_rolling_appender);
    let err_file_logging_layer =
        BunyanFormattingLayer::new(app_name.to_string(), err_rolling_writer);
    guards.push(err_rolling_writer_guard);

    // Use env RUST_LOG to initialize log if present.
    // Otherwise use the specified level.
    let directives = env::var(EnvFilter::DEFAULT_ENV).unwrap_or_else(|_x| level.to_string());
    let filter = filter::Targets::new()
        // Only enable WARN and ERROR for 3rd-party crates
        // TODO(dennis): configure them?
        .with_target("hyper", Level::WARN)
        .with_target("tower", Level::WARN)
        .with_target("datafusion", Level::WARN)
        .with_target("reqwest", Level::WARN)
        .with_target("sqlparser", Level::WARN)
        .with_target("h2", Level::INFO)
        .with_default(
            directives
                .parse::<filter::LevelFilter>()
                .expect("error parsing level string"),
        );

    let subscriber = Registry::default()
        .with(filter)
        .with(JsonStorageLayer)
        .with(stdout_logging_layer)
        .with(file_logging_layer)
        .with(err_file_logging_layer.with_filter(filter::LevelFilter::ERROR));

    // Must enable 'tokio_unstable' cfg, https://github.com/tokio-rs/console
    #[cfg(feature = "console")]
    let subscriber = subscriber.with(console_subscriber::spawn());

    if enable_jaeger_tracing {
        // Jaeger layer.
        global::set_text_map_propagator(TraceContextPropagator::new());
        let tracer = opentelemetry_jaeger::new_pipeline()
            .with_service_name(app_name)
            .install_batch(opentelemetry::runtime::Tokio)
            .expect("install");
        let jaeger_layer = Some(tracing_opentelemetry::layer().with_tracer(tracer));
        let subscriber = subscriber.with(jaeger_layer);
        tracing::subscriber::set_global_default(subscriber)
            .expect("error setting global tracing subscriber");
    } else {
        tracing::subscriber::set_global_default(subscriber)
            .expect("error setting global tracing subscriber");
    }

    guards
}
