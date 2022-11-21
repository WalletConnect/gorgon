use {
    super::{AnalyticsCollector, AnalyticsEvent},
    async_trait::async_trait,
    std::{
        fmt::{Debug, Display},
        io::Write,
        marker::PhantomData,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    },
    tokio::{
        sync::mpsc::{self, error::TrySendError},
        time::MissedTickBehavior,
    },
    tracing::error,
};

mod aws;
mod parquet;
#[cfg(test)]
mod tests;

pub use {
    self::parquet::{create_parquet_collector, ParquetWriter, ParquetWriterError},
    aws::{AwsExporter, AwsExporterOpts},
};

#[derive(Debug, thiserror::Error)]
pub enum BatchCollectorError<T: Debug + Display> {
    #[error("event queue overflow")]
    EventQueueOverflow,

    #[error("event queue channel closed")]
    EventQueueChannelClosed,

    #[error("writer error: {0}")]
    Writer(T),
}

impl<T, E> From<TrySendError<T>> for BatchCollectorError<E>
where
    T: AnalyticsEvent,
    E: Debug + Display,
{
    fn from(val: TrySendError<T>) -> Self {
        match val {
            TrySendError::Full(_) => Self::EventQueueOverflow,
            TrySendError::Closed(_) => Self::EventQueueChannelClosed,
        }
    }
}

#[derive(Debug)]
enum ControlEvent<T> {
    Process(T),
    Shutdown,
}

/// Wrapper around the `Vec<u8>` buffer to accurately report the underlying
/// buffer size after giving away its ownership.
pub struct Buffer {
    data: Vec<u8>,
    size_bytes: Arc<AtomicUsize>,
}

impl Buffer {
    fn new(capacity: usize) -> Self {
        let data = Vec::with_capacity(capacity);
        let size_bytes = Arc::new(AtomicUsize::new(0));
        Self { data, size_bytes }
    }

    fn into_inner(self) -> Vec<u8> {
        self.data
    }

    fn size_bytes(&self) -> &Arc<AtomicUsize> {
        &self.size_bytes
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let len = self.data.write(buf)?;
        self.size_bytes.fetch_add(len, Ordering::Relaxed);
        Ok(len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.data.flush()
    }
}

pub trait BatchWriter<T: AnalyticsEvent>: 'static + Send + Sync + Sized {
    type Error: Debug + Display;

    fn create(buffer: Buffer, opts: &BatchCollectorOpts) -> Result<Self, Self::Error>;

    fn write(&mut self, data: T) -> Result<(), Self::Error>;

    fn flush(&mut self) -> Result<(), Self::Error>;

    fn into_buffer(self) -> Result<Vec<u8>, Self::Error>;
}

struct Batch<T: AnalyticsEvent, W: BatchWriter<T>> {
    data: W,
    num_rows: usize,
    size_bytes: Arc<AtomicUsize>,
    expiration: Instant,
    has_expiration: bool,
    _marker: PhantomData<T>,
}

impl<T, W> Batch<T, W>
where
    T: AnalyticsEvent,
    W: BatchWriter<T>,
{
    fn new(opts: &BatchCollectorOpts) -> Result<Self, BatchCollectorError<W::Error>> {
        let buffer = Buffer::new(opts.batch_alloc_size);
        let size_bytes = buffer.size_bytes().clone();
        let data = W::create(buffer, opts).map_err(BatchCollectorError::Writer)?;

        Ok(Self {
            data,
            num_rows: 0,
            size_bytes,
            expiration: Instant::now(),
            has_expiration: false,
            _marker: PhantomData,
        })
    }

    fn write(&mut self, data: T) -> Result<usize, BatchCollectorError<W::Error>> {
        self.data.write(data).map_err(BatchCollectorError::Writer)?;
        self.num_rows += 1;
        Ok(self.size_bytes())
    }

    fn flush(&mut self) -> Result<(), BatchCollectorError<W::Error>> {
        self.data.flush().map_err(BatchCollectorError::Writer)
    }

    fn num_rows(&self) -> usize {
        self.num_rows
    }

    fn size_bytes(&self) -> usize {
        self.size_bytes.load(Ordering::Relaxed)
    }

    fn has_expiration(&self) -> bool {
        self.has_expiration
    }

    fn set_expiration(&mut self, expiration: Instant) {
        self.has_expiration = true;
        self.expiration = expiration;
    }

    fn expiration(&self) -> Instant {
        self.expiration
    }

    fn into_buffer(self) -> Result<Vec<u8>, BatchCollectorError<W::Error>> {
        self.data.into_buffer().map_err(BatchCollectorError::Writer)
    }
}

#[async_trait]
pub trait BatchExporter: 'static + Clone + Send {
    type Error: Debug + Display;

    async fn export(self, data: Vec<u8>) -> Result<(), Self::Error>;
}

struct Batcher<T: AnalyticsEvent, E: BatchExporter, W: BatchWriter<T>> {
    opts: BatchCollectorOpts,
    ctrl_rx: mpsc::Receiver<ControlEvent<T>>,
    batch: Batch<T, W>,
    exporter: E,
}

