mod entry;
mod impls;

use core::borrow::Borrow;
use std::marker::PhantomData;

use borsh::{BorshDeserialize, BorshSerialize};
use once_cell::unsync::OnceCell;

use crate::hash::{CryptoHasher, Sha256};
use crate::utils::{EntryState, StableMap};
use crate::{env, CacheEntry, IntoStorageKey};
pub use entry::{Entry, OccupiedEntry, VacantEntry};

const ERR_ELEMENT_DESERIALIZATION: &[u8] = b"Cannot deserialize element";
const ERR_ELEMENT_SERIALIZATION: &[u8] = b"Cannot serialize element";
const ERR_NOT_EXIST: &[u8] = b"Key does not exist in map";

type LookupKey = [u8; 32];

/// A non-iterable, lazily loaded storage map that stores its content directly on the storage trie.
///
/// This map stores the values under a hash of the map's `prefix` and [`BorshSerialize`] of the key
/// using the map's [`CryptoHasher`] implementation.
///
/// The default hash function for [`LookupMap`] is [`Sha256`] which uses a syscall to hash the
/// key. To use a custom function, use [`new_with_hasher`]. Alternative builtin hash functions
/// can be found at [`near_sdk::hash`](crate::hash).
///
/// # Examples
/// ```
/// use near_sdk::store::LookupMap;
///
/// // Initializes a map, the generic types can be inferred to `LookupMap<String, u8, Sha256>`
/// let mut map = LookupMap::new(b"a");
///
/// map.set("test".to_string(), Some(7u8));
/// assert!(map.contains_key("test"));
/// assert_eq!(map.get("test"), Some(&7u8));
///
/// let prev = map.insert("test".to_string(), 5u8);
/// assert_eq!(prev, Some(7u8));
/// assert_eq!(map["test"], 5u8);
/// ```
///
/// `LookupMap` also implements an [`Entry API`](Self::entry), which allows
/// for more complex methods of getting, setting, updating and removing keys and
/// their values:
///
/// ```
/// use near_sdk::store::LookupMap;
///
/// // type inference lets us omit an explicit type signature (which
/// // would be `LookupMap<String, u8>` in this example).
/// let mut player_stats = LookupMap::new(b"m");
///
/// fn random_stat_buff() -> u8 {
///     // could actually return some random value here - let's just return
///     // some fixed value for now
///     42
/// }
///
/// // insert a key only if it doesn't already exist
/// player_stats.entry("health".to_string()).or_insert(100);
///
/// // insert a key using a function that provides a new value only if it
/// // doesn't already exist
/// player_stats.entry("defence".to_string()).or_insert_with(random_stat_buff);
///
/// // update a key, guarding against the key possibly not being set
/// let stat = player_stats.entry("attack".to_string()).or_insert(100);
/// *stat += random_stat_buff();
/// ```
///
/// [`new_with_hasher`]: Self::new_with_hasher
#[derive(BorshSerialize, BorshDeserialize)]
pub struct LookupMap<K, V, H = Sha256>
where
    K: BorshSerialize + Ord,
    V: BorshSerialize,
    H: CryptoHasher<Digest = [u8; 32]>,
{
    prefix: Box<[u8]>,
    #[borsh_skip]
    /// Cache for loads and intermediate changes to the underlying vector.
    /// The cached entries are wrapped in a [`Box`] to avoid existing pointers from being
    /// invalidated.
    cache: StableMap<K, OnceCell<CacheEntry<V>>>,

    #[borsh_skip]
    hasher: PhantomData<H>,
}

impl<K, V> LookupMap<K, V, Sha256>
where
    K: BorshSerialize + Ord,
    V: BorshSerialize,
{
    #[inline]
    pub fn new<S>(prefix: S) -> Self
    where
        S: IntoStorageKey,
    {
        Self::new_with_hasher(prefix)
    }
}

