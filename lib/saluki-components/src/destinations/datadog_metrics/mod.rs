use std::error::Error as _;

use async_trait::async_trait;
use http::{Request, Uri};
use http_body_util::BodyExt as _;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use metrics::Counter;
use saluki_config::GenericConfiguration;
use saluki_core::components::{destinations::*, ComponentContext, MetricsBuilder};
use saluki_error::GenericError;
use saluki_event::DataType;
use saluki_io::{
    buf::{get_fixed_bytes_buffer_pool, ChunkedBytesBuffer, ChunkedBytesBufferObjectPool},
    net::client::http::{ChunkedHttpsClient, HttpClient},
};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, trace, Instrument as _};

mod request_builder;
use self::request_builder::{MetricsEndpoint, RequestBuilder};

const DEFAULT_SITE: &str = "datadoghq.com";
const RB_BUFFER_POOL_COUNT: usize = 128;
const RB_BUFFER_POOL_BUF_SIZE: usize = 32_768;

fn default_site() -> String {
    DEFAULT_SITE.to_owned()
}

#[derive(Clone)]
struct Metrics {
    events_sent: Counter,
    bytes_sent: Counter,
    events_dropped_http: Counter,
    events_dropped_encoder: Counter,
    http_failed_send: Counter,
}

impl Metrics {
    fn from_component_context(context: ComponentContext) -> Self {
        let builder = MetricsBuilder::from_component_context(context);

        Self {
            events_sent: builder.register_counter("component_events_sent_total"),
            bytes_sent: builder.register_counter("component_bytes_sent_total"),
            events_dropped_http: builder.register_counter_with_labels(
                "component_events_dropped_total",
                &[("intentional", "false"), ("drop_reason", "http_failure")],
            ),
            events_dropped_encoder: builder.register_counter_with_labels(
                "component_events_dropped_total",
                &[("intentional", "false"), ("drop_reason", "encoder_failure")],
            ),
            http_failed_send: builder
                .register_counter_with_labels("component_errors_total", &[("error_type", "http_send")]),
        }
    }

    fn events_sent(&self) -> &Counter {
        &self.events_sent
    }

    fn bytes_sent(&self) -> &Counter {
        &self.bytes_sent
    }

    fn events_dropped_http(&self) -> &Counter {
        &self.events_dropped_http
    }

    fn events_dropped_encoder(&self) -> &Counter {
        &self.events_dropped_encoder
    }

    fn http_failed_send(&self) -> &Counter {
        &self.http_failed_send
    }
}

/// Datadog Metrics destination.
///
/// Forwards metrics to the Datadog platform. It can handle both series and sketch metrics, and only utilizes the latest
/// API endpoints for both, which both use Protocol Buffers-encoded payloads.
///
/// ## Missing
///
/// - ability to configure either the basic site _or_ a specific endpoint (requires a full URI at the moment, even if
///   it's just something like `https`)
/// - retries, timeouts, rate limiting (no Tower middleware stack yet)
#[derive(Deserialize)]
pub struct DatadogMetricsConfiguration {
    /// The API key to use.
    api_key: String,

    /// The site to send metrics to.
    ///
    /// This is the base domain for the Datadog site in which the API key originates from. This will generally be a
    /// portion of the domain used to access the Datadog UI, such as `datadoghq.com` or `us5.datadoghq.com`.
    ///
    /// Defaults to `datadoghq.com`.
    #[serde(default = "default_site")]
    site: String,

    /// The full URL base to send metrics to.
    ///
    /// This takes precedence over `site`, and is not altered in any way. This can be useful to specifying the exact
    /// endpoint used, such as when looking to change the scheme (e.g. `http` vs `https`) or specifying a custom port,
    /// which are both useful when proxying traffic to an intermediate destination before forwarding to Datadog.
    ///
    /// Defaults to unset.
    #[serde(default)]
    dd_url: Option<String>,
}

impl DatadogMetricsConfiguration {
    /// Creates a new `DatadogMetricsConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    fn api_base(&self) -> Result<Uri, GenericError> {
        match &self.dd_url {
            Some(url) => Uri::try_from(url).map_err(Into::into),
            None => {
                let site = if self.site.is_empty() {
                    DEFAULT_SITE
                } else {
                    self.site.as_str()
                };
                let authority = format!("api.{}", site);

                Uri::builder()
                    .scheme("https")
                    .authority(authority.as_str())
                    .path_and_query("/")
                    .build()
                    .map_err(Into::into)
            }
        }
    }
}

