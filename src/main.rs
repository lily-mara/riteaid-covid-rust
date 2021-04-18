use dashmap::DashMap;
use futures::prelude::*;
use futures::stream::FuturesUnordered;
use opentelemetry::trace::SpanKind;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tonic::metadata::{MetadataMap, MetadataValue};
use tracing::info_span;
use tracing_futures::Instrument;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Registry;
use warp::reject::{self, Rejection};
use warp::Filter;

#[derive(Debug)]
struct Err(anyhow::Error);

impl warp::reject::Reject for Err {}

impl<T> From<T> for Err
where
    T: 'static + std::error::Error + Send + Sync,
{
    fn from(err: T) -> Err {
        Err(err.into())
    }
}

#[derive(Deserialize, Clone)]
struct GetStoresResponse {
    #[serde(rename = "Data")]
    data: GetStoresData,
}

#[derive(Deserialize, Clone)]
struct GetStoresData {
    stores: Vec<Store>,
}

#[derive(Deserialize, Clone)]
struct Store {
    #[serde(rename = "storeNumber")]
    store_number: i32,

    address: String,

    #[serde(rename = "zipcode")]
    zip_code: String,

    #[serde(rename = "fullPhone")]
    phone: String,
}

#[derive(Deserialize)]
struct CheckSlotsResponse {
    #[serde(rename = "Data")]
    data: CheckSlotsData,
}

#[derive(Deserialize)]
struct CheckSlotsData {
    slots: HashMap<String, bool>,
}

#[derive(Serialize)]
struct AvailabilityResponse {
    id: i32,
    address: String,
    possible_availability: bool,
    zip: String,
    phone: String,
}

#[tokio::main]
async fn main() {
    let client = Client::new();

    let store_cache = Arc::new(DashMap::new());

    if let Ok(api_key) = std::env::var("HONEYCOMB_API_KEY") {
        let mut tracing_headers = MetadataMap::new();
        tracing_headers.insert(
            "x-honeycomb-team",
            MetadataValue::from_str(api_key.as_ref()).unwrap(),
        );
        tracing_headers.insert(
            "x-honeycomb-dataset",
            MetadataValue::from_static("riteaid-covid"),
        );

        let tracer = opentelemetry_otlp::new_pipeline()
            .with_endpoint("https://api.honeycomb.io")
            .with_tonic()
            .with_metadata(tracing_headers)
            .with_tls_config(tonic::transport::ClientTlsConfig::new())
            .install_batch(opentelemetry::runtime::Tokio)
            .unwrap();

        let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
        let subscriber = Registry::default()
            .with(telemetry)
            .with(EnvFilter::new("info"));

        tracing::subscriber::set_global_default(subscriber).unwrap();
    }

    let routes = warp::path!("availability" / String)
        .map(move |s| (s, client.clone(), store_cache.clone()))
        .untuple_one()
        .and_then(availability)
        .with(warp::trace(|info| {
            let mut host = "";
            if let Some(h) = info.host() {
                host = h;
            }

            let mut user_agent = "";
            if let Some(h) = info.user_agent() {
                user_agent = h;
            }

            let mut client_ip = Cow::Borrowed("");
            if let Some(ip) = info.remote_addr() {
                client_ip = Cow::Owned(match ip {
                    SocketAddr::V4(s) => format!("{:?}", s.ip()),
                    SocketAddr::V6(s) => format!("{:?}", s.ip()),
                });
            }

            info_span!(
                "http_request",
                http.method = %info.method(),
                http.target = info.path(),
                http.host = host,
                http.user_agent = user_agent,
                otel.kind = %SpanKind::Client,
                http.client_ip = client_ip.as_ref(),
            )
        }));

    warp::serve(routes).run(([127, 0, 0, 1], 3030)).await;
}

async fn list_stores(
    zip_code: String,
    client: Client,
    store_cache: Arc<DashMap<String, GetStoresResponse>>,
) -> Result<GetStoresResponse, Err> {
    if let Some(stores) = store_cache.get(&zip_code) {
        let span = info_span!("list stores", %zip_code, source="cache");
        let _g = span.enter();

        return Ok(stores.clone());
    }

    let response: GetStoresResponse = client
        .get("https://www.riteaid.com/services/ext/v2/stores/getStores")
        .query(&[
            ("address", &zip_code[..]),
            ("attrFilter", "PREF-112"),
            ("fetchMechanismVersion", "2"),
            ("radius", "50"),
        ])
        .send()
        .instrument(info_span!("list stores", %zip_code, source="http"))
        .await?
        .json()
        .await?;

    store_cache.insert(zip_code, response.clone());

    Ok(response)
}

async fn availability(
    zip_code: String,
    client: Client,
    store_cache: Arc<DashMap<String, GetStoresResponse>>,
) -> Result<impl warp::Reply, Rejection> {
    let response = list_stores(zip_code, client.clone(), store_cache)
        .await
        .map_err(|e| reject::custom(e))?;

    let requests = FuturesUnordered::new();
    for store in response.data.stores {
        let client = client.clone();

        requests.push(async move {
            let response: CheckSlotsResponse = client
                .get("https://www.riteaid.com/services/ext/v2/vaccine/checkSlots")
                .query(&[("storeNumber", store.store_number)])
                .send()
                .instrument(info_span!("get store availability"))
                .await
                .map_err(|e| reject::custom(Err::from(e)))?
                .json()
                .await
                .map_err(|e| reject::custom(Err::from(e)))?;

            let possible_availability = *response.data.slots.get("1").unwrap_or(&false)
                && *response.data.slots.get("1").unwrap_or(&false)
                && response.data.slots.len() == 2;

            Ok::<_, Rejection>(AvailabilityResponse {
                id: store.store_number,
                address: store.address,
                possible_availability,
                zip: store.zip_code,
                phone: store.phone,
            })
        });
    }

    let response = requests
        .try_collect::<Vec<_>>()
        .instrument(info_span!("get all store availability"))
        .await?;

    Ok(warp::reply::json(&response))
}
