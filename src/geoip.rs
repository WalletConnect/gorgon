use {
    aws_sdk_s3::Client as S3Client,
    bytes::Bytes,
    std::{net::IpAddr, sync::Arc},
};

#[derive(Default, Debug, Clone)]
pub struct GeoIpReader {
    reader: Option<Arc<maxminddb::Reader<Bytes>>>,
}

#[derive(Debug, Clone)]
pub struct AnalyticsGeoData {
    pub country: Option<Arc<str>>,
    pub continent: Option<Arc<str>>,
}

impl GeoIpReader {
    pub fn empty() -> Self {
        Self::default()
    }

    pub async fn from_aws_s3(
        s3_client: &S3Client,
        bucket: impl Into<String>,
        key: impl Into<String>,
    ) -> anyhow::Result<Self> {
        load_s3_object(s3_client, bucket, key)
            .await
            .map(GeoIpReader::from_buffer)
    }

    pub fn from_buffer(buffer: Bytes) -> Self {
        maxminddb::Reader::from_source(buffer)
            .map(|reader| Self {
                reader: Some(Arc::new(reader)),
            })
            .unwrap_or_else(|err| panic!("failed to read geoip database: {err}"))
    }

    pub fn lookup_geo_data(&self, addr: IpAddr) -> Option<AnalyticsGeoData> {
        use maxminddb::geoip2::Country;

        self.reader
            .as_ref()?
            .lookup::<Country>(addr)
            .ok()
            .map(|country| AnalyticsGeoData {
                country: country
                    .country
                    .and_then(|country| country.iso_code.map(Into::into)),
                continent: country
                    .continent
                    .and_then(|continent| continent.code.map(Into::into)),
            })
    }
}

async fn load_s3_object(
    s3_client: &S3Client,
    bucket: impl Into<String>,
    key: impl Into<String>,
) -> Result<Bytes, anyhow::Error> {
    let bytes = s3_client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await?
        .body
        .collect()
        .await?
        .into_bytes();

    Ok(bytes)
}
