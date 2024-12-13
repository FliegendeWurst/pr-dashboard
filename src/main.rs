use std::any::Any;
use std::cell::RefCell;
use std::env;
use std::error::Error;
use std::net::SocketAddr;
use std::num::ParseIntError;
use std::sync::Arc;
use std::time::SystemTime;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use axum_client_ip::{InsecureClientIp, SecureClientIpSource};
use database::DB;
use octocrab::Octocrab;
use tokio::fs;
use tokio::sync::{Mutex, RwLock};
use tower_http::catch_panic::CatchPanicLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

mod database;
mod route;

use route::*;

pub static TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

pub static AWAITING_AUTHOR: &str = "AwaitingAuthor";
pub static NEEDS_REVIEWER: &str = "NeedsReviewer";
pub static AWAITING_REVIEWER: &str = "AwaitingReviewer";
pub static NEEDS_MERGER: &str = "NeedsMerger";

thread_local! {
	static DATABASE: RefCell<Option<DB>> = RefCell::new(None);
}

#[macro_export]
macro_rules! with_db {
	($code:expr) => {
		crate::DATABASE.with(|db| {
			let mut db = db.borrow_mut();
			if db.is_none() {
				*db = Some(DB::new().unwrap());
			}
			let db: &mut DB = db.as_mut().unwrap();
			let result: Result<_, Box<dyn std::error::Error>> = $code(db);
			result
		})
	};
}

#[macro_export]
macro_rules! extract_row {
	($($t:ty)*) => {
		|_row| {
			let mut _i = 0usize;
			Ok(($(_row.get::<_, $t>({ _i += 1; _i - 1 })?),*))
		}
	};
}

#[tokio::main]
async fn main() {
	real_main().await.unwrap();
}

async fn real_main() -> Result<(), Box<dyn Error>> {
	tracing_subscriber::registry()
		.with(
			tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
				// axum logs rejections from built-in extractors with the `axum::rejection`
				// target, at `TRACE` level. `axum::rejection=trace` enables showing those events
				format!("{}=debug,axum::rejection=trace", env!("CARGO_CRATE_NAME")).into()
			}),
		)
		.with(tracing_subscriber::fmt::layer())
		.init();

	let gh = octocrab::OctocrabBuilder::default();
	let gh = if let Ok(pat) = env::var("GITHUB_PAT") {
		gh.personal_token(pat)
	} else if let Ok(file) = env::var("GITHUB_PAT_FILE") {
		gh.personal_token(fs::read_to_string(file).await?)
	} else {
		panic!("no GITHUB_PAT / GITHUB_PAT_FILE configured");
	}
	.build()?;

	// Categories
	// Awaiting changes
	// New
	// Needs reviewer (= New + ???)
	// Awaiting reviewer (*)
	// Needs merger

	// Routes
	// GET /: main dashboard
	// POST /update-prs: fetch new data from GH
	// POST /reserve-pr: claim PR
	let app = Router::new()
		.route("/", get(root))
		.route("/update-prs", post(update_prs))
		.route("/housekeep-prs", post(housekeep_prs))
		.route("/reserve-pr", post(reserve_pr))
		.route("/list-reservations", get(list_reservations))
		.route("/extend-reservations", post(extend_reservations))
		.layer(middleware::from_fn(log_time))
		.layer(SecureClientIpSource::ConnectInfo.into_extension())
		.layer(CatchPanicLayer::custom(handle_panic))
		.with_state(AppState {
			update_lock: Arc::new(Mutex::new(())),
			gh: Arc::new(RwLock::new(gh)),
		});

	let port = env::var("PORT")
		.map(|x| x.parse::<u16>().expect("invalid port"))
		.unwrap_or(8080);

	let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;
	tracing::info!("listening on {}", listener.local_addr().unwrap());
	axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;

	Ok(())
}

async fn log_time(InsecureClientIp(ip): InsecureClientIp, req: Request, next: Next) -> Response {
	let start = SystemTime::now();

	let method = req.method().to_string();
	let path = req.uri().path().to_owned();

	let res = next.run(req).await;

	let end = SystemTime::now();
	tracing::debug!(
		"{} {} from {}: {} ms elapsed",
		method,
		path,
		ip,
		end.duration_since(start).unwrap().as_millis()
	);

	res
}

#[derive(Clone)]
pub struct AppState {
	pub update_lock: Arc<Mutex<()>>,
	pub gh: Arc<RwLock<Octocrab>>,
}

pub fn construct_sql_filter(filter_query: &str) -> String {
	let mut filter = "".to_owned();
	for label in filter_query.split(';') {
		if let Some(offender) = label
			.chars()
			.find(|x| !x.is_ascii_alphanumeric() && !matches!(x, '.' | ' ' | '-' | '_' | ':' | '/' | '(' | ')'))
		{
			panic!("invalid character in label filter: {offender:?}");
		}
		filter += &format!(r#"AND data LIKE "%{label}%""#);
	}
	filter
}

pub struct AppError {
	inner: Box<dyn Error>,
}

macro_rules! impl_from {
	($type:ty) => {
		impl From<$type> for AppError {
			fn from(value: $type) -> Self {
				Self {
					inner: Box::new(value),
				}
			}
		}
	};
}
impl_from!(octocrab::Error);
impl_from!(serde_json::Error);
impl_from!(ParseIntError);
impl_from!(std::io::Error);

impl From<Box<dyn Error>> for AppError {
	fn from(value: Box<dyn Error>) -> Self {
		Self { inner: value }
	}
}

impl IntoResponse for AppError {
	fn into_response(self) -> Response {
		let msg = format!("{:?}", self.inner);
		(StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
	}
}

fn handle_panic(err: Box<dyn Any + Send + 'static>) -> Response {
	let mut msg = String::new();
	if let Some(s) = err.downcast_ref::<String>() {
		msg += s;
	} else if let Some(s) = err.downcast_ref::<&str>() {
		msg += s;
	} else {
		msg += "Unknown panic message";
	};
	(StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
}
