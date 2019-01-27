//
// Copyright 2018-2019 Tamas Blummer
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
//!
//! # Blockchain DB for a node
//!

use bitcoin::{
    BitcoinHash,
    blockdata::{
        block::{Block, BlockHeader},
        constants::genesis_block,
        script::Script,
        transaction::{OutPoint, Transaction},
    },
    consensus::{Decodable, Decoder, Encodable, encode, Encoder},
    network::constants::Network,
    util::hash::Sha256dHash,
};

use byteorder::{BigEndian, ByteOrder};
use error::SPVError;
use filtercache::FilterCache;
use headercache::{HeaderCache, HeaderIterator};
use blockfilter::{BlockFilter, BlockFilterReader, COIN_FILTER, SCRIPT_FILTER};
use scriptcache::ScriptCache;

use hammersbald::{
    BitcoinAdaptor, HammersbaldAPI, persistent, PRef,
    transient,
};

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, RwLock, Mutex},
};
use std::{
    path::Path,
    io::Cursor
};

pub type SharedChainDB = Arc<RwLock<ChainDB>>;

pub struct ChainDB {
    light: BitcoinAdaptor,
    heavy: Option<BitcoinAdaptor>,
    headercache: HeaderCache,
    filtercache: FilterCache,
    scriptcache: Mutex<ScriptCache>,
    network: Network,
}

impl ChainDB {
    /// Create an in-memory database instance
    pub fn mem(network: Network, heavy: bool, script_cache_size: usize) -> Result<ChainDB, SPVError> {
        info!("working with in memory chain db");
        let light = BitcoinAdaptor::new(transient(2)?);
        let headercache = HeaderCache::new(network);
        let filtercache = FilterCache::new();
        let scriptcache = Mutex::new(ScriptCache::new(script_cache_size));
        if heavy {
            let heavy = Some(BitcoinAdaptor::new(transient(2)?));
            Ok(ChainDB { light, heavy, network, headercache, filtercache, scriptcache })
        } else {
            Ok(ChainDB { light, heavy: None, network, headercache, filtercache, scriptcache })
        }
    }

    /// Create or open a persistent database instance identified by the path
    pub fn new(path: &Path, network: Network, heavy: bool, script_cache_size: usize) -> Result<ChainDB, SPVError> {
        let basename = path.to_str().unwrap().to_string();
        let light = BitcoinAdaptor::new(persistent((basename.clone() + ".h").as_str(), 100, 2)?);
        let headercache = HeaderCache::new(network);
        let filtercache = FilterCache::new();
        let scriptcache = Mutex::new(ScriptCache::new(script_cache_size));
        if heavy {
            let heavy = Some(BitcoinAdaptor::new(persistent((basename + ".b").as_str(), 1000, 2)?));
            Ok(ChainDB { light, heavy, network, headercache, filtercache, scriptcache })
        } else {
            Ok(ChainDB { light, heavy: None, network, headercache, filtercache, scriptcache })
        }
    }

    pub fn init(&mut self, server: bool) -> Result<(), SPVError> {
        self.init_headers(server)?;
        Ok(())
    }

    pub fn batch(&mut self) -> Result<(), SPVError> {
        self.light.batch()?;
        if let Some(ref mut heavy) = self.heavy {
            heavy.batch()?;
        }
        Ok(())
    }

    fn init_headers(&mut self, server: bool) -> Result<(), SPVError> {
        let mut sl = VecDeque::new();
        {
            if let Some(tip) = self.fetch_header_tip()? {
                info!("reading stored header chain from tip {}", tip);
                let mut h = tip;
                while let Some(stored) = self.fetch_header(&h)? {
                    sl.push_front(stored.clone());
                    if stored.header.prev_blockhash != Sha256dHash::default() {
                        h = stored.header.prev_blockhash;
                    } else {
                        break;
                    }
                }
                info!("read {} headers", sl.len());
            }
        }

        if sl.is_empty() {
            info!("Initialized with genesis header.");
            let genesis = genesis_block(self.network).header;
            if let Some((stored, _, _)) = self.headercache.add_header(&genesis)? {
                self.store_header(&stored)?;
                self.store_header_tip(&stored.bitcoin_hash())?;
            }
        } else {
            self.headercache.clear();
            while let Some(stored) = sl.pop_front() {
                self.headercache.add_header_unchecked(&stored);
                if let Some(filter_ref) = stored.script_filter {
                    let filter = self.fetch_filter_by_ref(filter_ref)?;
                    self.filtercache.add_filter_header(&filter);
                }
                if let Some(filter_ref) = stored.coin_filter {
                    let filter = self.fetch_filter_by_ref(filter_ref)?;
                    if server {
                        self.filtercache.add_filter(&filter);
                    }
                    else {
                        self.filtercache.add_filter_header(&filter);
                    }
                }
            }
            info!("read {} filter header", self.filtercache.len());
        }
        Ok(())
    }

