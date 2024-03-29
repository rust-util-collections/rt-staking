use ruc::*;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    mem,
    result::Result as StdResult,
};
use vsdb_trie_map::{TrieHash, TrieMap, ValueEnDe};

pub type ValidatorID = Vec<u8>;
pub type ValidatorIDRef<'a> = &'a [u8];

pub type StakerID = Vec<u8>;
// pub type StakerIDRef<'a> = &'a [u8];

pub type Amount = u128;

pub type Power = Amount;

/// Used to measure the overall quality of a validator.
pub type Score = i64;

// The maximum value of validator score is 10_0000.
const SCORE_MAX: Score = 10_0000;

// For offline behaviors,
// the `score` should not be deducted to a negative/zero value,
// that is, the minimum value is 1.
const SCORE_MIN_OFFLINE: Score = 1;

// The capacity definition of formal validators.
const DEFAULT_VALIDATOR_CAP: u32 = 100;

macro_rules! from_be_bytes {
    ($t: ty, $bytes: expr) => {{
        <[u8; mem::size_of::<$t>()]>::try_from(&$bytes[..])
            .map(<$t>::from_be_bytes)
            .map_err(|e| eg!("{:?}", e))
    }};
}

pub struct Staking {
    // validator id => Vldtor { staker id => amount, .. }
    state: TrieMap,

    config: Config,
}

impl Staking {
    /// NOTE: native usage only
    pub fn new_default() -> Result<Self> {
        Self::new(None, None, None, None).c(d!())
    }

    /// NOTE: native usage only
    pub fn new(
        score_max: Option<Score>,
        score_min_for_offline: Option<Score>,
        validator_cap: Option<u32>,
        cache_size: Option<usize>,
    ) -> Result<Self> {
        let mut me = Self {
            state: TrieMap::create(cache_size).c(d!())?,
            config: Config {
                score_max: score_max.unwrap_or(SCORE_MAX),
                score_min_for_offline: score_min_for_offline.unwrap_or(SCORE_MIN_OFFLINE),
                validator_cap: validator_cap.unwrap_or(DEFAULT_VALIDATOR_CAP),
            },
        };

        me.commit();

        Ok(me)
    }

    pub fn root(&self) -> TrieHash {
        self.state.root()
    }

    /// Call this API after all changes
    pub fn commit(&mut self) -> TrieHash {
        self.state.commit()
    }

    pub fn score_max(&self) -> Score {
        self.config.score_max
    }

    fn score_min_for_offline(&self) -> Score {
        self.config.score_min_for_offline
    }

    pub fn validator_cap(&self) -> u32 {
        self.config.validator_cap
    }

    pub fn set_validator_cap(&mut self, n: u32) {
        self.config.validator_cap = n;
    }

    /// Do a full replacement for the current validator set
    pub fn refresh_validators(
        &mut self,
        validators: &BTreeMap<ValidatorID, Validator>,
    ) -> Result<()> {
        self.state.clear().c(d!())?;
        for (id, v) in validators.iter() {
            let v = Vldtor::try_from(v).c(d!())?;
            self.state.insert(id, &v.to_bytes()).c(d!())?;
        }
        Ok(())
    }

    fn get_vldtors(&self) -> Result<Vec<Vldtor>> {
        let mut ret = vec![];
        for i in self.state.ro_handle(self.state.root()).iter() {
            let (_, v) = i.c(d!())?;
            let v = Vldtor::from_bytes(&v).c(d!())?;
            ret.push(v);
        }
        Ok(ret)
    }

    fn get_vldtor(&self, id: ValidatorIDRef) -> Result<Vldtor> {
        self.state
            .get(id)
            .c(d!())?
            .c(d!("not found"))
            .and_then(|v| Vldtor::from_bytes(&v).c(d!()))
    }

    pub fn get_validators(&self) -> Result<Vec<Validator>> {
        let mut ret = vec![];
        for v in self.get_vldtors().c(d!())?.iter() {
            ret.push(Validator::try_from(v).c(d!())?);
        }
        Ok(ret)
    }

