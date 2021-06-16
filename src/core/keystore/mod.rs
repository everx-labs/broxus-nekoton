use std::any::TypeId;
use std::collections::hash_map::{self, HashMap};
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use ed25519_dalek::PublicKey;
use futures::future;
use serde::{Serialize, Serializer};
use tokio::sync::RwLock;

use crate::crypto::{Signature, Signer, SignerEntry, SignerStorage};
use crate::external::Storage;
use crate::utils::*;

const STORAGE_KEYSTORE: &str = "__core__keystore";

#[repr(C)]
#[allow(missing_debug_implementations)]
pub struct KeyStore {
    state: RwLock<KeyStoreState>,
    storage: Arc<dyn Storage>,
}

impl KeyStore {
    pub fn builder(storage: Arc<dyn Storage>) -> KeyStoreBuilder {
        KeyStoreBuilder {
            storage,
            signers: Default::default(),
            signer_types: Default::default(),
        }
    }

    pub async fn get_entries(&self) -> Vec<KeyStoreEntry> {
        let state = self.state.read().await;
        state
            .entries
            .iter()
            .filter_map(|(public_key, (master_key, account_id, type_id))| {
                let signer_name = state.signers.get(type_id)?.0.clone();

                Some(KeyStoreEntry {
                    signer_name,
                    public_key: PublicKey::from_bytes(public_key).trust_me(),
                    master_key: *master_key,
                    account_id: *account_id,
                })
            })
            .collect()
    }

    pub async fn add_key<T>(&self, input: T::CreateKeyInput) -> Result<KeyStoreEntry>
    where
        T: Signer,
    {
        let mut state = self.state.write().await;

        let (signer_name, signer): (_, &mut T) = state.get_signer_entry::<T>()?;

        let signer_entry = signer.add_key(input).await?;
        state.entries.insert(
            signer_entry.public_key.to_bytes(),
            (
                signer_entry.master_key,
                signer_entry.account_id,
                TypeId::of::<T>(),
            ),
        );

        self.save(&state.signers).await?;
        Ok(KeyStoreEntry::from_signer_entry(signer_name, signer_entry))
    }

    pub async fn update_key<T>(&self, input: T::UpdateKeyInput) -> Result<KeyStoreEntry>
    where
        T: Signer,
    {
        let mut state = self.state.write().await;

        let (signer_name, signer) = state.get_signer_entry::<T>()?;

        let signer_entry = signer.update_key(input).await?;

        self.save(&state.signers).await?;
        Ok(KeyStoreEntry::from_signer_entry(signer_name, signer_entry))
    }

    pub async fn export_key<T>(&self, input: T::ExportKeyInput) -> Result<T::ExportKeyOutput>
    where
        T: Signer,
    {
        let state = self.state.read().await;
        state.get_signer_ref::<T>()?.export_key(input).await
    }

    pub async fn sign<T>(&self, data: &[u8], input: T::SignInput) -> Result<Signature>
    where
        T: Signer,
    {
        let state = self.state.read().await;
        state.get_signer_ref::<T>()?.sign(data, input).await
    }

    pub async fn remove_key(&self, public_key: &PublicKey) -> Result<Option<KeyStoreEntry>> {
        let mut state = self.state.write().await;

        let signer_id = match state.entries.remove(public_key.as_bytes()) {
            Some((_, _, signer_id)) => signer_id,
            None => return Ok(None),
        };

        let (signer_name, signer) = match state.signers.get_mut(&signer_id) {
            Some(entry) => entry,
            None => return Ok(None),
        };

        let entry = signer.remove_key(public_key).await.map(|signer_entry| {
            KeyStoreEntry::from_signer_entry(signer_name.clone(), signer_entry)
        });

        self.save(&state.signers).await?;
        Ok(entry)
    }

    pub async fn clear(&self) -> Result<()> {
        let mut state = self.state.write().await;

        state.entries.clear();
        future::join_all(state.signers.values_mut().map(|(_, signer)| signer.clear())).await;

        self.save(&state.signers).await
    }

    async fn save(&self, signers: &SignersMap) -> Result<()> {
        use serde::ser::SerializeSeq;

        struct StoredData<'a>(&'a SignersMap);

        #[derive(Serialize)]
        struct StoredDataItem<'a>(&'a str, &'a str);

        impl<'a> Serialize for StoredData<'a> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
                for (name, signer) in self.0.values() {
                    seq.serialize_element(&StoredDataItem(name.as_str(), &signer.store_state()))?;
                }
                seq.end()
            }
        }

        let data = serde_json::to_string(&StoredData(signers))?;
        self.storage.set(STORAGE_KEYSTORE, &data).await
    }
}

struct KeyStoreState {
    signers: SignersMap,
    entries: EntriesMap,
}

type SignersMap = HashMap<TypeId, (String, Box<dyn SignerStorage>)>;
type EntriesMap = HashMap<[u8; ed25519_dalek::PUBLIC_KEY_LENGTH], (PublicKey, u16, TypeId)>;