    pub fn add_header(&mut self, header: &BlockHeader) -> Result<Option<(StoredHeader, Option<Vec<Sha256dHash>>, Option<Vec<Sha256dHash>>)>, SPVError> {
        if let Some((stored, unwinds, forward)) = self.headercache.add_header(header)? {
            self.store_header(&stored)?;
            if let Some(forward) = forward.clone() {
                if forward.len() > 0 {
                    self.store_header_tip(forward.last().unwrap())?;
                }
            }
            return Ok(Some((stored, unwinds, forward)));
        }
        Ok(None)
    }

    pub fn update_header_with_block(&mut self, id: &Sha256dHash, block_ref: PRef) -> Result<Option<PRef>, SPVError> {
        if let Some(stored) = self.headercache.update_header_with_block(id, block_ref) {
            return Ok(Some(self.store_header(&stored)?));
        }
        Ok(None)
    }


    pub fn update_header_with_filter(&mut self, block_id: &Sha256dHash, script_ref: PRef, coin_ref: PRef) -> Result<Option<PRef>, SPVError> {
        if let Some(stored) = self.headercache.update_header_with_filter(block_id, script_ref, coin_ref) {
            return Ok(Some(self.store_header(&stored)?));
        }
        Ok(None)
    }

    // itarate from any hash to genesis. start_with might not be on trunk. result is an iterator of [genesis .. from]
    pub fn iter_to_genesis<'a>(&'a self, from: Option<Sha256dHash>) -> HeaderIterator<'a> {
        return self.headercache.iter_to_genesis(from);
    }

    // return position of hash on trunk if hash is on trunk
    pub fn pos_on_trunk(&self, hash: &Sha256dHash) -> Option<u32> {
        self.headercache.pos_on_trunk(hash)
    }

    // iterate trunk (after .. tip]
    pub fn iter_trunk<'a> (&'a self, after: u32) -> impl Iterator<Item=StoredHeader> +'a {
        self.headercache.iter_trunk(after)
    }

    // iterate trunk [genesis .. from] in reverse order from is the tip if not specified
    pub fn iter_trunk_rev<'a> (&'a self, from: Option<u32>) -> impl Iterator<Item=StoredHeader> +'a {
        self.headercache.iter_trunk_rev(from)
    }

    /// retrieve the id of the block/header with most work
    pub fn header_tip(&self) -> Option<StoredHeader> {
        self.headercache.tip()
    }

    /// Fetch a header by its id from cache
    pub fn get_header(&self, id: &Sha256dHash) -> Option<StoredHeader> {
        self.headercache.get_header(id)
    }

    /// Fetch a header by its id from cache
    pub fn get_header_for_height(&self, height: u32) -> Option<StoredHeader> {
        self.headercache.get_header_for_height(height)
    }

    // locator for getheaders message
    pub fn header_locators(&self) -> Vec<Sha256dHash> {
        self.headercache.locator_hashes()
    }

    pub fn add_filter_header (&mut self, filter: StoredFilter) -> Result<(), SPVError> {
        let filter_pref = self.store_filter(&filter)?;
        self.filtercache.add_filter_header(&filter);
        if filter.filter_type == SCRIPT_FILTER {
            self.update_header_with_filter(&filter.block_id, filter_pref, PRef::invalid())?;
        }
        if filter.filter_type == COIN_FILTER {
            self.update_header_with_filter(&filter.block_id, PRef::invalid(),filter_pref)?;
        }
        Ok(())
    }

    pub fn get_filter_header(&self, filter_id: &Sha256dHash) -> Option<StoredFilter> {
        self.filtercache.get_filter(filter_id)
    }

    pub fn get_block_filter_header(&self, block_id: &Sha256dHash, filter_type: u8) -> Option<StoredFilter> {
        self.filtercache.get_block_filter(block_id, filter_type)
    }

    pub fn get_block_filter(&self, block_id: &Sha256dHash, filter_type: u8) -> Result<Option<StoredFilter>, SPVError> {
        if let Some(filter) = self.filtercache.get_block_filter(block_id, filter_type) {
            if filter.filter.is_some () {
                return Ok(Some(filter));
            }
            else {
                if let Some(header) = self.get_header (block_id) {
                    if let Some(filter_ref) = if filter_type == SCRIPT_FILTER {
                        header.script_filter
                    } else if filter_type == COIN_FILTER {
                        header.coin_filter
                    } else {
                        None
                    } {
                        return Ok(Some(self.fetch_filter_by_ref(filter_ref)?));
                    }
                }
            }
        }
        Ok(None)
    }

    // update if matching stored filter_header chain
    pub fn update_filter(&mut self, block_id: &Sha256dHash, filter_type: u8, filter: Vec<u8>) -> Result<bool, SPVError> {
        if let Some(filter_header) = self.filtercache.get_block_filter(block_id, filter_type) {
            let filter_hash = Sha256dHash::from_data(filter.as_slice());
            let mut buf = [0u8; 64];
            buf[0..32].copy_from_slice(&filter_hash.to_bytes()[..]);
            buf[32..].copy_from_slice(&filter_header.previous.to_bytes()[..]);
            let filter_id = Sha256dHash::from_data(&buf);
            if filter_id == filter_header.bitcoin_hash() {
                let stored = StoredFilter {
                    block_id: *block_id,
                    previous: filter_header.previous,
                    filter_hash,
                    filter: Some(filter),
                    filter_type
                };
                self.store_filter(&stored)?;
                self.filtercache.add_filter_header(&stored);
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn store_header(&mut self, stored: &StoredHeader) -> Result<PRef, SPVError> {
        Ok(self.light.put_hash_keyed(stored)?)
    }

    pub fn store_header_tip(&mut self, tip: &Sha256dHash) -> Result<(), SPVError> {
        self.light.put_keyed_encodable(HEADER_TIP_KEY, tip)?;
        Ok(())
    }

    pub fn fetch_header_tip(&self) -> Result<Option<Sha256dHash>, SPVError> {
        Ok(self.light.get_keyed_decodable::<Sha256dHash>(HEADER_TIP_KEY)?.map(|(_, h)| h.clone()))
    }

    pub fn fetch_header(&self, id: &Sha256dHash) -> Result<Option<StoredHeader>, SPVError> {
        Ok(self.light.get_hash_keyed::<StoredHeader>(id)?.map(|(_, header)| header))
    }

    pub fn store_filter(&mut self, filter: &StoredFilter) -> Result<PRef, SPVError> {
        Ok(self.light.put_hash_keyed(filter)?)
    }


    pub fn fetch_filter_by_ref(&self, pref: PRef) -> Result<StoredFilter, SPVError> {
        Ok(self.light.get_decodable::<StoredFilter>(pref).map(|(_, filter)| filter)?)
    }

    pub fn fetch_filter(&self, filter_id: &Sha256dHash) -> Result<Option<StoredFilter>, SPVError> {
        Ok(self.light.get_hash_keyed::<StoredFilter>(filter_id)?.map(|(_, filter)| filter))
    }

    pub fn store_known_filter (&mut self, previous_script: &Sha256dHash, previous_coin: &Sha256dHash, script_filter: &BlockFilter, coin_filter: &BlockFilter) -> Result<(Sha256dHash, Sha256dHash), SPVError> {
        let stored_script_filter = StoredFilter{block_id: script_filter.block, previous: previous_script.clone(),
            filter_hash: Sha256dHash::from_data(script_filter.content.as_slice()), filter: Some(script_filter.content.clone()), filter_type: script_filter.filter_type };
        let pref_script = self.store_filter(&stored_script_filter)?;
        self.filtercache.add_filter_header(&stored_script_filter);

        let stored_coin_filter = StoredFilter{block_id: coin_filter.block, previous: previous_coin.clone(),
            filter_hash: Sha256dHash::from_data(coin_filter.content.as_slice()), filter: Some(coin_filter.content.clone()), filter_type: coin_filter.filter_type };
        let pref_coin = self.store_filter(&stored_coin_filter)?;
        self.filtercache.add_filter_header(&stored_coin_filter);

        self.update_header_with_filter(&script_filter.block, pref_script, pref_coin)?;
        Ok((stored_script_filter.bitcoin_hash(), stored_coin_filter.bitcoin_hash()))
    }

    pub fn store_block(&mut self, block: &Block) -> Result<PRef, SPVError> {
        if let Some(header) = self.headercache.get_header(&block.bitcoin_hash()) {
            let pref;
            if let Some(ref mut heavy) = self.heavy {
                pref = heavy.put_hash_keyed(&StoredBlock { id: block.bitcoin_hash(), txdata: block.txdata.clone() })?;
            } else {
                panic!("Configuration error. No db to store block.");
            }
            self.update_header_with_block(&block.bitcoin_hash(), pref)?;
            return Ok(pref);
        }
        panic!("should not call store block before header is known {}", block.bitcoin_hash());
    }

    pub fn fetch_block_by_ref(&self, pref: PRef) -> Result<StoredBlock, SPVError> {
        if let Some(ref heavy) = self.heavy {
            let (_, stored) = heavy.get_decodable::<StoredBlock>(pref)?;
            return Ok(stored)
        }
        panic!("Configuration error. No db to fetch block.");
    }

    pub fn cache_scripts(&mut self, block: &Block, height: u32) {
        let block_id = Arc::new(block.bitcoin_hash());
        let mut script_cache = self.scriptcache.lock().unwrap();
        for (i, tx) in block.txdata.iter().enumerate() {
            let tx_nr = i as u32;
            let txid = tx.txid();
            for (idx, output) in tx.output.iter().enumerate() {
                let vout = idx as u32;
                if !output.script_pubkey.is_provably_unspendable() {
                    script_cache.insert(OutPoint{txid, vout}, output.script_pubkey.clone(), height);
                }
            }
        }
    }

    pub fn get_scripts(&self, coins: Vec<OutPoint>, mut sofar: Vec<Script>) -> Result<Vec<Script>, SPVError> {
        let mut remains = Vec::with_capacity(coins.len());
        {
            // check in script cache
            let mut scriptcache = self.scriptcache.lock().unwrap();
            for coin in coins {
                if let Some(script) = scriptcache.remove(&coin) {
                    sofar.push(script);
                }
                else {
                    let mut buf = Vec::new();
                    coin.consensus_encode(&mut buf)?;
                    remains.push(buf);
                }
            }
        }
        remains.sort();
        if remains.len() > 0 {
            // check what remains in coin filters
            let from = self.scriptcache.lock().unwrap().complete_after();
            // iterate backward on blocks starting with the highest one not covered by the cache
            for header in self.iter_trunk_rev(Some(from)) {
                // if filter is known for this block
                if let Some(filter) = self.get_block_filter(&header.header.bitcoin_hash(), COIN_FILTER)? {
                    // check in a single pass read if any coins we search for might be in the filter for this block
                    let reader = BlockFilterReader::new(&header.bitcoin_hash())?;
                    if reader.match_any(&mut Cursor::new(filter.filter.unwrap()), &remains)? {
                        // do we have the block ?
                        if let Some(block_ref) = header.block {
                            // retrieve the block
                            let block = self.fetch_block_by_ref(block_ref)?;
                            // for all transactions
                            for tx in &block.txdata {
                                let txid = tx.txid();
                                // check if any or many! outputs of this transaction are those we search for
                                while let Ok(pos) = remains.binary_search_by(|r| r[0..32].cmp(txid.as_bytes())) {
                                    // a transaction that we are interested in
                                    let coin = OutPoint::consensus_decode(&mut Cursor::new(remains[pos].as_slice()))?;
                                    // get the script
                                    sofar.push(tx.output[coin.vout as usize].script_pubkey.clone());
                                    // one less to worry about
                                    remains.remove(pos);
                                    // are we done?
                                    if remains.len() == 0 {
                                        break;
                                    }
                                }
                                // are we done?
                                if remains.len() == 0 {
                                    break;
                                }
                            }
                            // are we done?
                            if remains.len() == 0 {
                                break;
                            }
                        }
                    }
                }
            }
        }
        // are we done?
        if remains.len () > 0 {
            let coins = remains.iter().map(|v| OutPoint::consensus_decode(&mut Cursor::new(v.as_slice())).unwrap())
                .map(|o| format!("{} {}", o.txid, o.vout)).collect::<Vec<_>>();
            error!("can not find coins {:?}", coins);
            return Err(SPVError::UnknownUTXO);
        }
        Ok(sofar)
    }

    pub fn get_script_accessor<'a>(&'a self, block: &Block) -> DBScriptAccessor<'a> {
        DBScriptAccessor::new(self, block)
    }
}