impl<K, V, H> LookupMap<K, V, H>
where
    K: BorshSerialize + Ord,
    V: BorshSerialize,
    H: CryptoHasher<Digest = [u8; 32]>,
{
    /// Initialize a [`LookupMap`] with a custom hash function.
    ///
    /// # Example
    /// ```
    /// use near_sdk::hash::Keccak256;
    /// use near_sdk::store::LookupMap;
    ///
    /// let map = LookupMap::<String, String, Keccak256>::new_with_hasher(b"m");
    /// ```
    pub fn new_with_hasher<S>(prefix: S) -> Self
    where
        S: IntoStorageKey,
    {
        Self {
            prefix: prefix.into_storage_key().into_boxed_slice(),
            cache: Default::default(),
            hasher: Default::default(),
        }
    }

    /// Overwrites the current value for the given key.
    ///
    /// This function will not load the existing value from storage and return the value in storage.
    /// Use [`LookupMap::insert`] if you need the previous value.
    ///
    /// Calling `set` with a `None` value will delete the entry from storage.
    pub fn set(&mut self, key: K, value: Option<V>) {
        let entry = self.cache.get_mut(key);
        match entry.get_mut() {
            Some(entry) => *entry.value_mut() = value,
            None => {
                let _ = entry.set(CacheEntry::new_modified(value));
            }
        }
    }

    fn lookup_key<Q: ?Sized>(prefix: &[u8], key: &Q) -> LookupKey
    where
        Q: BorshSerialize,
        K: Borrow<Q>,
    {
        // Concat the prefix with serialized key and hash the bytes for the lookup key.
        let mut buffer = prefix.to_vec();
        key.serialize(&mut buffer).unwrap_or_else(|_| env::panic(ERR_ELEMENT_SERIALIZATION));

        H::hash(&buffer)
    }
}