#[async_trait]
impl DestinationBuilder for DatadogMetricsConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::Metric
    }

    async fn build(&self) -> Result<Box<dyn Destination + Send>, GenericError> {
        let http_client = HttpClient::https()?;

        let api_base = self.api_base()?;

        let rb_buffer_pool = create_request_builder_buffer_pool();
        let series_request_builder = RequestBuilder::new(
            self.api_key.clone(),
            api_base.clone(),
            MetricsEndpoint::Series,
            rb_buffer_pool.clone(),
        )
        .await?;
        let sketches_request_builder = RequestBuilder::new(
            self.api_key.clone(),
            api_base,
            MetricsEndpoint::Sketches,
            rb_buffer_pool,
        )
        .await?;

        Ok(Box::new(DatadogMetrics {
            http_client,
            series_request_builder,
            sketches_request_builder,
        }))
    }
}

impl MemoryBounds for DatadogMetricsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // The request builder buffer pool is shared between both the series and the sketches request builder, so we
        // only count it once.
        let rb_buffer_pool_size = RB_BUFFER_POOL_COUNT * RB_BUFFER_POOL_BUF_SIZE;

        // Each request builder has a scratch buffer for encoding.
        //
        // TODO: Since it's just a `Vec<u8>`, it could trivially be expanded/grown for encoding larger payloads... which
        // we don't really have a good answer to here. Best thing would be to change the encoding logic to write
        // directly to the compressor but we have the current intermediate step to cope with avoiding writing more than
        // the (un)compressed payload limits and it will take a little work to eliminate that, I believe.
        let scratch_buffer_size = request_builder::SCRATCH_BUF_CAPACITY * 2;

        builder
            .minimum()
            .with_fixed_amount(rb_buffer_pool_size)
            .with_fixed_amount(scratch_buffer_size);
    }
}

pub struct DatadogMetrics {
    http_client: ChunkedHttpsClient,
    series_request_builder: RequestBuilder<ChunkedBytesBufferObjectPool>,
    sketches_request_builder: RequestBuilder<ChunkedBytesBufferObjectPool>,
}

#[async_trait]
impl Destination for DatadogMetrics {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), ()> {
        let Self {
            http_client,
            mut series_request_builder,
            mut sketches_request_builder,
        } = *self;

        // Spawn our IO task to handle sending requests.
        let (io_shutdown_tx, io_shutdown_rx) = oneshot::channel();
        let (requests_tx, requests_rx) = mpsc::channel(32);
        let metrics = Metrics::from_component_context(context.component_context());
        tokio::spawn(run_io_loop(requests_rx, io_shutdown_tx, http_client, metrics.clone()).in_current_span());

        debug!("Datadog Metrics destination started.");

        // Loop and process all incoming events.
        while let Some(event_buffers) = context.events().next_ready().await {
            debug!(event_buffers_len = event_buffers.len(), "Received event buffers.");

            for event_buffer in event_buffers {
                debug!(events_len = event_buffer.len(), "Processing event buffer.");

                for event in event_buffer {
                    if let Some(metric) = event.into_metric() {
                        let request_builder = match MetricsEndpoint::from_metric(&metric) {
                            MetricsEndpoint::Series => &mut series_request_builder,
                            MetricsEndpoint::Sketches => &mut sketches_request_builder,
                        };

                        // Encode the metric. If we get it back, that means the current request is full, and we need to
                        // flush it before we can try to encode the metric again... so we'll hold on to it in that case
                        // before flushing and trying to encode it again.
                        let metric_to_retry = match request_builder.encode(metric).await {
                            Ok(None) => continue,
                            Ok(Some(metric)) => metric,
                            Err(e) => {
                                error!(error = %e, "Failed to encode metric.");
                                metrics.events_dropped_encoder().increment(1);
                                continue;
                            }
                        };

                        // Get the flushed request and enqueue it to be sent.
                        match request_builder.flush().await {
                            Ok(Some((metrics_written, request))) => {
                                if requests_tx.send((metrics_written, request)).await.is_err() {
                                    error!("Failed to send request to IO task: receiver dropped.");
                                    return Err(());
                                }
                            }
                            Ok(None) => unreachable!(
                                "request builder indicated required flush, but no request was given during flush"
                            ),
                            Err(e) => {
                                error!(error = %e, "Failed to flush request.");
                                // TODO: Increment a counter here that metrics were dropped due to a flush failure.
                                if e.is_recoverable() {
                                    // If the error is recoverable, we'll hold on to the metric to retry it later.
                                    continue;
                                } else {
                                    return Err(());
                                }
                            }
                        };

                        // Now try to encode the metric again. If it fails again, we'll just log it because it shouldn't
                        // be possible to fail at this point, otherwise we would have already caught that the first
                        // time.
                        if let Err(e) = request_builder.encode(metric_to_retry).await {
                            error!(error = %e, "Failed to encode metric.");
                            metrics.events_dropped_encoder().increment(1);
                        }
                    }
                }
            }

            debug!("All event buffers processed.");

            // Once we've encoded and written all metrics, we flush the request builders to generate a request with
            // anything left over. Again, we'll  enqueue those requests to be sent immediately.
            match series_request_builder.flush().await {
                Ok(Some(request)) => {
                    debug!("Flushed request from series request builder. Sending to I/O task...");
                    if requests_tx.send(request).await.is_err() {
                        error!("Failed to send request to IO task: receiver dropped.");
                        return Err(());
                    }
                }
                Ok(None) => {
                    trace!("No flushed request from series request builder.");
                }
                Err(e) => {
                    error!(error = %e, "Failed to flush request.");
                    return Err(());
                }
            };

            match sketches_request_builder.flush().await {
                Ok(Some(request)) => {
                    debug!("Flushed request from sketches request builder. Sending to I/O task...");
                    if requests_tx.send(request).await.is_err() {
                        error!("Failed to send request to IO task: receiver dropped.");
                        return Err(());
                    }
                }
                Ok(None) => {
                    trace!("No flushed request from sketches request builder.");
                }
                Err(e) => {
                    error!(error = %e, "Failed to flush request.");
                    return Err(());
                }
            };

            debug!("All flushed requests sent to I/O task. Waiting for next event buffer...");
        }

