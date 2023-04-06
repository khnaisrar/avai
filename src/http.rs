//! HTTP server for confidence and data retrieval.
//!
//! # Endpoints
//!
//! * `GET /v1/mode` - returns client mode (light or light+app client)
//! * `GET /v1/status` - returns status of a latest processed block
//! * `GET /v1/latest_block` - returns latest processed block
//! * `GET /v1/confidence/{block_number}` - returns calculated confidence for a given block number
//! * `GET /v1/appdata/{block_number}` - returns decoded extrinsic data for configured app_id and given block number
//! * `POST /v1/appdata` - submits app data to avail
use std::{
	convert::Infallible,
	net::SocketAddr,
	str::FromStr,
	sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use avail_subxt::{
	api::runtime_types::{da_control::pallet::Call, da_runtime::RuntimeCall},
	primitives::AppUncheckedExtrinsic,
};
use base64::{engine::general_purpose, Engine as _};
use codec::Decode;
use kate_recovery::com::AppData;
use num::{BigUint, FromPrimitive};
use rand::{thread_rng, Rng};
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, info};
use warp::{http::StatusCode, Filter};

use crate::{
	custom::{self, CustomClient},
	data::{get_confidence_from_db, get_decoded_data_from_db},
	types::{Mode, RuntimeConfig},
};

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ConfidenceResponse {
	pub block: u32,
	pub confidence: f64,
	pub serialised_confidence: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
enum Extrinsics {
	Encoded(Vec<AppUncheckedExtrinsic>),
	Decoded(Vec<String>),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ExtrinsicsDataResponse {
	block: u32,
	extrinsics: Extrinsics,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct LatestBlockResponse {
	pub latest_block: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Status {
	pub block_num: u32,
	confidence: f64,
	pub app_id: Option<u32>,
}

/// Calculates confidence from given number of verified cells
pub fn calculate_confidence(count: u32) -> f64 {
	100f64 * (1f64 - 1f64 / 2u32.pow(count) as f64)
}

fn serialised_confidence(block: u32, factor: f64) -> Option<String> {
	let block_big: BigUint = FromPrimitive::from_u64(block as u64)?;
	let factor_big: BigUint = FromPrimitive::from_u64((10f64.powi(7) * factor) as u64)?;
	let shifted: BigUint = block_big << 32 | factor_big;
	Some(shifted.to_str_radix(10))
}

#[derive(Debug)]
enum ClientResponse<T>
where
	T: Serialize,
{
	Normal(T),
	// BadRequest(T),
	NotFound,
	NotFinalized,
	InProcess,
	Error(anyhow::Error),
}

fn confidence(block_num: u32, db: Arc<DB>, counter: u32) -> ClientResponse<ConfidenceResponse> {
	info!("Got request for confidence for block {block_num}");
	let res = match get_confidence_from_db(db, block_num) {
		Ok(Some(count)) => {
			let confidence = calculate_confidence(count);
			let serialised_confidence = serialised_confidence(block_num, confidence);
			ClientResponse::Normal(ConfidenceResponse {
				block: block_num,
				confidence,
				serialised_confidence,
			})
		},
		Ok(None) => {
			// let lock = counter.lock().unwrap();
			if block_num < counter {
				return ClientResponse::NotFinalized;
			} else {
				return ClientResponse::NotFound;
			}
		},
		Err(e) => ClientResponse::Error(e),
	};
	info!("Returning confidence: {res:?}");
	res
}

fn status(cfg: &RuntimeConfig, counter: u32, db: Arc<DB>) -> ClientResponse<Status> {
	let res = match get_confidence_from_db(db, counter) {
		Ok(Some(count)) => {
			let confidence = calculate_confidence(count);
			ClientResponse::Normal(Status {
				block_num: counter,
				confidence,
				app_id: cfg.app_id,
			})
		},
		Ok(None) => ClientResponse::NotFound,

		Err(e) => ClientResponse::Error(e),
	};
	info!("Returning status: {res:?}");
	res
}

fn latest_block(counter: Arc<Mutex<u32>>) -> ClientResponse<LatestBlockResponse> {
	info!("Got request for latest block");
	let res = match counter.lock() {
		Ok(counter) => ClientResponse::Normal(LatestBlockResponse {
			latest_block: *counter,
		}),
		Err(_) => ClientResponse::NotFound,
	};
	res
}

fn appdata(
	block_num: u32,
	db: Arc<DB>,
	cfg: RuntimeConfig,
	counter: u32,
	decode: bool,
) -> ClientResponse<ExtrinsicsDataResponse> {
	fn decode_app_data_to_extrinsics(
		data: Result<Option<AppData>>,
	) -> Result<Option<Vec<AppUncheckedExtrinsic>>> {
		let xts = data.map(|e| {
			e.map(|e| {
				e.iter()
					.enumerate()
					.map(|(i, raw)| {
						<_ as Decode>::decode(&mut &raw[..])
							.context(format!("Couldn't decode AvailExtrinsic num {i}"))
					})
					.collect::<Result<Vec<_>>>()
			})
		});
		match xts {
			Ok(Some(Ok(s))) => Ok(Some(s)),
			Ok(Some(Err(e))) => Err(e),
			Ok(None) => Ok(None),
			Err(e) => Err(e),
		}
	}
	info!("Got request for AppData for block {block_num}");
	let res = match decode_app_data_to_extrinsics(get_decoded_data_from_db(
		db,
		cfg.app_id.unwrap_or(0u32),
		block_num,
	)) {
		Ok(Some(data)) => {
			if !decode {
				ClientResponse::Normal(ExtrinsicsDataResponse {
					block: block_num,
					extrinsics: Extrinsics::Encoded(data),
				})
			} else {
				let xts = data
					.iter()
					.flat_map(|xt| match &xt.function {
						RuntimeCall::DataAvailability(Call::submit_data { data, .. }) => Some(data),
						_ => None,
					})
					.map(|data| general_purpose::STANDARD.encode(data.0.as_slice()))
					.collect::<Vec<_>>();
				ClientResponse::Normal(ExtrinsicsDataResponse {
					block: block_num,
					extrinsics: Extrinsics::Decoded(xts),
				})
			}
		},

		Ok(None) => match counter {
			lock if block_num == lock => ClientResponse::InProcess,
			_ => ClientResponse::NotFound,
		},
		Err(e) => ClientResponse::Error(e),
	};
	debug!("Returning AppData: {res:?}");
	res
}

async fn custom_get_state(
	custom_client: Arc<AsyncMutex<CustomClient>>,
) -> Result<ClientResponse<custom::Balances>, Infallible> {
	let mut custom_client = custom_client.lock().await;
	let balances: custom::Balances = custom_client.query_state().await.unwrap();
	Ok(ClientResponse::Normal(balances))
}

async fn custom_post_appdata(
	custom_client: Arc<AsyncMutex<CustomClient>>,
	state: Arc<AsyncMutex<Vec<custom::types::Transfer>>>,
	value: serde_json::Value,
) -> Result<ClientResponse<custom::PostAppData>, Infallible> {
	let transfer: custom::types::Transfer = serde_json::from_value(value.clone()).unwrap();

	let mut custom_client = custom_client.lock().await;

	// TODO: Simulate should return "Not enough balance"
	// let balances: custom::Balances = custom_client.query_state().await.unwrap();
	// if let Some(balance) = balances.balances.iter().find(|b| b.0 == transfer.from) {
	// 	if usize::from_str(&balance.1).unwrap() < usize::from_str(&transfer.amount).unwrap() {
	// 		return Ok(ClientResponse::BadRequest(custom::PostAppData::Error(
	// 			"Not enough balance".to_string(),
	// 		)));
	// 	}
	// }

	let mut state = state.lock().await;
	state.push(transfer);
	let transfers = state.clone().into_iter().collect();
	let balances = custom_client.simulate(transfers).await.unwrap();

	// TODO: Add block height to the response

	Ok(ClientResponse::Normal(custom::PostAppData::Balances(
		balances,
	)))
}

// async fn post_appdata(
// 	app_id: Option<u32>,
// 	client: Arc<OnlineClient<AvailConfig>>,
// 	value: serde_json::Value,
// ) -> Result<ClientResponse<serde_json::Value>, Infallible> {
// 	let Some(app_id) = app_id else {
// 	    return Ok(ClientResponse::Normal("Application is not configured".into()));
// 	};
// 	let signer = PairSigner::new(AccountKeyring::Alice.pair());
// 	let data = value.to_string().into_bytes();
// 	let data_transfer = api::tx().data_availability().submit_data(BoundedVec(data));
// 	let extrinsic_params = AvailExtrinsicParams::new_with_app_id(app_id.into());

// 	client
// 		.tx()
// 		.sign_and_submit(&data_transfer, &signer, extrinsic_params)
// 		.await
// 		.unwrap();

// 	Ok(ClientResponse::Normal(value))
// }

impl<T: Send + Serialize> warp::Reply for ClientResponse<T> {
	fn into_response(self) -> warp::reply::Response {
		match self {
			ClientResponse::Normal(response) => {
				warp::reply::with_status(warp::reply::json(&response), StatusCode::OK)
					.into_response()
			},
			// ClientResponse::BadRequest(response) => {
			// 	warp::reply::with_status(warp::reply::json(&response), StatusCode::BAD_REQUEST)
			// 		.into_response()
			// },
			ClientResponse::NotFound => {
				warp::reply::with_status(warp::reply::json(&"Not found"), StatusCode::NOT_FOUND)
					.into_response()
			},
			ClientResponse::NotFinalized => warp::reply::with_status(
				warp::reply::json(&"Not synced".to_owned()),
				StatusCode::BAD_REQUEST,
			)
			.into_response(),
			ClientResponse::InProcess => warp::reply::with_status(
				warp::reply::json(&"Processing block".to_owned()),
				StatusCode::UNAUTHORIZED,
			)
			.into_response(),
			ClientResponse::Error(e) => warp::reply::with_status(
				warp::reply::json(&e.to_string()),
				StatusCode::INTERNAL_SERVER_ERROR,
			)
			.into_response(),
		}
	}
}

#[derive(Deserialize, Serialize)]
struct AppDataQuery {
	decode: Option<bool>,
}

/// Runs HTTP server
pub async fn run_server(
	store: Arc<DB>,
	cfg: RuntimeConfig,
	counter: Arc<Mutex<u32>>,
	custom_client: Arc<AsyncMutex<CustomClient>>,
	state: Arc<AsyncMutex<Vec<custom::types::Transfer>>>,
) {
	let host = cfg.http_server_host.clone();
	let port = if cfg.http_server_port.1 > 0 {
		let port: u16 = thread_rng().gen_range(cfg.http_server_port.0..=cfg.http_server_port.1);
		info!("Using random http server port: {port}");
		port
	} else {
		cfg.http_server_port.0
	};
	let app_id = cfg.app_id;

	let get_mode = warp::path!("v1" / "mode").map(move || warp::reply::json(&Mode::from(app_id)));

	let counter_clone = counter.clone();
	let get_latest_block =
		warp::path!("v1" / "latest_block").map(move || latest_block(counter_clone.clone()));

	let counter_confidence = counter.clone();
	let db = store.clone();
	let get_confidence = warp::path!("v1" / "confidence" / u32).map(move |block_num| {
		let counter_lock = counter_confidence.lock().unwrap();
		confidence(block_num, db.clone(), *counter_lock)
	});

	let db = store.clone();
	let cfg1 = cfg.clone();
	let counter_appdata = counter.clone();
	let get_appdata = (warp::path!("v1" / "appdata" / u32))
		.and(warp::query::<AppDataQuery>())
		.map(move |block_num, query: AppDataQuery| {
			let counter_lock = counter_appdata.lock().unwrap();
			appdata(
				block_num,
				db.clone(),
				cfg1.clone(),
				*counter_lock,
				query.decode.unwrap_or(false),
			)
		});

	let cfg = cfg.clone();

	let db = store.clone();
	let counter_status = counter.clone();
	let get_status = warp::path!("v1" / "status").map(move || {
		let counter_lock = counter_status.lock().unwrap();
		status(&cfg, *counter_lock, db.clone())
	});

	let post_appdata_custom_client = custom_client.clone();
	let post_appdata = warp::path!("v1" / "appdata")
		.and(warp::body::json::<serde_json::Value>())
		.and_then(move |value| {
			let custom_client = post_appdata_custom_client.clone();
			let state = state.clone();
			async move { custom_post_appdata(custom_client, state, value).await }
		});

	let get_custom_state_custom_client = custom_client.clone();
	let get_custom_state = warp::path!("v1" / "custom" / "state").and_then(move || {
		let custom_client = get_custom_state_custom_client.clone();
		async move { custom_get_state(custom_client).await }
	});

	let cors = warp::cors()
		.allow_any_origin()
		.allow_header("content-type")
		.allow_methods(vec!["GET", "POST", "DELETE"]);

	let routes = warp::get()
		.and(
			get_mode
				.or(get_latest_block)
				.or(get_confidence)
				.or(get_appdata)
				.or(get_status)
				.or(get_custom_state),
		)
		.or(warp::post().and(post_appdata))
		.with(cors);

	let addr = SocketAddr::from_str(format!("{host}:{port}").as_str())
		.context("Unable to parse host address from config")
		.unwrap();
	info!("RPC running on http://{host}:{port}");
	warp::serve(routes).run(addr).await;
}
