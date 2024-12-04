use axum::response::Html;

use crate::{database::DB, extract_row, with_db, AppError};

pub async fn list_reservations() -> Result<Html<String>, AppError> {
	let mut html = String::new();

	let results: Vec<_> = with_db!(|db: &mut DB| {
		let tx = db.transaction()?;
		let mut stmt = tx.prepare("SELECT id, time FROM reservations")?;
		let rows = stmt
			.query_map([], extract_row!(usize String))?
			.map(Result::unwrap)
			.collect();
		Ok(rows)
	})?;

	html += "<!DOCTYPE html>";
	html += "<button id='extend'>Extend all to one week</button>";
	html += "<table><thead><td>ID</td><td>time</td><tbody>";
	for (id, time) in results {
		html += &format!("<tr><td>{id}</td><td>{time}</td>");
	}
	html += "</tbody></table>";
	html += "<script>";
	html += "document.getElementById('extend').addEventListener('click', (e) => { fetch('/extend-reservations', { 'method': 'POST' }); });";
	html += "</script>";

	Ok(Html(html))
}
