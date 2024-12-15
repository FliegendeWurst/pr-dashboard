use std::collections::HashMap;

use axum::extract::{Query, State};
use axum_client_ip::InsecureClientIp;
use chrono::Local;
use rusqlite::params;

use crate::{
	database::{CommonQueries, DB},
	extract_row, with_db, AppError, AppState, TIME_FORMAT,
};

pub async fn reserve_pr(
	State(state): State<AppState>,
	Query(params): Query<HashMap<String, String>>,
	InsecureClientIp(ip): InsecureClientIp,
) -> Result<String, AppError> {
	let cat = params.get("category").expect("malformed request, requires category");
	let filter = params.get("filter");

	let lock = state.update_lock.lock().await;

	let time = Local::now().naive_local().format(TIME_FORMAT).to_string();

	let result = with_db!(|db: &mut DB| {
		let tx = db.transaction()?;
		let pulls = tx.get_pulls(Some(cat), filter.map(|x| &**x).unwrap_or_default(), true, true, 1)?;
		if pulls.is_empty() {
			return Ok(None);
		}
		let mut query = tx.prepare(&format!(
			"UPDATE pulls
			SET reserved_by = ?1
			WHERE id = ?2
			RETURNING id"
		))?;
		let Some(id) = query
			.query_map(params![format!("{ip}"), pulls[0].number], extract_row!(usize))?
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

	drop(lock);

	if let Some(result) = result {
		Ok(result)
	} else {
		Ok("".to_owned())
	}
}
