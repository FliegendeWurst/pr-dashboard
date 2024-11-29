use std::{env, error::Error};

use rusqlite::{Connection, Transaction};

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
