//! The collection type used by iroh
use std::collections::BTreeMap;

use anyhow::Context;
use bao_tree::blake3;
use bytes::Bytes;
use iroh_bytes::get::fsm::EndBlobNext;
use iroh_bytes::get::Stats;
use iroh_bytes::hashseq::HashSeq;
use iroh_bytes::store::MapEntry;
use iroh_bytes::util::TempTag;
use iroh_bytes::{BlobFormat, Hash};
use iroh_io::AsyncSliceReaderExt;
use serde::{Deserialize, Serialize};

/// A collection of blobs
///
/// Note that the format is subject to change.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Collection {
    /// Links to the blobs in this collection
    blobs: Vec<(String, Hash)>,
}

impl std::ops::Index<usize> for Collection {
    type Output = (String, Hash);

    fn index(&self, index: usize) -> &Self::Output {
        &self.blobs[index]
    }
}

impl Default for Collection {
    fn default() -> Self {
        Self { blobs: Vec::new() }
    }
}

impl<K, V> Extend<(K, V)> for Collection
where
    K: Into<String>,
    V: Into<Hash>,
{
    fn extend<T: IntoIterator<Item = (K, V)>>(&mut self, iter: T) {
        self.blobs
            .extend(iter.into_iter().map(|(k, v)| (k.into(), v.into())));
    }
}

impl<K, V> FromIterator<(K, V)> for Collection
where
    K: Into<String>,
    V: Into<Hash>,
{
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        let mut res = Self::default();
        res.extend(iter);
        res
    }
}

/// Metadata for a collection
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
struct CollectionMeta {
    names: Vec<String>,
}

impl Collection {
    /// Convert the collection to an iterator of blobs, with the last being the
    /// root blob.
    ///
    /// To persist the collection, write all the blobs to storage, and use the
    /// hash of the last blob as the collection hash.
    pub fn to_blobs(&self) -> impl Iterator<Item = Bytes> {
        let meta = CollectionMeta {
            names: self.names(),
        };
        let meta_bytes = postcard::to_stdvec(&meta).unwrap();
        let meta_bytes_hash = blake3::hash(&meta_bytes).into();
        let links = std::iter::once(meta_bytes_hash)
            .chain(self.links())
            .collect::<HashSeq>();
        let links_bytes = links.into_inner();
        [meta_bytes.into(), links_bytes].into_iter()
    }

    /// Read the collection from a get fsm.
    ///
    /// Returns the fsm at the start of the first child blob (if any),
    /// the links array, and the collection.
    pub async fn read_fsm(
        fsm_at_start_root: iroh_bytes::get::fsm::AtStartRoot,
    ) -> anyhow::Result<(iroh_bytes::get::fsm::EndBlobNext, HashSeq, Collection)> {
        let (next, links) = {
            let curr = fsm_at_start_root.next();
            let (curr, data) = curr.concatenate_into_vec().await?;
            let links = HashSeq::new(data.into()).context("links could not be parsed")?;
            (curr.next(), links)
        };
        let EndBlobNext::MoreChildren(at_meta) = next else {
            anyhow::bail!("expected meta");
        };
        let (next, collection) = {
            let mut children = links.clone();
            let meta_link = children.pop_front().context("meta link not found")?;
            let curr = at_meta.next(meta_link);
            let (curr, names) = curr.concatenate_into_vec().await?;
            let names = postcard::from_bytes::<CollectionMeta>(&names)?;
            let collection = Collection::from_parts(children, names);
            (curr.next(), collection)
        };
        Ok((next, links, collection))
    }

    /// Read the collection and all it's children from a get fsm.
    ///
    /// Returns the collection, a map from blob offsets to bytes, and the stats.
    pub async fn read_fsm_all(
        fsm_at_start_root: iroh_bytes::get::fsm::AtStartRoot,
    ) -> anyhow::Result<(Collection, BTreeMap<u64, Bytes>, Stats)> {
        let (next, links, collection) = Self::read_fsm(fsm_at_start_root).await?;
        let mut res = BTreeMap::new();
        let mut curr = next;
        let end = loop {
            match curr {
                EndBlobNext::MoreChildren(more) => {
                    let child_offset = more.child_offset();
                    let Some(hash) = links.get(usize::try_from(child_offset)?) else {
                        break more.finish();
                    };
                    let header = more.next(hash);
                    let (next, blob) = header.concatenate_into_vec().await?;
                    res.insert(child_offset - 1, blob.into());
                    curr = next.next();
                }
                EndBlobNext::Closing(closing) => break closing,
            }
        };
        let stats = end.next().await?;
        Ok((collection, res, stats))
    }