impl<K, V, H> LookupMap<K, V, H>
where
    K: BorshSerialize + Ord,
    V: BorshSerialize + BorshDeserialize,
    H: CryptoHasher<Digest = [u8; 32]>,
{
    fn deserialize_element(bytes: &[u8]) -> V {
        V::try_from_slice(bytes).unwrap_or_else(|_| env::panic(ERR_ELEMENT_DESERIALIZATION))
    }

    fn load_element<Q: ?Sized>(prefix: &[u8], key: &Q) -> Option<V>
    where
        Q: BorshSerialize,
        K: Borrow<Q>,
    {
        let storage_bytes = env::storage_read(&Self::lookup_key(prefix, key));
        storage_bytes.as_deref().map(Self::deserialize_element)
    }

    /// Returns a reference to the value corresponding to the key.
    ///
    /// The key may be any borrowed form of the map's key type, but
    /// [`BorshSerialize`] and [`ToOwned<Owned = K>`](ToOwned) on the borrowed form *must* match those for
    /// the key type.
    pub fn get<Q: ?Sized>(&self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: BorshSerialize + ToOwned<Owned = K>,
    {
        //* ToOwned bound, which forces a clone, is required to be able to keep the key in the cache
        let entry = self
            .cache
            .get(k.to_owned())
            .get_or_init(|| CacheEntry::new_cached(Self::load_element(&self.prefix, k)));
        entry.value().as_ref()
    }

    fn get_mut_inner<Q: ?Sized>(&mut self, k: &Q) -> &mut CacheEntry<V>
    where
        K: Borrow<Q>,
        Q: BorshSerialize + ToOwned<Owned = K>,
    {
        let prefix = &self.prefix;
        //* ToOwned bound, which forces a clone, is required to be able to keep the key in the cache
        let entry = self.cache.get_mut(k.to_owned());
        entry.get_or_init(|| CacheEntry::new_cached(Self::load_element(prefix, k)));
        let entry = entry.get_mut().unwrap_or_else(|| unreachable!());
        entry
    }

    /// Returns a mutable reference to the value corresponding to the key.
    ///
    /// The key may be any borrowed form of the map's key type, but
    /// [`BorshSerialize`] and [`ToOwned<Owned = K>`](ToOwned)on the borrowed form *must* match those for
    /// the key type.
    pub fn get_mut<Q: ?Sized>(&mut self, k: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: BorshSerialize + ToOwned<Owned = K>,
    {
        self.get_mut_inner(k).value_mut().as_mut()
    }

    /// Inserts a key-value pair into the map.
    ///
    /// If the map did not have this key present, [`None`] is returned.
    ///
    /// If the map did have this key present, the value is updated, and the old
    /// value is returned. The key is not updated, though; this matters for
    /// types that can be `==` without being identical.
    pub fn insert(&mut self, k: K, v: V) -> Option<V>
    where
        K: Clone,
    {
        self.get_mut_inner(&k).replace(Some(v))
    }

    /// Returns `true` if the map contains a value for the specified key.
    ///
    /// The key may be any borrowed form of the map's key type, but
    /// [`BorshSerialize`] and [`ToOwned<Owned = K>`](ToOwned)on the borrowed form *must* match those for
    /// the key type.
    pub fn contains_key<Q: ?Sized>(&self, k: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: BorshSerialize + ToOwned<Owned = K> + Ord,
    {
        // Check cache before checking storage
        if self
            .cache
            .with_value(&k, |v| v.get().and_then(|s| s.value().as_ref()).is_some())
            .unwrap_or(false)
        {
            return true;
        }
        let storage_key = Self::lookup_key(&self.prefix, k);
        let contains = env::storage_has_key(&storage_key);

        if !contains {
            // If value not in cache and not in storage, can set a cached `None`
            let _ = self.cache.get(k.to_owned()).set(CacheEntry::new_cached(None));
        }
        contains
    }

    /// Removes a key from the map, returning the value at the key if the key
    /// was previously in the map.
    ///
    /// The key may be any borrowed form of the map's key type, but
    /// [`BorshSerialize`] and [`ToOwned<Owned = K>`](ToOwned)on the borrowed form *must* match those for
    /// the key type.
    pub fn remove<Q: ?Sized>(&mut self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: BorshSerialize + ToOwned<Owned = K>,
    {
        self.get_mut_inner(k).replace(None)
    }

    /// Gets the given key's corresponding entry in the map for in-place manipulation.
    /// ```
    /// use near_sdk::store::LookupMap;
    ///
    /// let mut count = LookupMap::new(b"m");
    ///
    /// for ch in [7, 2, 4, 7, 4, 1, 7] {
    ///     let counter = count.entry(ch).or_insert(0);
    ///     *counter += 1;
    /// }
    ///
    /// assert_eq!(count[&4], 2);
    /// assert_eq!(count[&7], 3);
    /// assert_eq!(count[&1], 1);
    /// assert_eq!(count.get(&8), None);
    /// ```
    pub fn entry(&mut self, key: K) -> Entry<K, V>
    where
        K: Clone,
    {
        let entry = self.get_mut_inner(&key);
        if entry.value().is_some() {
            // Value exists in cache and is `Some`
            Entry::Occupied(OccupiedEntry { key, entry })
        } else {
            // Value exists in cache, but is `None`
            Entry::Vacant(VacantEntry { key, entry })
        }
    }
}

impl<K, V, H> LookupMap<K, V, H>
where
    K: BorshSerialize + Ord,
    V: BorshSerialize,
    H: CryptoHasher<Digest = [u8; 32]>,
{
    /// Flushes the intermediate values of the map before this is called when the structure is
    /// [`Drop`]ed. This will write all modified values to storage but keep all cached values
    /// in memory.
    pub fn flush(&mut self) {
        let mut buf = Vec::new();
        for (k, v) in self.cache.inner().iter_mut() {
            if let Some(v) = v.get_mut() {
                if v.is_modified() {
                    let key = Self::lookup_key(&self.prefix, k);
                    match v.value().as_ref() {
                        Some(modified) => {
                            buf.clear();
                            BorshSerialize::serialize(modified, &mut buf)
                                .unwrap_or_else(|_| env::panic(ERR_ELEMENT_SERIALIZATION));
                            env::storage_write(&key, &buf);
                        }
                        None => {
                            // Element was removed, clear the storage for the value
                            env::storage_remove(&key);
                        }
                    }

                    // Update state of flushed state as cached, to avoid duplicate writes/removes
                    // while also keeping the cached values in memory.
                    v.replace_state(EntryState::Cached);
                }
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(test)]
mod tests {
    use super::LookupMap;
    use crate::env;
    use crate::hash::Keccak256;
    use rand::seq::SliceRandom;
    use rand::{Rng, SeedableRng};
    use std::collections::HashMap;

    #[test]
    fn test_insert() {
        let mut map = LookupMap::new(b"m");
        let mut rng = rand_xorshift::XorShiftRng::seed_from_u64(0);
        for _ in 0..500 {
            let key = rng.gen::<u64>();
            let value = rng.gen::<u64>();
            map.insert(key, value);
        }
    }

    #[test]
    fn test_insert_has_key() {
        let mut map = LookupMap::new(b"m");
        let mut rng = rand_xorshift::XorShiftRng::seed_from_u64(0);
        let mut key_to_value = HashMap::new();
        for _ in 0..100 {
            let key = rng.gen::<u64>();
            let value = rng.gen::<u64>();
            map.insert(key, value);
            key_to_value.insert(key, value);
        }
        // Non existing
        for _ in 0..100 {
            let key = rng.gen::<u64>();
            assert_eq!(map.contains_key(&key), key_to_value.contains_key(&key));
        }
        // Existing
        for (key, _) in key_to_value.iter() {
            assert!(map.contains_key(&key));
        }
    }

    #[test]
    fn test_insert_remove() {
        let mut map = LookupMap::new(b"m");
        let mut rng = rand_xorshift::XorShiftRng::seed_from_u64(1);
        let mut keys = vec![];
        let mut key_to_value = HashMap::new();
        for _ in 0..100 {
            let key = rng.gen::<u64>();
            let value = rng.gen::<u64>();
            keys.push(key);
            key_to_value.insert(key, value);
            map.insert(key, value);
        }
        keys.shuffle(&mut rng);
        for key in keys {
            let actual = map.remove(&key).unwrap();
            assert_eq!(actual, key_to_value[&key]);
        }
    }

    #[test]
    fn test_remove_last_reinsert() {
        let mut map = LookupMap::new(b"m");
        let key1 = 1u64;
        let value1 = 2u64;
        map.insert(key1, value1);
        let key2 = 3u64;
        let value2 = 4u64;
        map.insert(key2, value2);

        let actual_value2 = map.remove(&key2).unwrap();
        assert_eq!(actual_value2, value2);

        let actual_insert_value2 = map.insert(key2, value2);
        assert_eq!(actual_insert_value2, None);
    }

    #[test]
    fn test_insert_override_remove() {
        let mut map = LookupMap::new(b"m");
        let mut rng = rand_xorshift::XorShiftRng::seed_from_u64(2);
        let mut keys = vec![];
        let mut key_to_value = HashMap::new();
        for _ in 0..100 {
            let key = rng.gen::<u64>();
            let value = rng.gen::<u64>();
            keys.push(key);
            key_to_value.insert(key, value);
            map.insert(key, value);
        }
        keys.shuffle(&mut rng);
        for key in &keys {
            let value = rng.gen::<u64>();
            let actual = map.insert(*key, value).unwrap();
            assert_eq!(actual, key_to_value[key]);
            key_to_value.insert(*key, value);
        }
        keys.shuffle(&mut rng);
        for key in keys {
            let actual = map.remove(&key).unwrap();
            assert_eq!(actual, key_to_value[&key]);
        }
    }

    #[test]
    fn test_get_non_existent() {
        let mut map = LookupMap::new(b"m");
        let mut rng = rand_xorshift::XorShiftRng::seed_from_u64(3);
        let mut key_to_value = HashMap::new();
        for _ in 0..500 {
            let key = rng.gen::<u64>() % 20_000;
            let value = rng.gen::<u64>();
            key_to_value.insert(key, value);
            map.insert(key, value);
        }
        for _ in 0..500 {
            let key = rng.gen::<u64>() % 20_000;
            assert_eq!(map.get(&key), key_to_value.get(&key));
        }
    }

    #[test]
    fn test_extend() {
        let mut map = LookupMap::new(b"m");
        let mut rng = rand_xorshift::XorShiftRng::seed_from_u64(4);
        let mut key_to_value = HashMap::new();
        for _ in 0..100 {
            let key = rng.gen::<u64>();
            let value = rng.gen::<u64>();
            key_to_value.insert(key, value);
            map.insert(key, value);
        }
        for _ in 0..10 {
            let mut tmp = vec![];
            for _ in 0..=(rng.gen::<u64>() % 20 + 1) {
                let key = rng.gen::<u64>();
                let value = rng.gen::<u64>();
                tmp.push((key, value));
            }
            key_to_value.extend(tmp.iter().cloned());
            map.extend(tmp.iter().cloned());
        }

        for (key, value) in key_to_value {
            assert_eq!(*map.get(&key).unwrap(), value);
        }
    }

    #[test]
    fn flush_on_drop() {
        let mut map = LookupMap::<_, _, Keccak256>::new_with_hasher(b"m");

        // Set a value, which does not write to storage yet
        map.set(5u8, Some(8u8));

        // Create duplicate which references same data
        assert_eq!(map[&5], 8);

        let storage_key = LookupMap::<u8, u8, Keccak256>::lookup_key(b"m", &5);
        assert!(!env::storage_has_key(&storage_key));

        drop(map);

        let dup_map = LookupMap::<u8, u8, Keccak256>::new_with_hasher(b"m");

        // New map can now load the value
        assert_eq!(dup_map[&5], 8);
    }
}