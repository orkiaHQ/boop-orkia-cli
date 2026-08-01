//! Rebuildable Postgres projection of the signed Git ledger.

use orkia_model::{event_kind, IndexRecord, LedgerEvent, OrkiaError, Result};
use orkia_ports::ReviewIndex;
use postgres::{Client, NoTls};
use std::sync::Mutex;

pub const MIGRATION: &str = "CREATE TABLE IF NOT EXISTS orkia_ledger_index (event_id TEXT PRIMARY KEY, event_hash TEXT NOT NULL, occurred_at BIGINT NOT NULL, event_kind TEXT NOT NULL, payload TEXT NOT NULL); CREATE INDEX IF NOT EXISTS orkia_ledger_index_kind ON orkia_ledger_index(event_kind);";

pub struct PostgresIndex { client: Mutex<Client> }
impl PostgresIndex { pub fn connect(url: &str) -> Result<Self> { let mut client = Client::connect(url, NoTls).map_err(|e| OrkiaError::External(format!("Postgres: {e}")))?; client.batch_execute(MIGRATION).map_err(|e| OrkiaError::External(format!("Postgres migration: {e}")))?; Ok(Self { client: Mutex::new(client) }) } }
impl ReviewIndex for PostgresIndex {
    fn rebuild(&self, events: &[LedgerEvent]) -> Result<()> { let mut client = self.client.lock().map_err(|_| OrkiaError::External("Postgres lock poisoned".into()))?; let mut transaction = client.transaction().map_err(|e| OrkiaError::External(e.to_string()))?; transaction.execute("DELETE FROM orkia_ledger_index", &[]).map_err(|e| OrkiaError::External(e.to_string()))?; for event in events { let payload = serde_json::to_string(event).map_err(|e| OrkiaError::Invalid(e.to_string()))?; transaction.execute("INSERT INTO orkia_ledger_index (event_id,event_hash,occurred_at,event_kind,payload) VALUES ($1,$2,$3,$4,$5)", &[&event.unsigned.id.0.to_string(), &event.hash, &event.unsigned.occurred_at.unix_timestamp(), &event_kind(&event.unsigned.event), &payload]).map_err(|e| OrkiaError::External(e.to_string()))?; } transaction.commit().map_err(|e| OrkiaError::External(e.to_string()))?; Ok(()) }
    fn search(&self, query: &str) -> Result<Vec<IndexRecord>> { let mut client = self.client.lock().map_err(|_| OrkiaError::External("Postgres lock poisoned".into()))?; let pattern = format!("%{}%", query); client.query("SELECT event_id,event_hash,occurred_at,event_kind FROM orkia_ledger_index WHERE event_kind ILIKE $1 OR payload ILIKE $1 ORDER BY occurred_at", &[&pattern]).map_err(|e| OrkiaError::External(e.to_string()))?.into_iter().map(|row| { Ok(IndexRecord { event_id: orkia_model::EventId(row.get::<_, String>(0).parse().map_err(|e| OrkiaError::Integrity(format!("invalid indexed event id: {e}")))?), event_hash: row.get(1), occurred_at: time::OffsetDateTime::from_unix_timestamp(row.get(2)).map_err(|e| OrkiaError::Integrity(e.to_string()))?, event_kind: row.get(3) }) }).collect() }
}

#[derive(Default)]
pub struct MemoryIndex { events: Mutex<Vec<LedgerEvent>> }
impl ReviewIndex for MemoryIndex { fn rebuild(&self, events: &[LedgerEvent]) -> Result<()> { *self.events.lock().map_err(|_| OrkiaError::External("index lock poisoned".into()))? = events.to_vec(); Ok(()) } fn search(&self, query: &str) -> Result<Vec<IndexRecord>> { Ok(self.events.lock().map_err(|_| OrkiaError::External("index lock poisoned".into()))?.iter().filter(|event| event_kind(&event.unsigned.event).contains(query)).map(|event| IndexRecord { event_id: event.unsigned.id.clone(), event_hash: event.hash.clone(), occurred_at: event.unsigned.occurred_at, event_kind: event_kind(&event.unsigned.event).into() }).collect()) } }
