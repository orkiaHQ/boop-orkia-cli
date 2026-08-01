//! Ed25519 identity and signing primitives. Key persistence is delegated to a
//! `SecretStore`; plaintext ledger data is never used as a key container.

use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use orkia_model::{Actor, ActorId, OrkiaError, Result};
use orkia_ports::SecretStore;
use rand_core::OsRng;

pub struct Identity {
    actor: Actor,
    signing: SigningKey,
}

impl Identity {
    pub fn generate(display_name: impl Into<String>) -> Self {
        let signing = SigningKey::generate(&mut OsRng);
        let actor = Actor {
            id: ActorId::new(),
            display_name: display_name.into(),
            public_key: STANDARD_NO_PAD.encode(signing.verifying_key().as_bytes()),
        };
        Self { actor, signing }
    }
    pub fn actor(&self) -> &Actor {
        &self.actor
    }
    /// Generates a replacement key while preserving the durable actor ID.
    pub fn successor(&self) -> Self {
        let signing = SigningKey::generate(&mut OsRng);
        Self {
            actor: Actor {
                id: self.actor.id.clone(),
                display_name: self.actor.display_name.clone(),
                public_key: STANDARD_NO_PAD.encode(signing.verifying_key().as_bytes()),
            },
            signing,
        }
    }
    pub fn sign(&self, bytes: &[u8]) -> String {
        STANDARD_NO_PAD.encode(self.signing.sign(bytes).to_bytes())
    }
    pub fn save(&self, store: &dyn SecretStore, key_name: &str) -> Result<()> {
        store.put(key_name, &self.signing.to_bytes())
    }
    pub fn load(store: &dyn SecretStore, key_name: &str, actor: Actor) -> Result<Option<Self>> {
        let Some(bytes) = store.get(key_name)? else {
            return Ok(None);
        };
        let raw: [u8; 32] = bytes
            .try_into()
            .map_err(|_| OrkiaError::Integrity("invalid Ed25519 private key length".into()))?;
        let signing = SigningKey::from_bytes(&raw);
        if STANDARD_NO_PAD.encode(signing.verifying_key().as_bytes()) != actor.public_key {
            return Err(OrkiaError::Integrity(
                "stored key does not match actor public key".into(),
            ));
        }
        Ok(Some(Self { actor, signing }))
    }
}

pub fn verify(public_key: &str, bytes: &[u8], signature: &str) -> Result<()> {
    let key = STANDARD_NO_PAD
        .decode(public_key)
        .map_err(|e| OrkiaError::Integrity(format!("invalid public key: {e}")))?;
    let sig = STANDARD_NO_PAD
        .decode(signature)
        .map_err(|e| OrkiaError::Integrity(format!("invalid signature: {e}")))?;
    let key: [u8; 32] = key
        .try_into()
        .map_err(|_| OrkiaError::Integrity("invalid public key length".into()))?;
    let sig: [u8; 64] = sig
        .try_into()
        .map_err(|_| OrkiaError::Integrity("invalid signature length".into()))?;
    VerifyingKey::from_bytes(&key)
        .map_err(|e| OrkiaError::Integrity(e.to_string()))?
        .verify(bytes, &Signature::from_bytes(&sig))
        .map_err(|e| OrkiaError::Integrity(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signs_and_verifies() {
        let i = Identity::generate("Ada");
        let s = i.sign(b"ledger");
        assert!(verify(&i.actor().public_key, b"ledger", &s).is_ok());
        assert!(verify(&i.actor().public_key, b"other", &s).is_err());
    }
}
