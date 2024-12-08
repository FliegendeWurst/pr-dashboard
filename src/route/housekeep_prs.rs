use std::rc::Rc;

use axum::extract::State;
use chrono::{Local, NaiveDateTime};
use octocrab::models::pulls::PullRequest;
use rusqlite::params;

use crate::{
	database::DB, extract_row, with_db, AppError, AppState, AWAITING_AUTHOR, NEEDS_MERGER, NEEDS_REVIEWER, TIME_FORMAT,
};

pub async fn housekeep_prs(State(state): State<AppState>) -> Result<&'static str, AppError> {
	let update_lock = state.update_lock.lock().await;

	with_db!(|db: &mut DB| {
		let tx = db.transaction()?;

		let mut query = tx.prepare("SELECT id, data, category FROM pulls")?;
		let pulls: Vec<_> = query
			.query_map([], extract_row!(usize String Option<String>))?
			.map(Result::unwrap)
			.collect();

		for (id, data, category) in pulls {
			let data: PullRequest = serde_json::from_str(&data)?;
			let labels = data.labels.as_deref().unwrap_or_default();
			// 1. Mark new PRs as ready for review if ofborg labeled them!
			let ofborg_evaled = labels.iter().any(|x| x.name.starts_with("10."));
			// 2. Mark PRs based on labels
			let await_author =
				labels.iter().map(|x| &x.name).any(|x| {
					x == "awaiting_changes" || x == "2.status: merge conflict" || x == "2.status: needs-changes"
				}) || data.draft.unwrap_or(false);
			let need_merger = labels.iter().map(|x| &x.name).any(|x| {
				x == "needs_merger"
					|| x == "awaiting_merger"
					|| x == "12.approvals: 3+"
					|| x == "12.approved-by: package-maintainer"
			});
			let need_reviewer = ofborg_evaled;

			if await_author {
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
			} else if need_reviewer {
				if category.as_deref() != Some(NEEDS_REVIEWER) {
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
		}
		drop(query);

		let mut query = tx.prepare("SELECT id, time FROM reservations")?;
		let reservations: Vec<_> = query
			.query_map([], extract_row!(usize String))?
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