    pub fn get_validator(&self, id: ValidatorIDRef) -> Result<Validator> {
        self.get_vldtor(id)
            .c(d!())
            .and_then(|v| Validator::try_from(&v).c(d!()))
    }

    /// NOTE: gas will not be charged in this function
    pub fn stake_to<B: BalanceDB>(
        &mut self,
        sender: &mut AccMgmt<B>,
        validator: ValidatorIDRef,
        amount: Amount,
        static_validator_set: bool,
    ) -> Result<()> {
        alt!(0 == amount, return Ok(()));

        if sender
            .balance
            .balance_is_sufficient(sender.address, amount)
            .c(d!())?
        {
            sender.balance_sub(amount).c(d!())?;
        } else {
            return Err(eg!("Insufficient balance"));
        }

        let mut v = if let Some(v) = self.state.get(validator).c(d!())? {
            Vldtor::from_bytes(&v).c(d!())?
        } else if static_validator_set {
            // POA
            return Err(eg!("The target validator not found!"));
        } else {
            Vldtor::new(validator.to_vec(), None).c(d!())?
        };

        v.staking_total = v.staking_total.checked_add(amount).c(d!())?;

        let old_am = if let Some(bytes) = v.storage.get(sender.address).c(d!())? {
            from_be_bytes!(Amount, bytes)?
        } else {
            0
        };
        let new_am = old_am.checked_add(amount).c(d!())?;

        v.storage
            .insert(sender.address, new_am.to_be_bytes().as_slice())
            .c(d!())?;

        // commit the per-validator local trie
        // but do not commit the global state trie,
        // for better perforance?
        v.storage.commit();

        // set balance before change the state
        sender
            .balance_commit()
            .c(d!())
            .and_then(|_| self.state.insert(validator, &v.to_bytes()).c(d!()))
    }

    /// NOTE: gas will not be charged in this function
    pub fn unstake_from<B: BalanceDB>(
        &mut self,
        sender: &mut AccMgmt<B>,
        validator: ValidatorIDRef,
        amount: Option<Amount>,
    ) -> Result<Amount> {
        let mut v = self
            .state
            .get(validator)
            .c(d!())?
            .c(d!("Vldtor does not exist"))
            .and_then(|v| Vldtor::from_bytes(&v).c(d!()))?;

        if 0 > v.score {
            return Err(eg!(
                "Unstake from validators with negative scores are not allowed"
            ));
        }

        let total_am = v
            .storage
            .get(sender.address)
            .c(d!())?
            .c(d!())
            .and_then(|am| from_be_bytes!(Amount, am))?;
        let amount = amount.unwrap_or(total_am);

        let new_am = if amount > total_am {
            return Err(eg!("Target amount exceed total amount"));
        } else if 0 == amount {
            return Ok(0);
        } else {
            total_am - amount
        };

        sender.balance_sub(amount).c(d!())?;

        v.staking_total = v.staking_total.checked_sub(amount).c(d!())?;

        if 0 == new_am {
            v.storage.remove(sender.address).c(d!())?;
        } else {
            v.storage
                .insert(sender.address, new_am.to_be_bytes().as_slice())
                .c(d!())?;
        }

        // commit the per-validator local trie
        // but do not commit the global state trie,
        // for better perforance?
        v.storage.commit();

        // set balance before change the state
        sender.balance_commit().c(d!())?;

        if 0 == v.staking_total {
            self.state.remove(validator).c(d!())?;
        } else {
            self.state
                .insert(validator, v.to_bytes().as_slice())
                .c(d!())?;
        }

        Ok(amount)
    }

