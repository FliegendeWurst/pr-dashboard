use std::{
	cmp::Reverse,
	env,
	error::Error,
	ops::{Deref, DerefMut},
};

use chrono::Utc;
use octocrab::models::pulls::PullRequest;
use rusqlite::{params, Connection, Transaction};

use crate::{construct_sql_filter, extract_row, NEEDS_MERGER};

pub struct DB {
	db: Connection,
}

impl DB {
	pub fn new() -> Result<Self, Box<dyn Error>> {
		let db =
			Connection::open(env::var("PR_DASHBOARD_DATABASE").unwrap_or_else(|_err| "./pr-dashboard.db".to_owned()))?;
		rusqlite::vtab::array::load_module(&db)?;

		db.execute(
			"CREATE TABLE IF NOT EXISTS pulls(
            id INTEGER NOT NULL PRIMARY KEY,
            author TEXT NOT NULL,
            last_updated TEXT NOT NULL,
            data TEXT NOT NULL,
            category TEXT,
            reserved_by TEXT
        ) STRICT",
			[],
		)?;

		db.execute(
			"CREATE TABLE IF NOT EXISTS reservations(
            id INTEGER NOT NULL PRIMARY KEY,
            time TEXT NOT NULL
        ) STRICT",
			[],
		)?;

		Ok(Self { db })
	}

	pub fn last_update(&self) -> Result<Option<String>, Box<dyn Error>> {
		Ok(self.db.query_row("SELECT MAX(last_updated) FROM pulls", [], |row| {
			row.get::<_, Option<String>>(0)
		})?)
	}

	pub fn transaction(&mut self) -> Result<Transaction, Box<dyn Error>> {
		Ok(self.db.transaction()?)
	}
}

#[derive(Clone)]
pub struct PR {
	inner: PullRequest,
	pub category: Option<String>,
}

impl Deref for PR {
	type Target = PullRequest;

	fn deref(&self) -> &Self::Target {
		&self.inner
	}
}

impl DerefMut for PR {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.inner
	}
}

pub trait CommonQueries {
	fn get_pulls(
		&self,
		category: Option<&str>,
		filter_query: &str,
		exclude: &str,
		only_not_reserved: bool,
		tweak_sort: bool,
		limit: u64,
	) -> Result<Vec<PR>, Box<dyn Error>>;
}

impl<'conn> CommonQueries for Transaction<'conn> {
	fn get_pulls(
		&self,
		category: Option<&str>,
		filter_query: &str,
		exclude: &str,
		only_not_reserved: bool,
		mut tweak_sort: bool,
		limit: u64,
	) -> Result<Vec<PR>, Box<dyn Error>> {
		if category == Some(NEEDS_MERGER) {
			tweak_sort = false;
		}
		let sql_filter = construct_sql_filter(filter_query, exclude);
		let reserved_filter = if only_not_reserved {
			"AND reserved_by IS NULL"
		} else {
			""
		};
		let new = category.map(|x| x == "New").unwrap_or(true);
		let qual = if new { "IS NULL" } else { "= ?1" };
		let cat = if new { "" } else { category.as_ref().unwrap() };

		let mut query = self.prepare(&format!(
			"SELECT data, category
			FROM pulls
			WHERE
			category {qual}
			{sql_filter}
			{reserved_filter}
			ORDER BY last_updated ASC LIMIT {limit}"
		))?;
		println!("query = {query:?}");
		let params = if cat != "" { params![cat] } else { params![] };
		let rows = query.query_map(params, extract_row!(String Option<String>))?;
		let mut prs: Vec<PR> = vec![];
		for data in rows {
			let data = data?;
			let pr = data.0;
			let cat = data.1;
			prs.push(PR {
				inner: serde_json::from_str(&pr)?,
				category: cat,
			});
		}
		if tweak_sort {
			// sort by: number of approvals, last updated time
			let now = Utc::now();
			prs.sort_unstable_by_key(|x| {
				let mut approvals = 0;
				let labels = x.labels.as_deref().unwrap_or(&[]);
				if labels.iter().any(|x| x.name == "12.approvals: 1") {
					approvals = 1;
				}
				if labels.iter().any(|x| x.name == "12.approvals: 2") {
					approvals = 2;
				}
				if labels.iter().any(|x| x.name == "12.approvals: 3+") {
					approvals = 3;
				}
				if labels.iter().any(|x| x.name == "12.approved-by: package-maintainer") {
					approvals += 1;
				}
				(approvals, Reverse(now - x.updated_at.unwrap()))
			});
		}
		Ok(prs)
	}
}
