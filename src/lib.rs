pub use noop::*;
use {serde::Serialize, std::sync::Arc};

pub mod batcher;
pub mod geoip;
mod noop;
pub mod time;

#[derive(Clone)]
pub struct Analytics<T>
where
    T: AnalyticsEvent,
{
    inner: Arc<dyn AnalyticsCollector<T>>,
}

impl<T> Analytics<T>
where
    T: AnalyticsEvent,
{
    pub fn new(collector: impl AnalyticsCollector<T> + 'static) -> Self {
        Self {
            inner: Arc::new(collector),
        }
    }

    pub fn collect(&self, data: T) {
        self.inner.collect(data);
    }
}

pub trait AnalyticsEvent: 'static + Serialize + Send + Sync {}

impl<T> AnalyticsEvent for T where T: 'static + Serialize + Send + Sync {}

pub trait AnalyticsCollector<T>: Send + Sync
where
    T: AnalyticsEvent,
{
    fn collect(&self, data: T);
}
