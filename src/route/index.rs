use std::collections::HashMap;

use axum::{extract::Query, http::StatusCode, response::Html};
use octocrab::models::pulls::PullRequest;

use crate::{
	construct_sql_filter,
	database::{CommonQueries, DB},
	with_db, AppError, AWAITING_AUTHOR, AWAITING_REVIEWER, NEEDS_MERGER, NEEDS_REVIEWER, TIME_FORMAT,
};

static INDEX: &'static str = include_str!("../../index.html");

pub async fn root(Query(params): Query<HashMap<String, String>>) -> Result<(StatusCode, Html<String>), AppError> {
	let filter = params.get("filter");
	let limit = params
		.get("limit")
		.map(|x| x.parse().expect("bad limit parameter"))
		.unwrap_or(50);
	let filter_query = filter.cloned().unwrap_or_default();
	let sql_filter = if let Some(filter_query) = filter {
		construct_sql_filter(filter_query)
	} else {
		"".to_owned()
	};

	let (counts, pulls) = with_db!(|db: &mut DB| {
		let tx = db.transaction()?;

		let mut query = tx.prepare(&format!(
			"SELECT category, COUNT(*) FROM pulls WHERE 1=1 {sql_filter} GROUP BY category"
		))?;
		let counts: Vec<_> = query
			.query_map([], |row| {
				Ok((row.get::<_, Option<String>>(0)?, row.get::<_, usize>(1)?))
			})?
			.map(Result::unwrap)
			.collect();

		let mut rows2 = vec![];
		rows2.extend_from_slice(&tx.get_pulls(None, &filter_query, true, true, limit)?);
		for cat in [AWAITING_AUTHOR, NEEDS_REVIEWER, NEEDS_MERGER] {
			rows2.extend_from_slice(&tx.get_pulls(Some(cat), &filter_query, true, true, limit)?);
		}
		Ok((counts, rows2))
	})?;
	if counts.iter().all(|x| x.1 == 0) {
		return Ok((StatusCode::NOT_FOUND, Html(include_str!("../../404.html").to_owned())));
	}

	let mut prs_author = String::new();
	let mut prs_new = String::new();
	let mut prs_need_review = String::new();
	let mut prs_need_merger = String::new();

	for mut pr in pulls {
		let category = pr.category.clone();
		let data: &mut PullRequest = &mut *pr;
		let last_updated = data.updated_at.unwrap().format(TIME_FORMAT).to_string();
		let title = data.title.as_deref().unwrap();
		let title = askama_escape::escape(title, askama_escape::Html).to_string();
		let date = &last_updated[0..10];
		let id = data.number;

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
			let name = label.name.replace('+', "");
			let href_filter = if let Some(filter) = filter {
				if filter.contains(&name) {
					format!("?filter={filter}")
				} else {
					format!("?filter={filter};{}", name)
				}
			} else {
				format!("?filter={}", name)
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
			<span class="pr-title">{title}</span>
			<br>
			{labels}
			<button class="pr-hide">hide</button>
			</div>"#
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
		.replace(
			"$RESERVE_FILTER",
			&filter.map(|x| format!("&filter={x}")).unwrap_or_default(),
		)
		.replace("$PRS_1", &prs_author)
		.replace("$PRS_2", &prs_new)
		.replace("$PRS_3", &prs_need_review)
		.replace("$PRS_4", &prs_need_merger);

	Ok((StatusCode::OK, Html(index)))
}
