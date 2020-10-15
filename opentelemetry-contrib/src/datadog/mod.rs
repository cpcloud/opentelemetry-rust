//! # OpenTelemetry Datadog Exporter
//!
//! An OpenTelemetry exporter implementation
//!
//! See the [Datadog Docs](https://docs.datadoghq.com/agent/) for information on how to run the datadog-agent
//!
//! ## Quirks
//!
//! There are currently some incompatibilities between Datadog and OpenTelemetry, and this manifests
//! as minor quirks to this exporter.
//!
//! Firstly Datadog uses operation_name to describe what OpenTracing would call a component.
//! Or to put it another way, in OpenTracing the operation / span name's are relatively
//! granular and might be used to identify a specific endpoint. In datadog, however, they
//! are less granular - it is expected in Datadog that a service will have single
//! primary span name that is the root of all traces within that service, with an additional piece of
//! metadata called resource_name providing granularity - https://docs.datadoghq.com/tracing/guide/configuring-primary-operation/
//!
//! The Datadog Golang API takes the approach of using a `resource.name` OpenTelemetry attribute to set the
//! resource_name - https://github.com/DataDog/dd-trace-go/blob/ecb0b805ef25b00888a2fb62d465a5aa95e7301e/ddtrace/opentracer/tracer.go#L10
//!
//! Unfortunately, this breaks compatibility with other OpenTelemetry exporters which expect
//! a more granular operation name - as per the OpenTracing specification.
//!
//! This exporter therefore takes a different approach of naming the span with the name of the
//! tracing provider, and using the span name to set the resource_name. This should in most cases
//! lead to the behaviour that users expect.
//!
//! Datadog additionally has a span_type string that alters the rendering of the spans in the web UI.
//! This can be set as the `span.type` OpenTelemetry span attribute.
//!
//! For standard values see here - https://github.com/DataDog/dd-trace-go/blob/ecb0b805ef25b00888a2fb62d465a5aa95e7301e/ddtrace/ext/app_types.go#L31
//!
//! ## Performance
//!
//! For optimal performance, a batch exporter is recommended as the simple
//! exporter will export each span synchronously on drop. You can enable the
//! [`tokio`] or [`async-std`] features to have a batch exporter configured for
//! you automatically for either executor when you install the pipeline.
//!
//! ```toml
//! [dependencies]
//! opentelemetry = { version = "*", features = ["tokio"] }
//! opentelemetry-datadog = "*"
//! ```
//!
//! [`tokio`]: https://tokio.rs
//! [`async-std`]: https://async.rs
//!

//! ## Bring your own http client
//!
//! Users can choose appropriate http clients to align with their runtime.
//!
//! Based on the feature enabled. The default http client will be different. If user doesn't specific
//! features or enabled `reqwest-blocking` feature. The blocking reqwest http client will be used as
//! default client. If `reqwest` feature is enabled. The async reqwest http client will be used. If
//! `surf` feature is enabled. The surf http client will be used.
//!
//! Note that async http clients may need specific runtime otherwise it will panic. User should make
//! sure the http client is running in appropriate runime.
//!
//! Users can always use their own http clients by implementing `HttpClient` trait.
//!
//! ## Kitchen Sink Full Configuration
//!
//! Example showing how to override all configuration options. See the
//! [`DatadogPipelineBuilder`] docs for details of each option.
//!
//! [`DatadogPipelineBuilder`]: struct.DatadogPipelineBuilder.html
//!
//! ```no_run
//! use opentelemetry::api::{KeyValue, trace::Tracer};
//! use opentelemetry::sdk::{trace::{self, IdGenerator, Sampler}, Resource};
//! use opentelemetry::exporter::trace::ExportResult;
//! use opentelemetry::exporter::trace::HttpClient;
//! use async_trait::async_trait;
//! use std::error::Error;
//!
//! // `reqwest` and `surf` are supported through features, if you prefer an
//! // alternate http client you can add support by implementing `HttpClient` as
//! // shown here.
//! #[derive(Debug)]
//! struct IsahcClient(isahc::HttpClient);
//!
//! #[async_trait]
//! impl HttpClient for IsahcClient {
//!   type Body = Vec<u8>;
//!   async fn send(&self, request: http::Request<Self::Body>) -> Result<ExportResult, Box<dyn Error>> {
//!     let result = self.0.send_async(request).await?;
//!
//!     Ok(if result.status().is_success() {
//!       ExportResult::Success
//!     } else {
//!       ExportResult::FailedNotRetryable
//!     })
//!   }
//! }
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let (tracer, _uninstall) =
//!     opentelemetry_contrib::datadog::new_pipeline(IsahcClient(isahc::HttpClient::new()))
//!         .with_service_name("my_app")
//!         .with_version(opentelemetry_contrib::datadog::ApiVersion::Version05)
//!         .with_agent_endpoint("http://localhost:8126")
//!         .with_trace_config(
//!             trace::config()
//!                 .with_default_sampler(Sampler::AlwaysOn)
//!                 .with_id_generator(IdGenerator::default())
//!         )
//!         .install()?;
//!
//!     tracer.in_span("doing_work", |cx| {
//!         // Traced app logic here...
//!     });
//!
//!     Ok(())
//! }
//! ```
#![deny(missing_docs, unreachable_pub, missing_debug_implementations)]
#![cfg_attr(test, deny(warnings))]