        // Drop the requests channel, which allows the IO task to naturally shut down once it has received and sent all
        // requests. We then wait for it to signal back to us that it has stopped before exiting ourselves.
        drop(requests_tx);
        let _ = io_shutdown_rx.await;

        debug!("Datadog Metrics destination stopped.");

        Ok(())
    }
}

async fn run_io_loop(
    mut requests_rx: mpsc::Receiver<(usize, Request<ChunkedBytesBuffer>)>, io_shutdown_tx: oneshot::Sender<()>,
    http_client: ChunkedHttpsClient, metrics: Metrics,
) {
    // Loop and process all incoming requests.
    while let Some((metrics_count, request)) = requests_rx.recv().await {
        // TODO: This doesn't include the actual headers, or the HTTP framing, or anything... so it's a darn good
        // approximation, but still only an approximation.
        let request_length = request.body().len();

        match http_client.send(request).await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    debug!(%status, "Request sent.");

                    metrics.events_sent().increment(metrics_count as u64);
                    metrics.bytes_sent().increment(request_length as u64);
                } else {
                    metrics.http_failed_send().increment(1);
                    metrics.events_dropped_http().increment(metrics_count as u64);

                    match response.into_body().collect().await {
                        Ok(body) => {
                            let body = body.to_bytes();
                            let body_str = String::from_utf8_lossy(&body[..]);
                            error!(%status, "Received non-success response. Body: {}", body_str);
                        }
                        Err(e) => {
                            error!(%status, error = %e, "Failed to read response body of non-success response.");
                        }
                    }
                }
            }
            Err(e) => {
                error!(error = %e, error_source = ?e.source(), "Failed to send request.");
            }
        }
    }

    // Signal back to the main task that we've stopped.
    let _ = io_shutdown_tx.send(());
}

fn create_request_builder_buffer_pool() -> ChunkedBytesBufferObjectPool {
    // Create the underlying fixed-size buffer pool for the individual chunks.
    //
    // This is 4MB total, in 32KB chunks, which ensures we have enough to simultaneously encode a request for the
    // Series/Sketch V1 endpoint (max of 3.2MB) as well as the Series V2 endpoint (max 512KB).
    //
    // We chunk it up into 32KB segments mostly to allow for balancing fragmentation vs acquisition overhead.
    let pool = get_fixed_bytes_buffer_pool(RB_BUFFER_POOL_COUNT, RB_BUFFER_POOL_BUF_SIZE);

    // Turn it into a chunked buffer pool.
    //
    // `ChunkedBytesBuffer` is an optimized buffer type for writing where the target is eventually an HTTP request. This
    // is because it can be grown dynamically by acquiring more "chunks" from the wrapped buffer pool, which are then
    // written into. As well, it can be used as a `Body` type for `hyper` requests.
    ChunkedBytesBufferObjectPool::new(pool)
}