impl KeyStoreState {
    fn get_signer_ref<T>(&self) -> Result<&T>
    where
        T: Signer,
    {
        let signer = self
            .signers
            .get(&TypeId::of::<T>())
            .and_then(|(_, signer)| signer.downcast_ref::<T>())
            .ok_or(KeyStoreError::UnsupportedSigner)?;
        Ok(signer)
    }

    fn get_signer_entry<T>(&mut self) -> Result<(String, &mut T)>
    where
        T: Signer,
    {
        let signer = self
            .signers
            .get_mut(&TypeId::of::<T>())
            .and_then(|(name, signer)| {
                signer
                    .downcast_mut::<T>()
                    .map(|signer| (name.clone(), signer))
            })
            .ok_or(KeyStoreError::UnsupportedSigner)?;
        Ok(signer)
    }
}

impl SignerEntry {
    fn into_plain(
        self,
        type_id: TypeId,
    ) -> (
        [u8; ed25519_dalek::PUBLIC_KEY_LENGTH],
        (PublicKey, u16, TypeId),
    ) {
        (
            self.public_key.to_bytes(),
            (self.master_key, self.account_id, type_id),
        )
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct KeyStoreEntry {
    pub signer_name: String,
    #[serde(with = "crate::utils::serde_public_key")]
    pub public_key: PublicKey,
    #[serde(with = "crate::utils::serde_public_key")]
    pub master_key: PublicKey,
    pub account_id: u16,
}

impl KeyStoreEntry {
    fn from_signer_entry(signer_name: String, signer_entry: SignerEntry) -> Self {
        Self {
            signer_name,
            public_key: signer_entry.public_key,
            master_key: signer_entry.master_key,
            account_id: signer_entry.account_id,
        }
    }
}

#[allow(missing_debug_implementations)]
pub struct KeyStoreBuilder {
    storage: Arc<dyn Storage>,
    signers: HashMap<String, (Box<dyn SignerStorage>, TypeId)>,
    signer_types: HashSet<TypeId>,
}

type BuilderSignersMap = HashMap<String, (Box<dyn SignerStorage>, TypeId)>;

impl KeyStoreBuilder {
    pub fn with_signer<T>(mut self, name: &str, signer: T) -> Result<Self, KeyStoreError>
    where
        T: Signer,
    {
        let type_id = TypeId::of::<T>();
        if !self.signer_types.insert(type_id) {
            return Err(KeyStoreError::DuplicateSignerType);
        }

        match self.signers.entry(name.to_owned()) {
            hash_map::Entry::Vacant(entry) => {
                entry.insert((Box::new(signer), type_id));
            }
            hash_map::Entry::Occupied(_) => return Err(KeyStoreError::DuplicateSignerType),
        }

        Ok(self)
    }

    pub async fn load(mut self) -> Result<KeyStore> {
        let data = self.load_stored_data().await?;

        let mut entries = HashMap::new();

        for (name, data) in data.into_iter() {
            if let Some((storage, type_id)) = self.signers.get_mut(&name) {
                storage.load_state(&data)?;

                entries.extend(
                    storage
                        .get_entries()
                        .into_iter()
                        .map(|entry| entry.into_plain(*type_id)),
                );
            }
        }

        Ok(KeyStore {
            state: RwLock::new(KeyStoreState {
                signers: transpose_signers(self.signers),
                entries,
            }),
            storage: self.storage,
        })
    }

    pub async fn load_unchecked(mut self) -> KeyStore {
        let data = self.load_stored_data().await.unwrap_or_default();

        let mut entries = HashMap::new();

        for (name, data) in data.into_iter() {
            if let Some((storage, type_id)) = self.signers.get_mut(&name) {
                if storage.load_state(&data).is_ok() {
                    entries.extend(
                        storage
                            .get_entries()
                            .into_iter()
                            .map(|entry| entry.into_plain(*type_id)),
                    );
                }
            }
        }

        KeyStore {
            state: RwLock::new(KeyStoreState {
                signers: transpose_signers(self.signers),
                entries,
            }),
            storage: self.storage,
        }
    }

    async fn load_stored_data(&self) -> Result<Vec<(String, String)>> {
        match self.storage.get(STORAGE_KEYSTORE).await? {
            Some(data) => {
                let data = serde_json::from_str(&data)?;
                Ok(data)
            }
            None => Ok(Default::default()),
        }
    }
}

fn transpose_signers(signers: BuilderSignersMap) -> SignersMap {
    signers
        .into_iter()
        .map(|(name, (signer, type_id))| (type_id, (name, signer)))
        .collect()
}

#[derive(thiserror::Error, Debug, Copy, Clone)]
pub enum KeyStoreError {
    #[error("Duplicate signer name")]
    DuplicateSignerName,
    #[error("Duplicate signer type")]
    DuplicateSignerType,
    #[error("Key not found")]
    KeyNotFound,
    #[error("Unsupported signer")]
    UnsupportedSigner,
}