    /// NOTE: gas will not be charged in this function
    pub fn unstake_all<B: BalanceDB>(&mut self, sender: &mut AccMgmt<B>) -> Result<Amount> {
        let mut validators = self.get_vldtors().c(d!())?;

        let mut amount: Amount = 0;
        for v in validators.iter_mut() {
            if 0 < v.score {
                let am = if let Some(am) = v.storage.get(sender.address).c(d!())? {
                    from_be_bytes!(Amount, am)?
                } else {
                    0
                };
                v.staking_total = v.staking_total.checked_sub(am).c(d!())?;
                amount = amount.checked_add(am).c(d!())?;
            }
        }

        for v in validators.iter_mut() {
            // commit the per-validator local trie
            // but do not commit the global state trie,
            // for better perforance?
            v.storage.commit();
        }

        // set balance before change the state
        sender
            .balance_add(amount)
            .c(d!())
            .and_then(|_| sender.balance_commit().c(d!()))?;

        for v in validators.iter_mut() {
            v.storage.remove(sender.address).c(d!())?;
            if 0 == v.staking_total {
                self.state.remove(&v.id).c(d!())?;
            } else {
                self.state.insert(&v.id, &v.to_bytes()).c(d!())?;
            }
        }

        Ok(amount)
    }

    pub fn get_vldtor_score(&self, id: ValidatorIDRef) -> Result<Score> {
        self.get_vldtor(id).c(d!()).map(|v| v.score)
    }

    pub fn get_vldtor_staking_total(&self, id: ValidatorIDRef) -> Result<Amount> {
        self.get_vldtor(id).c(d!()).map(|v| v.staking_total)
    }

    pub fn get_vldtor_power(&self, id: ValidatorIDRef) -> Result<Amount> {
        self.get_vldtor(id).c(d!()).map(|v| v.voting_power())
    }

    pub fn validator_in_formal_list(&self, id: ValidatorIDRef) -> Result<bool> {
        self.validator_formal_list()
            .c(d!())
            .map(|l| l.contains_key(id))
    }

    /// Get the formal validator list
    pub fn validator_formal_list(&self) -> Result<BTreeMap<ValidatorID, Power>> {
        self.validator_power_top_n(self.validator_cap())
            .c(d!())
            .map(|l| {
                l.into_iter()
                    .map(|(v, power)| (v.id, power))
                    .collect::<BTreeMap<_, _>>()
            })
    }

    // TODO: implement a pre-sorted cache for better performance
    fn validator_power_top_n(&self, n: u32) -> Result<Vec<(Vldtor, Power)>> {
        let mut vldtors = self
            .get_vldtors()
            .c(d!())?
            .into_iter()
            .map(|v| {
                let p = v.voting_power();
                (v, p)
            })
            .filter(|(_, power)| 0 < *power)
            .collect::<Vec<_>>();

        vldtors.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        vldtors.truncate(n as usize);

        Ok(vldtors)
    }

    // Unconditional increment
    fn validator_score_incr_by_new_block(&mut self) -> Result<()> {
        self.validator_score_incr_by_n(None, 1).c(d!())
    }

    fn validator_score_incr_by_online(&mut self, id: ValidatorIDRef) -> Result<()> {
        self.validator_score_incr_by_n(Some(id), 100).c(d!())
    }

    fn validator_score_incr_by_n(&mut self, id: Option<ValidatorIDRef>, n: Score) -> Result<()> {
        let score_max = self.score_max();

        if let Some(id) = id {
            let mut v = self.get_vldtor(id).c(d!())?;
            v.score_incr_by_n(n, score_max);
            self.state.insert(id, &v.to_bytes()).c(d!())?;
        } else {
            for v in self.get_vldtors().c(d!())?.iter_mut() {
                v.score_incr_by_n(n, score_max);
                self.state.insert(&v.id, &v.to_bytes()).c(d!())?;
            }
        }

        Ok(())
    }

    fn validator_score_decr_by_offline(&mut self, id: ValidatorIDRef) -> Result<()> {
        self.validator_score_decr_by_n(id, Vldtor::score_decr_for_offline(), true)
            .c(d!())
    }

    // Malicious behaviors
    fn validator_score_decr_by_punishment(&mut self, id: ValidatorIDRef) -> Result<()> {
        self.validator_score_decr_by_n(
            id,
            Vldtor::score_decr_for_punishment(self.score_max()),
            false,
        )
        .c(d!())
    }

