//! Canonical, hash-chained signed provenance ledger.

use orkia_identity::{Identity, verify};
use orkia_model::{
    Actor, CanonicalJson, CaptureEvent, EventId, Hash, LedgerEvent, OrkiaError, RepositoryId,
    Result, UnsignedEvent,
};
use orkia_ports::{Clock, LedgerStore};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub fn canonical_json<T: serde::Serialize>(value: &T) -> Result<CanonicalJson> {
    orkia_model::canonical_json(value)
}

fn legacy_json<T: serde::Serialize>(value: &T) -> Result<CanonicalJson> {
    serde_json::to_vec(value)
        .map_err(|error| OrkiaError::Invalid(format!("cannot serialize legacy JSON: {error}")))
}

pub fn hash(bytes: &[u8]) -> Hash {
    hex::encode(Sha256::digest(bytes))
}

pub fn unsigned_hash(unsigned: &UnsignedEvent) -> Result<Hash> {
    Ok(hash(&canonical_json(unsigned)?))
}

pub struct Ledger<S, C> {
    store: S,
    clock: C,
    repository: RepositoryId,
    identity: Identity,
}

impl<S: LedgerStore, C: Clock> Ledger<S, C> {
    pub fn new(store: S, clock: C, repository: RepositoryId, identity: Identity) -> Self {
        Self {
            store,
            clock,
            repository,
            identity,
        }
    }
    pub fn append(&self, event: CaptureEvent) -> Result<LedgerEvent> {
        let existing = self.store.read_all()?;
        let previous_hash = existing.last().map(|e| e.hash.clone());
        let unsigned = UnsignedEvent {
            id: EventId::new(),
            repository: self.repository.clone(),
            actor: self.identity.actor().id.clone(),
            occurred_at: self.clock.now(),
            previous_hash,
            event,
        };
        let bytes = canonical_json(&unsigned)?;
        let signed = LedgerEvent {
            hash: hash(&bytes),
            signature: self.identity.sign(&bytes),
            unsigned,
        };
        self.store.append(&signed)?;
        Ok(signed)
    }
}

pub fn verify_chain(
    events: &[LedgerEvent],
    actors: &BTreeMap<orkia_model::ActorId, Actor>,
) -> Result<()> {
    let mut previous = None;
    for event in events {
        if event.unsigned.previous_hash != previous {
            return Err(OrkiaError::Integrity(
                "ledger chain is discontinuous".into(),
            ));
        }
        let canonical = canonical_json(&event.unsigned)?;
        let bytes = if hash(&canonical) == event.hash {
            canonical
        } else {
            let legacy = legacy_json(&event.unsigned)?;
            if hash(&legacy) != event.hash {
                return Err(OrkiaError::Integrity(
                    "event hash does not match payload".into(),
                ));
            }
            legacy
        };
        if hash(&bytes) != event.hash {
            return Err(OrkiaError::Integrity(
                "event hash does not match payload".into(),
            ));
        }
        let actor = actors
            .get(&event.unsigned.actor)
            .ok_or_else(|| OrkiaError::Integrity("unknown ledger actor".into()))?;
        verify(&actor.public_key, &bytes, &event.signature)?;
        previous = Some(event.hash.clone());
    }
    Ok(())
}

/// Test and local-contract double; production persistence is `orkia-git`.
#[derive(Clone, Default)]
pub struct MemoryLedgerStore(Arc<Mutex<Vec<LedgerEvent>>>);
impl LedgerStore for MemoryLedgerStore {
    fn append(&self, event: &LedgerEvent) -> Result<()> {
        self.0
            .lock()
            .map_err(|_| OrkiaError::External("ledger lock poisoned".into()))?
            .push(event.clone());
        Ok(())
    }
    fn read_all(&self) -> Result<Vec<LedgerEvent>> {
        Ok(self
            .0
            .lock()
            .map_err(|_| OrkiaError::External("ledger lock poisoned".into()))?
            .clone())
    }
}

pub struct FixedClock(pub time::OffsetDateTime);
impl Clock for FixedClock {
    fn now(&self) -> time::OffsetDateTime {
        self.0
    }
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> time::OffsetDateTime {
        time::OffsetDateTime::now_utc()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_model::CaptureOrigin;
    #[test]
    fn chain_is_signed_and_tamper_evident() {
        let identity = Identity::generate("Ada");
        let actor = identity.actor().clone();
        let store = MemoryLedgerStore::default();
        let ledger = Ledger::new(
            store.clone(),
            FixedClock(time::OffsetDateTime::UNIX_EPOCH),
            RepositoryId::new(),
            identity,
        );
        ledger
            .append(CaptureEvent::SessionStarted {
                session: orkia_model::SessionId::new(),
                origin: CaptureOrigin::Human,
                base_commit: "abc".into(),
                objective: "test".into(),
            })
            .unwrap();
        let mut actors = BTreeMap::new();
        actors.insert(actor.id.clone(), actor);
        assert!(verify_chain(&store.read_all().unwrap(), &actors).is_ok());
    }

    #[test]
    fn legacy_json_events_remain_verifiable_after_canonicalization() {
        let identity = Identity::generate("Ada");
        let actor = identity.actor().clone();
        let unsigned = UnsignedEvent {
            id: EventId::new(),
            repository: RepositoryId::new(),
            actor: actor.id.clone(),
            occurred_at: time::OffsetDateTime::UNIX_EPOCH,
            previous_hash: None,
            event: CaptureEvent::SessionClosed {
                session: orkia_model::SessionId::new(),
            },
        };
        let bytes = legacy_json(&unsigned).unwrap();
        let event = LedgerEvent {
            hash: hash(&bytes),
            signature: identity.sign(&bytes),
            unsigned,
        };
        let actors = BTreeMap::from([(actor.id.clone(), actor)]);
        assert!(verify_chain(&[event], &actors).is_ok());
    }
}
