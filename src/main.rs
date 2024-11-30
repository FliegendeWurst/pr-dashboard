use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::net::SocketAddr;
use std::num::ParseIntError;
use std::rc::Rc;
use std::sync::Arc;
use std::time::SystemTime;

use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use axum_client_ip::{InsecureClientIp, SecureClientIpSource};
use chrono::{Local, NaiveDateTime};
use database::DB;
use octocrab::models::pulls::PullRequest;
use octocrab::models::IssueState;
use octocrab::params::pulls::Sort;
use octocrab::params::Direction;
use octocrab::Octocrab;
use rusqlite::{params, params_from_iter};
use tokio::fs;
use tokio::sync::{Mutex, RwLock};
use tower_http::catch_panic::CatchPanicLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

mod database;

pub static TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

static AWAITING_AUTHOR: &str = "AwaitingAuthor";
static NEEDS_REVIEWER: &str = "NeedsReviewer";
static AWAITING_REVIEWER: &str = "AwaitingReviewer";
static NEEDS_MERGER: &str = "NeedsMerger";

thread_local! {
	static DATABASE: RefCell<Option<DB>> = RefCell::new(None);
}

macro_rules! with_db {
	($code:expr) => {
		DATABASE.with(|db| {
			let mut db = db.borrow_mut();
			if db.is_none() {
				*db = Some(DB::new().unwrap());
			}
			let db: &mut DB = db.as_mut().unwrap();
			let result: Result<_, Box<dyn Error>> = $code(db);
			result
		})
	};
}