/// A header enriched with information about its position on the blockchain
#[derive(Clone)]
pub struct StoredHeader {
    /// header
    pub header: BlockHeader,
    /// chain height
    pub height: u32,
    /// log2 of total work
    pub log2work: f32,
    /// pointer to block if known
    pub block: Option<PRef>,
    /// pointer to script filter if known
    pub script_filter: Option<PRef>,
    /// pointer to coin filter if known
    pub coin_filter: Option<PRef>,
}

// need to implement if put_hash_keyed and get_hash_keyed should be used
impl BitcoinHash for StoredHeader {
    fn bitcoin_hash(&self) -> Sha256dHash {
        self.header.bitcoin_hash()
    }
}

// implement encoder. tedious just repeat the consensus_encode lines
impl<S: Encoder> Encodable<S> for StoredHeader {
    fn consensus_encode(&self, s: &mut S) -> Result<(), encode::Error> {
        self.header.consensus_encode(s)?;
        self.height.consensus_encode(s)?;
        let mut buf = [0u8; 4];
        BigEndian::write_f32(&mut buf, self.log2work);
        buf.consensus_encode(s)?;
        if let Some(pref) = self.block {
            pref.consensus_encode(s)?;
        } else {
            PRef::invalid().consensus_encode(s)?;
        }
        if let Some(pref) = self.script_filter {
            pref.consensus_encode(s)?;
        } else {
            PRef::invalid().consensus_encode(s)?;
        }
        if let Some(pref) = self.coin_filter {
            pref.consensus_encode(s)?;
        } else {
            PRef::invalid().consensus_encode(s)?;
        }
        Ok(())
    }
}