impl<T, E, W> Batcher<T, E, W>
where
    T: AnalyticsEvent,
    E: BatchExporter,
    W: BatchWriter<T>,
{
    fn new(
        opts: impl Into<BatchCollectorOpts>,
        ctrl_rx: mpsc::Receiver<ControlEvent<T>>,
        exporter: E,
    ) -> Result<Self, BatchCollectorError<W::Error>> {
        let opts = opts.into();
        let batch = Self::create_new_batch(&opts)?;

        Ok(Self {
            opts,
            ctrl_rx,
            batch,
            exporter,
        })
    }

    fn process(&mut self, evt: T) -> Result<(), BatchCollectorError<W::Error>> {
        if !self.batch.has_expiration() {
            self.batch
                .set_expiration(Instant::now() + self.opts.export_time_threshold);
        }

        let size = self.batch.write(evt)?;
        let num_rows = self.batch.num_rows();

        if size >= self.opts.export_size_threshold || num_rows >= self.opts.export_row_threshold {
            self.export()
        } else {
            Ok(())
        }
    }

    fn export(&mut self) -> Result<(), BatchCollectorError<W::Error>> {
        self.batch.flush()?;

        if self.has_data() {
            let next_batch = Self::create_new_batch(&self.opts)?;
            let prev_batch = std::mem::replace(&mut self.batch, next_batch);
            let data = prev_batch.into_buffer()?;
            let exporter = self.exporter.clone();

            // We want to continue processing events while exporting, so spawn a separate
            // task.
            tokio::spawn(async move {
                if let Err(error) = exporter.export(data).await {
                    error!(%error, bug = true, "analytics data export failed");
                }
            });
        }

        Ok(())
    }

    fn expiration(&self) -> Instant {
        self.batch.expiration()
    }

    fn has_data(&self) -> bool {
        self.batch.num_rows() > 0
    }

    fn create_new_batch(
        opts: &BatchCollectorOpts,
    ) -> Result<Batch<T, W>, BatchCollectorError<W::Error>> {
        Batch::new(opts)
    }
}

/// Most of these values should be left on default outside of testing.
#[derive(Debug, Clone)]
pub struct BatchCollectorOpts {
    /// Data collection queue size. Overflowing it will either drop additional
    /// analytics events, or cause an `await` (in async version) until there's
    /// room in the queue.
    pub event_queue_limit: usize,

    /// The amount of time after which current batch will be exported regardless
    /// of how much data it has collected.
    pub export_time_threshold: Duration,

    /// Writer buffer size threshold, going above which will cause data export
    /// and new batch allocation.
    pub export_size_threshold: usize,

    /// The maximum number of events that a single batch can contain. Going
    /// above this limit will cause data export and new batch allocation.
    pub export_row_threshold: usize,

    /// Allocation size for the batch data buffer. Overflowing it will cause
    /// reallocation of data, so it's best to set the export threshold below
    /// this value to never overflow.
    pub batch_alloc_size: usize,

    /// The interval at which the background task will check if current batch
    /// has expired and needs to be exported.
    pub export_check_interval: Duration,
}

impl Default for BatchCollectorOpts {
    fn default() -> Self {
        Self {
            event_queue_limit: 2048,
            export_time_threshold: Duration::from_secs(60 * 5),
            export_size_threshold: 1024 * 1024 * 128,
            export_row_threshold: 1024 * 128,
            batch_alloc_size: 1024 * 1024 * 130,
            export_check_interval: Duration::from_secs(30),
        }
    }
}

pub struct BatchCollector<T: AnalyticsEvent> {
    ctrl_tx: mpsc::Sender<ControlEvent<T>>,
}

impl<T> BatchCollector<T>
where
    T: AnalyticsEvent,
{
    pub fn new<W, E>(
        opts: BatchCollectorOpts,
        exporter: E,
    ) -> Result<Self, BatchCollectorError<W::Error>>
    where
        W: BatchWriter<T>,
        E: BatchExporter,
    {
        let (ctrl_tx, ctrl_rx) = mpsc::channel(opts.event_queue_limit);
        let mut inner = Batcher::<T, E, W>::new(opts.clone(), ctrl_rx, exporter)?;
        let export_check_interval = opts.export_check_interval;

        tokio::spawn(async move {
            let mut export_interval = tokio::time::interval(export_check_interval);
            export_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    ctrl = inner.ctrl_rx.recv() => match ctrl {
                        Some(ControlEvent::Process(data)) => {
                            if let Err(error) = inner.process(data) {
                                error!(
                                    %error,
                                    bug = true,
                                    "analytics collector data processing failed"
                                );
                            }
                        },

                        Some(ControlEvent::Shutdown) => {
                            if let Err(error) = inner.export() {
                                error!(
                                    %error,
                                    bug = true,
                                    trigger = "collector_shutdown",
                                    "analytics collector failed to start data export"
                                );
                            }

                            break;
                        },

                        _ => break,
                    },

                    _ = export_interval.tick() => {
                        if Instant::now() > inner.expiration() {
                            if let Err(error) = inner.export() {
                                error!(
                                    %error,
                                    bug = true,
                                    trigger = "batch_expired",
                                    "analytics collector failed to start data export"
                                );
                            }
                        }
                    }
                };
            }
        });

        Ok(Self { ctrl_tx })
    }
}

impl<T> AnalyticsCollector<T> for BatchCollector<T>
where
    T: AnalyticsEvent,
{
    fn collect(&self, data: T) {
        if let Err(error) = self.ctrl_tx.try_send(ControlEvent::Process(data)) {
            error!(
                %error,
                bug = true,
                "failed to send data collection command to analytics collector"
            );
        }
    }
}

impl<T> Drop for BatchCollector<T>
where
    T: AnalyticsEvent,
{
    fn drop(&mut self) {
        if let Err(error) = self.ctrl_tx.try_send(ControlEvent::Shutdown) {
            error!(
                %error,
                bug = true,
                "failed to send shutdown command to analytics collector"
            );
        }
    }
}