    fn validator_score_decr_by_n(
        &mut self,
        id: ValidatorIDRef,
        n: Score,
        reason_is_offline: bool,
    ) -> Result<()> {
        let min_in_offline = if reason_is_offline {
            Some(self.score_min_for_offline())
        } else {
            None
        };

        let mut v = self.get_vldtor(id).c(d!())?;
        v.score_decr_by_n(n, min_in_offline);
        self.state.insert(id, &v.to_bytes()).c(d!())
    }

    fn apply_punishments(&mut self, punishments: Vec<Punishment>) -> Result<()> {
        for p in punishments.into_iter() {
            match p {
                Punishment::Malicious(validators) => {
                    for v in validators.iter() {
                        self.validator_score_decr_by_punishment(v).c(d!())?;
                    }
                }
                Punishment::Offline((offline_validators, online_validators)) => {
                    for v in offline_validators.iter() {
                        self.validator_score_decr_by_offline(v).c(d!())?;
                    }
                    for v in online_validators.iter() {
                        self.validator_score_incr_by_online(v).c(d!())?;
                    }
                }
            }
        }

        Ok(())
    }

    /// NOTE: native usage only
    pub fn governance_with_each_block(&mut self, governances: Vec<Punishment>) -> Result<()> {
        self.validator_score_incr_by_new_block()
            .c(d!())
            .and_then(|_| self.apply_punishments(governances).c(d!()))
    }
}

struct Vldtor {
    id: ValidatorID,
    score: Score,

    // All tokens staked to it, including its own share.
    staking_total: Amount,

    // staker id => staking amount
    storage: TrieMap,
}

impl Vldtor {
    fn new(id: ValidatorID, score: Option<Score>) -> Result<Self> {
        Ok(Self {
            id,
            score: score.unwrap_or(SCORE_MAX),
            staking_total: 0,
            storage: TrieMap::create(None).c(d!())?,
        })
    }

    fn to_bytes(&self) -> Vec<u8> {
        let storage = self.storage.encode();
        let score = self.score.to_be_bytes();
        let total = self.staking_total.to_be_bytes();
        let data = [
            self.id.as_slice(),
            score.as_slice(),
            total.as_slice(),
            storage.as_slice(),
        ];
        pnk!(bcs::to_bytes(data.as_slice()), "{:?}", data)
    }

    fn from_bytes(data: &[u8]) -> Result<Self> {
        let raw = bcs::from_bytes::<[Vec<u8>; 4]>(data).c(d!())?;

        let score = from_be_bytes!(Score, raw[1])?;
        let staking_total = from_be_bytes!(Amount, raw[2])?;
        let storage = TrieMap::decode(&raw[3]).c(d!())?;

        Ok(Self {
            id: raw[0].clone(),
            score,
            staking_total,
            storage,
        })
    }

    fn voting_power(&self) -> Amount {
        let score = alt!(self.score < 0, 0, self.score);
        self.staking_total.saturating_mul(score as Amount)
    }

    // // Unconditional increment each block
    // fn score_incr(&mut self, max: Score) {
    //     self.score_incr_by_n(1, max)
    // }

    // fn score_incr_for_re_online(&mut self, max: Score) {
    //     self.score_incr_by_n(100, max)
    // }

    fn score_incr_by_n(&mut self, n: Score, max: Score) {
        let mut new_score = self.score.saturating_add(n);
        if new_score > max {
            new_score = max;
        }
        self.score = new_score;
    }

    fn score_decr_for_offline() -> Score {
        1000
    }

    // Malicious behaviors
    fn score_decr_for_punishment(max: Score) -> Score {
        max.saturating_mul(100)
    }

    fn score_decr_by_n(&mut self, n: Score, min: Option<Score>) {
        let mut new_score = self.score.saturating_sub(n);
        if let Some(min) = min {
            if min > new_score {
                new_score = min;
            }
        }
        self.score = new_score;
    }
}

