use chrono::{Days, Local};
use rusqlite::params;

use crate::{database::DB, extract_row, with_db, AppError, TIME_FORMAT};

pub async fn extend_reservations() -> Result<String, AppError> {
	let mut time = Local::now().naive_local();
	time = time.checked_add_days(Days::new(7)).unwrap();
	let time = time.format(TIME_FORMAT).to_string();
	let rows = with_db!(|db: &mut DB| {
		let tx = db.transaction()?;
		let mut stmt = tx.prepare("UPDATE reservations SET time = ?1")?;
		let rows = stmt
			.query_map(params![time], extract_row!())?
			.map(Result::unwrap)
			.count();
		drop(stmt);
		tx.commit()?;
		Ok(rows)
	})?;
	Ok(format!("updated {rows} rows"))
}