// implement decoder. tedious just repeat the consensus_encode lines
impl<D: Decoder> Decodable<D> for StoredHeader {
    fn consensus_decode(d: &mut D) -> Result<StoredHeader, encode::Error> {
        Ok(StoredHeader {
            header: Decodable::consensus_decode(d)?,
            height: Decodable::consensus_decode(d)?,
            log2work: {
                let buf: [u8; 4] = Decodable::consensus_decode(d)?;
                BigEndian::read_f32(&buf)
            },
            block: {
                let pref: PRef = Decodable::consensus_decode(d)?;
                if pref.is_valid() {
                    Some(pref)
                } else {
                    None
                }
            },
            script_filter: {
                let pref: PRef = Decodable::consensus_decode(d)?;
                if pref.is_valid() {
                    Some(pref)
                } else {
                    None
                }
            },
            coin_filter: {
                let pref: PRef = Decodable::consensus_decode(d)?;
                if pref.is_valid() {
                    Some(pref)
                } else {
                    None
                }
            }
        })
    }
}

const HEADER_TIP_KEY: &[u8] = &[0u8; 1];

/// Filter stored
#[derive(Clone)]
pub struct StoredFilter {
    /// filter type
    pub filter_type: u8,
    /// block
    pub block_id: Sha256dHash,
    /// hash of the filter content
    pub filter_hash: Sha256dHash,
    /// previous filter id
    pub previous: Sha256dHash,
    /// filter content
    pub filter: Option<Vec<u8>>,
}