    /// Load a collection from a store given a root hash
    ///
    /// This assumes that both the links and the metadata of the collection is stored in the store.
    /// It does not require that all child blobs are stored in the store.
    pub async fn load<D>(db: &D, root: &Hash) -> anyhow::Result<Self>
    where
        D: iroh_bytes::store::Map,
    {
        let links_entry = db.get(root).context("links not found")?;
        anyhow::ensure!(links_entry.is_complete(), "links not complete");
        let links_bytes = links_entry.data_reader().await?.read_to_end().await?;
        let mut links = HashSeq::try_from(links_bytes)?;
        let meta_hash = links.pop_front().context("meta hash not found")?;
        let meta_entry = db.get(&meta_hash).context("meta not found")?;
        anyhow::ensure!(links_entry.is_complete(), "links not complete");
        let meta_bytes = meta_entry.data_reader().await?.read_to_end().await?;
        let meta: CollectionMeta = postcard::from_bytes(&meta_bytes)?;
        anyhow::ensure!(
            meta.names.len() == links.len(),
            "names and links length mismatch"
        );
        Ok(Self::from_parts(links, meta))
    }

    /// Store a collection in a store. returns the root hash of the collection
    /// as a TempTag.
    pub async fn store<D>(self, db: &D) -> anyhow::Result<TempTag>
    where
        D: iroh_bytes::store::Store,
    {
        let (links, meta) = self.into_parts();
        let meta_bytes = postcard::to_stdvec(&meta)?;
        let meta_tag = db.import_bytes(meta_bytes.into(), BlobFormat::Raw).await?;
        let links_bytes = std::iter::once(*meta_tag.hash())
            .chain(links)
            .collect::<HashSeq>();
        let links_tag = db
            .import_bytes(links_bytes.into(), BlobFormat::HashSeq)
            .await?;
        Ok(links_tag)
    }

    /// Split a collection into a sequence of links and metadata
    fn into_parts(self) -> (Vec<Hash>, CollectionMeta) {
        let mut names = Vec::with_capacity(self.blobs.len());
        let mut links = Vec::with_capacity(self.blobs.len());
        for (name, hash) in self.blobs {
            names.push(name);
            links.push(hash);
        }
        let meta = CollectionMeta { names };
        (links, meta)
    }

    /// Create a new collection from a list of hashes and metadata
    fn from_parts(links: impl IntoIterator<Item = Hash>, meta: CollectionMeta) -> Self {
        meta.names.into_iter().zip(links).collect()
    }

    /// Get the links to the blobs in this collection
    fn links(&self) -> impl Iterator<Item = Hash> + '_ {
        self.blobs.iter().map(|(_name, hash)| *hash)
    }

    /// Get the names of the blobs in this collection
    fn names(&self) -> Vec<String> {
        self.blobs.iter().map(|(name, _)| name.clone()).collect()
    }

    /// Take ownership of the blobs in this collection
    pub fn into_iter(self) -> impl Iterator<Item = (String, Hash)> {
        self.blobs.into_iter()
    }

    /// Iterate over the blobs in this collection
    pub fn iter(&self) -> impl Iterator<Item = &(String, Hash)> {
        self.blobs.iter()
    }

    /// Get the number of blobs in this collection
    pub fn len(&self) -> usize {
        self.blobs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bao_tree::blake3;

    #[test]
    fn roundtrip_blob() {
        let b = (
            "test".to_string(),
            blake3::Hash::from_hex(
                "3aa61c409fd7717c9d9c639202af2fae470c0ef669be7ba2caea5779cb534e9d",
            )
            .unwrap()
            .into(),
        );

        let mut buf = bytes::BytesMut::zeroed(1024);
        postcard::to_slice(&b, &mut buf).unwrap();
        let deserialize_b: (String, Hash) = postcard::from_bytes(&buf).unwrap();
        assert_eq!(b, deserialize_b);
    }

    #[test]
    fn roundtrip_collection_meta() {
        let expected = CollectionMeta {
            names: vec!["test".to_string(), "a".to_string(), "b".to_string()],
        };
        let mut buf = bytes::BytesMut::zeroed(1024);
        postcard::to_slice(&expected, &mut buf).unwrap();
        let actual: CollectionMeta = postcard::from_bytes(&buf).unwrap();
        assert_eq!(expected, actual);
    }
}