macro_rules! extract_row {
	($($t:ty)*) => {
		|row| {
			let mut i = 0usize;
			Ok(($(row.get::<_, $t>({ i += 1; i - 1 })?),*))
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
	// POST /move-pr: move PR to new category
	// POST /reserve-pr: claim PR
	let app = Router::new()
		.route("/", get(root))
		.route("/update-prs", post(update_prs))
		.route("/housekeep-prs", post(housekeep_prs))
		.route("/move-pr", post(move_pr))
		.route("/reserve-pr", post(reserve_pr))
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
struct AppState {
	update_lock: Arc<Mutex<()>>,
	gh: Arc<RwLock<Octocrab>>,
}

static INDEX: &'static str = include_str!("../index.html");

async fn root(Query(params): Query<HashMap<String, String>>) -> Result<Html<String>, AppError> {
	let filter = params.get("filter");
	let sql_filter = if let Some(filter_query) = filter {
		let mut filter = "".to_owned();
		for label in filter_query.split(';') {
			if let Some(offender) = label
				.chars()
				.filter(|x| !x.is_ascii_alphanumeric() && *x != '.' && *x != ' ' && *x != '-' && *x != '_' && *x != ':')
				.next()
			{
				panic!("invalid character in label filter: {offender:?}");
			}
			filter += &format!(r#"AND data LIKE "%{label}%""#);
		}
		filter
	} else {
		"".to_owned()
	};

	let (counts, pulls) = with_db!(|db: &mut DB| {
		let tx = db.transaction()?;

		let mut query = tx.prepare(&format!(
			"SELECT category, COUNT(*) FROM pulls WHERE 1=1 {sql_filter} GROUP BY category"
		))?;
		let counts: Vec<_> = query
			.query_map(params![], |row| {
				Ok((row.get::<_, Option<String>>(0)?, row.get::<_, usize>(1)?))
			})?
			.map(Result::unwrap)
			.collect();

		let mut query = tx.prepare(&format!(
			"SELECT id, author, last_updated, data, category
			FROM pulls
			WHERE category IS NULL AND reserved_by IS NULL
			{sql_filter}
			ORDER BY last_updated ASC LIMIT 25"
		))?;
		let rows = query.query_map(params![], |row| {
			Ok((
				row.get::<_, i64>(0)?,
				row.get::<_, String>(1)?,
				row.get::<_, String>(2)?,
				row.get::<_, String>(3)?,
				row.get::<_, Option<String>>(4)?,
			))
		})?;
		let mut rows2 = vec![];
		for row in rows {
			rows2.push(row?);
		}
		for cat in [AWAITING_AUTHOR, NEEDS_REVIEWER, NEEDS_MERGER] {
			let mut query = tx.prepare(&format!(
				"SELECT id, author, last_updated, data, category
				FROM pulls
				WHERE category = ?1 AND reserved_by IS NULL
				{sql_filter}
				ORDER BY last_updated ASC LIMIT 25"
			))?;
			let rows = query.query_map(params![cat], |row| {
				Ok((
					row.get::<_, i64>(0)?,
					row.get::<_, String>(1)?,
					row.get::<_, String>(2)?,
					row.get::<_, String>(3)?,
					row.get::<_, Option<String>>(4)?,
				))
			})?;
			for row in rows {
				rows2.push(row?);
			}
		}
		Ok((counts, rows2))
	})?;

	let mut prs_author = String::new();
	let mut prs_new = String::new();
	let mut prs_need_review = String::new();
	let mut prs_need_merger = String::new();

	for (id, _author, last_updated, data, category) in pulls {
		let mut data: PullRequest = serde_json::from_str(&data)?;
		let title = data.title.as_deref().unwrap();
		let title = askama_escape::escape(title, askama_escape::Html).to_string();
		let date = &last_updated[0..10];

		data.labels.as_mut().map(|x| {
			x.sort_by_key(|x| {
				if let Some((pre, _)) = x.name.split_once('.') {
					if let Ok(number) = pre.parse::<usize>() {
						number
					} else {
						20 + x.name.len()
					}
				} else {
					20 + x.name.len()
				}
			})
		});

		let mut labels = String::new();
		for label in data.labels.as_deref().unwrap_or_default() {
			// white for dark labels
			let rgb_sum = usize::from_str_radix(&label.color[0..2], 16)?
				+ usize::from_str_radix(&label.color[2..4], 16)?
				+ usize::from_str_radix(&label.color[4..6], 16)?;
			let text_color = if rgb_sum > 128 * 3 { "000000" } else { "ffffff" };
			let href_filter = if let Some(filter) = filter {
				format!("?filter={filter};{}", label.name.replace('+', ""))
			} else {
				format!("?filter={}", label.name.replace('+', ""))
			};
			labels += &format!(
				r#"<a href="{href_filter}" class="pr-label" style="background-color: #{}; color: #{}">{}</a>"#,
				label.color,
				text_color,
				askama_escape::escape(&label.name, askama_escape::Html)
			);
		}

		let formatting = format!(
			r#"<div class="pr">
			<span class="pr-header">nixpkgs <a href="https://github.com/NixOS/nixpkgs/pull/{id}">#{id}</a></span>
			<span class="pr-date">{date}</span>
			<br>
			<span class="pr-title">{title}</span><br>{labels}</div>"#
		);
		if category.is_none() {
			prs_new += &formatting;
		} else if category.as_deref() == Some(NEEDS_REVIEWER) {
			prs_need_review += &formatting;
		} else if category.as_deref() == Some(NEEDS_MERGER) {
			prs_need_merger += &formatting;
		} else if category.as_deref() == Some(AWAITING_AUTHOR) {
			prs_author += &formatting;
		}
	}

	let count_awaiting_author = counts
		.iter()
		.filter(|x| x.0.as_deref() == Some(AWAITING_AUTHOR))
		.next()
		.map(|x| x.1)
		.unwrap_or(0);
	let count_null = counts.iter().filter(|x| x.0 == None).next().map(|x| x.1).unwrap_or(0);
	let count_needs_reviewer = counts
		.iter()
		.filter(|x| x.0.as_deref() == Some(NEEDS_REVIEWER))
		.next()
		.map(|x| x.1)
		.unwrap_or(0);
	let _count_awaiting_reviewer = counts
		.iter()
		.filter(|x| x.0.as_deref() == Some(AWAITING_REVIEWER))
		.next()
		.map(|x| x.1)
		.unwrap_or(0);
	let count_needs_merger = counts
		.iter()
		.filter(|x| x.0.as_deref() == Some(NEEDS_MERGER))
		.next()
		.map(|x| x.1)
		.unwrap_or(0);

	let index = INDEX
		.replace("$C1", &count_awaiting_author.to_string())
		.replace("$C2", &count_null.to_string())
		.replace("$C3", &count_needs_reviewer.to_string())
		.replace("$C4", &count_needs_merger.to_string())
		.replace("$PRS_1", &prs_author)
		.replace("$PRS_2", &prs_new)
		.replace("$PRS_3", &prs_need_review)
		.replace("$PRS_4", &prs_need_merger);

	Ok(Html(index))
}

async fn update_prs(State(state): State<AppState>) -> Result<&'static str, AppError> {
	let update_lock = state.update_lock.lock().await;
	let last_update = with_db!(|db: &mut DB| db.last_update())?;
	let gh = state.gh.read().await;

	// if we already have some data, we need to catch and remov eclosed PRs too
	let state = if last_update.is_some() {
		octocrab::params::State::All
	} else {
		octocrab::params::State::Open
	};

	let mut pulls = vec![];
	let mut to_remove = vec![];
	'pages: for page in 1u32.. {
		let prs = gh
			.pulls("NixOS", "nixpkgs")
			.list()
			.sort(Sort::Updated)
			.direction(Direction::Descending)
			.state(state)
			.per_page(100)
			.page(page)
			.send()
			.await?;
		tracing::debug!("update: loading page {page}");
		if prs.items.is_empty() {
			break;
		}
		for pr in prs {
			let id = pr.number as i64;
			let updated_at = pr.updated_at.map(|x| x.format(TIME_FORMAT).to_string());

			if pr.state.as_ref().map(|x| *x == IssueState::Closed).unwrap_or(false) {
				to_remove.push(id);
				continue;
			}

			if updated_at
				.as_ref()
				.map(|x| last_update.as_ref().map(|y| *x < *y).unwrap_or(false))
				.unwrap_or(false)
			{
				tracing::debug!("update: done, PR was updated {updated_at:?}");
				break 'pages; // we are done here!
			}

			let Some(author) = pr.user.as_ref() else {
				tracing::warn!("error during pr update of {id}: has no author");
				continue;
			};
			let author = author.login.clone();
			let data = serde_json::to_string(&pr)?;

			pulls.push(vec![Some(id.to_string()), Some(author), updated_at, Some(data)]);
		}
	}

	let to_delete = Rc::new(
		to_remove
			.into_iter()
			.map(rusqlite::types::Value::from)
			.collect::<Vec<_>>(),
	);
	with_db!(|db: &mut DB| {
		let tx = db.transaction()?;
		for data in pulls {
			let res = tx.execute(
				"INSERT INTO pulls
				(id,author,last_updated,data)
				VALUES (?1,?2,?3,?4) ON CONFLICT DO UPDATE SET
				author = ?2,
				last_updated = ?3,
				data = ?4",
				params_from_iter(data.iter()),
			);
			if let Err(err) = res {
				tracing::warn!("error during pr update: {:?}", err);
			}
		}
		tracing::debug!("update: removing {} closed PRs", to_delete.len());
		let res = tx.execute(
			"DELETE FROM pulls
			WHERE id IN rarray(?1)",
			params![to_delete],
		);
		if let Err(err) = res {
			tracing::warn!("error during pr update: {:?}", err);
		}
		let res = tx.commit();
		if let Err(err) = res {
			tracing::warn!("error during pr update: {:?}", err);
		}
		Ok(())
	})?;

	drop(update_lock);

	Ok("done")
}

async fn housekeep_prs(State(state): State<AppState>) -> Result<&'static str, AppError> {
	let update_lock = state.update_lock.lock().await;

	with_db!(|db: &mut DB| {
		let tx = db.transaction()?;

		let mut query = tx.prepare("SELECT id, data, category FROM pulls")?;
		let pulls: Vec<_> = query
			.query_map(params![], extract_row!(usize String Option<String>))?
			.map(Result::unwrap)
			.collect();

		for (id, data, category) in pulls {
			let data: PullRequest = serde_json::from_str(&data)?;
			let labels = data.labels.as_deref().unwrap_or_default();
			// 1. Mark new PRs as ready for review if ofborg labeled them!
			let ofborg_evaled = labels.iter().any(|x| x.name.starts_with("10."));
			// 2. Mark PRs as waiting for author based on label
			let await_author = labels
				.iter()
				.any(|x| x.name == "awaiting_changes" || x.name == "2.status: merge conflict");
			// 3. Mark PRs as needing merger based on label
			let need_merger = labels.iter().any(|x| x.name == "needs_merger");

			if await_author || data.draft.unwrap_or(false) {
				if category.as_deref() != Some(AWAITING_AUTHOR) {
					let res = tx.execute(
						"UPDATE pulls
						SET category = ?1
						WHERE id = ?2",
						params![AWAITING_AUTHOR, id],
					);
					if let Err(err) = res {
						tracing::warn!("error during pr housekeep: {:?}", err);
					}
				}
			} else if need_merger {
				if category.as_deref() != Some(NEEDS_MERGER) {
					let res = tx.execute(
						"UPDATE pulls
						SET category = ?1
						WHERE id = ?2",
						params![NEEDS_MERGER, id],
					);
					if let Err(err) = res {
						tracing::warn!("error during pr housekeep: {:?}", err);
					}
				}
			} else if category.is_none() && ofborg_evaled {
				let res = tx.execute(
					"UPDATE pulls
					SET category = ?1
					WHERE id = ?2",
					params![NEEDS_REVIEWER, id],
				);
				if let Err(err) = res {
					tracing::warn!("error during pr housekeep: {:?}", err);
				}
			}
		}
		drop(query);

		let mut query = tx.prepare("SELECT id, time FROM reservations")?;
		let reservations: Vec<_> = query
			.query_map(params![], extract_row!(usize String))?
			.map(Result::unwrap)
			.collect();
		let mut pulls_to_unreserve = vec![];

		let now = Local::now().naive_local();
		for (id, time) in reservations {
			let time = NaiveDateTime::parse_from_str(&time, TIME_FORMAT)?;
			if (now - time).num_hours() >= 1 {
				pulls_to_unreserve.push(id);
			}
		}
		drop(query);

		tracing::debug!("housekeep: remove reservations for {pulls_to_unreserve:?}");

		let ids = Rc::new(
			pulls_to_unreserve
				.iter()
				.copied()
				.map(|x| x as i64)
				.map(rusqlite::types::Value::from)
				.collect::<Vec<_>>(),
		);
		let mut query = tx.prepare("DELETE FROM reservations WHERE id IN rarray(?1)")?;
		query.execute(params![ids])?;
		drop(query);
		let mut query = tx.prepare("UPDATE pulls SET reserved_by = NULL WHERE id IN rarray(?1)")?;
		query.execute(params![ids])?;
		drop(query);

		if let Err(err) = tx.commit() {
			tracing::warn!("error during pr housekeep: {err:?}");
		}
		Ok(())
	})?;

	drop(update_lock);

	Ok("done")
}

async fn move_pr() -> &'static str {
	"TODO"
}

