use std::rc::Rc;

use axum::extract::State;
use octocrab::{
	models::IssueState,
	params::{pulls::Sort, Direction},
};
use rusqlite::{params, params_from_iter};

use crate::{database::DB, with_db, AppError, AppState, TIME_FORMAT};

pub async fn update_prs(State(state): State<AppState>) -> Result<&'static str, AppError> {
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