mod intern;
mod model;

pub use model::ApiVersion;

use async_trait::async_trait;
use http::{Method, Request, Uri};
use opentelemetry::exporter::trace;
use opentelemetry::exporter::trace::{HttpClient, SpanData};
use opentelemetry::{api::trace::TracerProvider, global, sdk};
use std::error::Error;

/// Default Datadog collector endpoint
const DEFAULT_AGENT_ENDPOINT: &str = "http://127.0.0.1:8126";

/// Default service name if no service is configured.
const DEFAULT_SERVICE_NAME: &str = "OpenTelemetry";

/// Datadog span exporter
#[derive(Debug)]
pub struct DatadogExporter<C> {
    client: C,
    request_url: Uri,
    service_name: String,
    version: ApiVersion,
}

impl<C> DatadogExporter<C> {
    fn new(service_name: String, request_url: Uri, version: ApiVersion, client: C) -> Self {
        Self {
            client,
            request_url,
            service_name,
            version,
        }
    }
}

/// Builder for `ExporterConfig` struct.
#[derive(Debug)]
pub struct DatadogPipelineBuilder<C> {
    service_name: String,
    agent_endpoint: String,
    trace_config: Option<sdk::trace::Config>,
    version: ApiVersion,
    client: C,
}

/// Create a new pipeline for Datadog publishing
pub fn new_pipeline() -> DatadogPipelineBuilder<impl HttpClient> {
    DatadogPipelineBuilder::new(reqwest::Client::new())
}

impl<C> DatadogPipelineBuilder<C> {
    /// Create a new DatadogPipelineBuilder with a particular client
    pub fn new(client: C) -> Self {
        Self {
            service_name: DEFAULT_SERVICE_NAME.to_string(),
            agent_endpoint: DEFAULT_AGENT_ENDPOINT.to_string(),
            trace_config: None,
            version: ApiVersion::Version05,
            client,
        }
    }

    /// Create `ExporterConfig` struct from current `ExporterConfigBuilder`
    pub fn install(
        mut self,
    ) -> Result<(sdk::trace::Tracer, Uninstall), Box<dyn Error + Send + Sync + 'static>>
    where
        C: HttpClient + std::fmt::Debug + Send + Sync + 'static,
    {
        let endpoint = self.agent_endpoint + self.version.path();
        let exporter = DatadogExporter::new(
            self.service_name.clone(),
            endpoint.parse()?,
            self.version,
            self.client,
        );
        let mut provider_builder = sdk::trace::TracerProvider::builder().with_exporter(exporter);
        if let Some(config) = self.trace_config.take() {
            provider_builder = provider_builder.with_config(config);
        }
        let provider = provider_builder.build();
        let tracer = provider.get_tracer("opentelemetry-datadog", Some(env!("CARGO_PKG_VERSION")));
        let provider_guard = global::set_tracer_provider(provider);
        Ok((tracer, Uninstall(provider_guard)))
    }

    /// Assign the service name under which to group traces
    pub fn with_service_name(mut self, name: impl Into<String>) -> Self {
        self.service_name = name.into();
        self
    }

    /// Assign the Datadog collector endpoint
    pub fn with_agent_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.agent_endpoint = endpoint.into();
        self
    }

    /// Assign the SDK trace configuration
    pub fn with_trace_config(mut self, config: sdk::trace::Config) -> Self {
        self.trace_config = Some(config);
        self
    }

    /// Set version of Datadog trace ingestion API
    pub fn with_version(mut self, version: ApiVersion) -> Self {
        self.version = version;
        self
    }
}

#[async_trait]
impl<C: HttpClient + std::fmt::Debug + Send + Sync> trace::SpanExporter for DatadogExporter<C> {
    /// Export spans to datadog-agent
    async fn export(&self, batch: Vec<SpanData>) -> trace::ExportResult {
        let data = match self.version.encode(&self.service_name, batch) {
            Ok(data) => data,
            Err(_) => return trace::ExportResult::FailedNotRetryable,
        };

        let req = match Request::builder()
            .method(Method::POST)
            .uri(self.request_url.clone())
            .header(http::header::CONTENT_TYPE, self.version.content_type())
            .body(data)
        {
            Ok(req) => req,
            _ => return trace::ExportResult::FailedNotRetryable,
        };
        self.client
            .send(req)
            .await
            .unwrap_or(trace::ExportResult::FailedNotRetryable)
    }
}

/// Uninstalls the Datadog pipeline on drop
#[derive(Debug)]
pub struct Uninstall(global::TracerProviderGuard);