// need to implement if put_hash_keyed and get_hash_keyed should be used
impl BitcoinHash for StoredFilter {
    fn bitcoin_hash(&self) -> Sha256dHash {
        let mut id_data = [0u8; 64];
        id_data[0..32].copy_from_slice(&self.filter_hash.as_bytes()[..]);
        id_data[0..32].copy_from_slice(&self.previous.as_bytes()[..]);
        Sha256dHash::from_data(&id_data)
    }
}

// implement encoder. tedious just repeat the consensus_encode lines
impl<S: Encoder> Encodable<S> for StoredFilter {
    fn consensus_encode(&self, s: &mut S) -> Result<(), encode::Error> {
        self.filter_type.consensus_encode(s)?;
        self.block_id.consensus_encode(s)?;
        self.filter_hash.consensus_encode(s)?;
        self.previous.consensus_encode(s)?;
        if let Some(ref filter) = self.filter {
            filter.consensus_encode(s)?;
        } else {
            [0u8; 0].consensus_encode(s)?;
        }
        Ok(())
    }
}

// implement decoder. tedious just repeat the consensus_encode lines
impl<D: Decoder> Decodable<D> for StoredFilter {
    fn consensus_decode(d: &mut D) -> Result<StoredFilter, encode::Error> {
        Ok(StoredFilter {
            filter_type: Decodable::consensus_decode(d)?,
            block_id: Decodable::consensus_decode(d)?,
            filter_hash: Decodable::consensus_decode(d)?,
            previous: Decodable::consensus_decode(d)?,
            filter: {
                let f: Vec<u8> = Decodable::consensus_decode(d)?;
                if f.len() == 0 {
                    None
                } else {
                    Some(f)
                }
            },
        })
    }
}