impl Serialize for Vldtor {
    fn serialize<S>(&self, serializer: S) -> StdResult<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_bytes().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Vldtor {
    fn deserialize<D>(deserializer: D) -> StdResult<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <Vec<u8> as Deserialize>::deserialize(deserializer)
            .and_then(|bytes| Self::from_bytes(&bytes).map_err(serde::de::Error::custom))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Validator {
    pub id: ValidatorID,
    pub score: Score,
    pub storage: BTreeMap<StakerID, Amount>,
}

impl TryFrom<&Validator> for Vldtor {
    type Error = Box<dyn ruc::RucError>;
    fn try_from(t: &Validator) -> Result<Vldtor> {
        let staking_total = t.storage.values().sum();
        let mut storage = TrieMap::create(None).c(d!())?;
        for (id, am) in t.storage.iter() {
            storage.insert(id, &am.to_be_bytes()).c(d!())?;
        }

        storage.commit();

        Ok(Self {
            id: t.id.clone(),
            score: t.score,
            staking_total,
            storage,
        })
    }
}

impl TryFrom<&Vldtor> for Validator {
    type Error = Box<dyn ruc::RucError>;
    fn try_from(t: &Vldtor) -> Result<Validator> {
        let mut storage = BTreeMap::new();
        for i in t.storage.ro_handle(t.storage.root()).iter() {
            let (id, am) = i.c(d!())?;
            let am = from_be_bytes!(Amount, am)?;
            storage.insert(id, am);
        }
        Ok(Self {
            id: t.id.clone(),
            score: t.score,
            storage,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Config {
    score_max: Score,
    score_min_for_offline: Score,

    // How many validators at most can exist
    validator_cap: u32,
}

#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Punishment {
    // Malicious validators
    Malicious(BTreeSet<ValidatorID>),
    // Offline validators
    Offline((BTreeSet<ValidatorID>, BTreeSet<ValidatorID>)),
}

pub struct AccMgmt<'a, B: BalanceDB> {
    pub address: &'a [u8],
    pub nonce: u128,
    pub balance: &'a B,

    pub balance_cache_add: Amount,
    pub balance_cache_sub: Amount,
}

impl<'a, B: BalanceDB> AccMgmt<'a, B> {
    pub fn new(address: &'a [u8], nonce: u128, balance: &'a B) -> Self {
        Self {
            address,
            nonce,
            balance,
            balance_cache_add: 0,
            balance_cache_sub: 0,
        }
    }

    pub fn check_nonce(&self, nonce: u128) -> Result<()> {
        if self.nonce != nonce {
            Err(eg!("Invalid nonce"))
        } else {
            Ok(())
        }
    }

    pub fn incre_nonce(&mut self) {
        self.nonce += 1;
    }

    pub fn balance_add(&mut self, amount: Amount) -> Result<()> {
        self.balance_cache_add = self.balance_cache_add.checked_add(amount).c(d!())?;
        Ok(())
    }

    pub fn balance_sub(&mut self, amount: Amount) -> Result<()> {
        self.balance_cache_sub = self.balance_cache_sub.checked_add(amount).c(d!())?;
        Ok(())
    }

    pub fn balance_commit(&self) -> Result<()> {
        match self.balance_cache_add.cmp(&self.balance_cache_sub) {
            Ordering::Greater => {
                let am = self.balance_cache_add - self.balance_cache_sub;
                self.balance.balance_add(self.address, am).c(d!())
            }
            Ordering::Less => {
                let am = self.balance_cache_sub - self.balance_cache_add;
                self.balance.balance_sub(self.address, am).c(d!())
            }
            Ordering::Equal => Ok(()),
        }
    }
}

pub trait BalanceDB {
    fn balance_is_sufficient(&self, address: &[u8], needed_balance: Amount) -> Result<bool>;
    fn balance_sub(&self, address: &[u8], amount: Amount) -> Result<()>;
    fn balance_add(&self, address: &[u8], amount: Amount) -> Result<()>;
}