async fn reserve_pr(
	Query(params): Query<HashMap<String, String>>,
	InsecureClientIp(ip): InsecureClientIp,
) -> Result<String, AppError> {
	let cat = params.get("category").expect("malformed request, requires category");

	let time = Local::now().naive_local().format(TIME_FORMAT).to_string();

	let result = with_db!(|db: &mut DB| {
		let tx = db.transaction()?;
		let qual = if cat == "New" { "IS NULL" } else { "= ?2" };
		let cat = if cat == "New" { "" } else { cat };
		let mut query = tx.prepare(&format!(
			"UPDATE pulls
			SET reserved_by = ?1
			WHERE rowid = (
  				SELECT rowid
  				FROM pulls
  				WHERE reserved_by IS NULL AND category {qual}
				ORDER BY last_updated ASC
				LIMIT 1
			)
			RETURNING id"
		))?;
		let Some(id) = query
			.query_map(
				if cat != "" {
					params_from_iter(vec![format!("{ip}"), cat.to_owned()].into_iter())
				} else {
					params_from_iter(vec![format!("{ip}")].into_iter())
				},
				extract_row!(usize),
			)?
			.next()
			.map(Result::unwrap)
		else {
			tracing::debug!("no PR to reserve for category {cat}");
			return Ok(None);
		};
		drop(query);

		let mut query = tx.prepare(
			"INSERT INTO reservations
			(id, time)
			VALUES (?1, ?2)
			ON CONFLICT DO UPDATE SET time = ?2",
		)?;
		let _ = query.query_map(params![id, time], |_row| Ok(()))?.count();
		drop(query);

		if let Err(e) = tx.commit() {
			tracing::warn!("error in PR reserve: {e:?}");
		}

		Ok(Some(format!("https://github.com/NixOS/nixpkgs/pull/{id}")))
	})?;

	if let Some(result) = result {
		Ok(result)
	} else {
		Ok("".to_owned())
	}
}

struct AppError {
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