/// Block stored
pub struct StoredBlock {
    pub id: Sha256dHash,
    // transactions
    pub txdata: Vec<Transaction>,
}

impl BitcoinHash for StoredBlock {
    fn bitcoin_hash(&self) -> Sha256dHash {
        self.id
    }
}

// implement encoder. tedious just repeat the consensus_encode lines
impl<S: Encoder> Encodable<S> for StoredBlock {
    fn consensus_encode(&self, s: &mut S) -> Result<(), encode::Error> {
        self.id.consensus_encode(s)?;
        self.txdata.consensus_encode(s)?;
        Ok(())
    }
}

// implement decoder. tedious just repeat the consensus_encode lines
impl<D: Decoder> Decodable<D> for StoredBlock {
    fn consensus_decode(d: &mut D) -> Result<StoredBlock, encode::Error> {
        Ok(StoredBlock {
            id: Decodable::consensus_decode(d)?,
            txdata: Decodable::consensus_decode(d)?,
        })
    }
}

pub struct DBScriptAccessor<'a> {
    db: &'a ChainDB,
    prev_block: Sha256dHash,
    same_block_scripts: HashMap<OutPoint, Script>,
}

impl<'a> DBScriptAccessor<'a> {
    pub fn new(db: &'a ChainDB, block: &Block) -> DBScriptAccessor<'a> {
        let mut acc = DBScriptAccessor { db: db, same_block_scripts: HashMap::new(), prev_block: block.header.prev_blockhash };
        for t in &block.txdata {
            let txid = t.txid();
            for (vout, o) in t.output.iter().enumerate() {
                acc.same_block_scripts.insert(OutPoint{txid, vout: vout as u32}, o.script_pubkey.clone());
            }
        }
        acc
    }
}


pub trait ScriptAccessor {
    fn get_scripts(&self, coins: Vec<OutPoint>) -> Result<Vec<Script>, SPVError>;
}

impl<'a> ScriptAccessor for DBScriptAccessor<'a> {
    fn get_scripts(&self, coins: Vec<OutPoint>) -> Result<Vec<Script>, SPVError> {
        let mut sofar = Vec::with_capacity(coins.len());
        let mut remains = Vec::with_capacity(coins.len());
        for coin in coins {
            if let Some(r) = self.same_block_scripts.get(&coin) {
                sofar.push(r.clone());
            }
            else {
                remains.push(coin);
            }
        }
        self.db.get_scripts(remains, sofar)
    }
}
