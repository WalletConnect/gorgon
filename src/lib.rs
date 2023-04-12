use std::sync::Arc;

pub mod collectors;
pub mod exporters;
pub mod geoip;
pub mod time;
pub mod writers;

#[cfg(test)]
mod tests;

pub struct Analytics<T>
where
    T: AnalyticsEvent,
{
    inner: Arc<dyn AnalyticsCollector<T>>,
}

impl<T: AnalyticsEvent> Clone for Analytics<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
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

pub trait AnalyticsEvent: 'static + Send + Sync {}

impl<T> AnalyticsEvent for T where T: 'static + Send + Sync {}

pub trait AnalyticsCollector<T>: Send + Sync
where
    T: AnalyticsEvent,
{
    fn collect(&self, data: T);
}

#[deprecated(
    since = "0.1.2",
    note = "please use `time::now` and `time::format` instead"
)]
pub fn create_timestamp() -> String {
    chrono::Utc::now()
        .format(time::TIMESTAMP_FORMAT)
        .to_string()
}
