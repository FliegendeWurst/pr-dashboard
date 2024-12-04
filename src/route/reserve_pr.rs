use std::collections::HashMap;

use axum::extract::{Query, State};
use axum_client_ip::InsecureClientIp;
use chrono::Local;
use rusqlite::{params, params_from_iter};

use crate::{construct_sql_filter, database::DB, extract_row, with_db, AppError, AppState, TIME_FORMAT};

pub async fn reserve_pr(
	State(state): State<AppState>,
	Query(params): Query<HashMap<String, String>>,
	InsecureClientIp(ip): InsecureClientIp,
) -> Result<String, AppError> {
	let cat = params.get("category").expect("malformed request, requires category");
	let filter = params.get("filter");
	let sql_filter = filter.map(|x| construct_sql_filter(x)).unwrap_or_default();

	let lock = state.update_lock.lock().await;

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
  				WHERE reserved_by IS NULL AND category {qual} {sql_filter}
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

	drop(lock);

	if let Some(result) = result {
		Ok(result)
	} else {
		Ok("".to_owned())
	}
}