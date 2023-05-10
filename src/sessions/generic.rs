use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use rand_core::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};

use crate::protocols::common::SessionId;
use crate::protocols::generic::{Round, ToSendTyped};
use crate::tools::collections::{HoleVecAccum, PartyIdx};

/// Serialized messages without the stage number specified.
pub enum ToSendSerialized {
    Broadcast(Box<[u8]>),
    // TODO: return an iterator instead, since preparing one message can take some time
    Direct(Vec<(PartyIdx, Box<[u8]>)>),
}

/// Serialized messages with the stage number specified.
pub enum ToSend<Id> {
    Broadcast { ids: Vec<Id>, message: Box<[u8]> },
    // TODO: return an iterator instead, since preparing one message can take some time
    Direct(Vec<(Id, Box<[u8]>)>),
}

fn serialize_message(message: &impl Serialize) -> Box<[u8]> {
    rmp_serde::encode::to_vec(message)
        .unwrap()
        .into_boxed_slice()
}

fn deserialize_message<M: for<'de> Deserialize<'de>>(
    message_bytes: &[u8],
) -> Result<M, rmp_serde::decode::Error> {
    rmp_serde::decode::from_slice(message_bytes)
}

fn serialize_with_round(round: u8, message: &[u8]) -> Box<[u8]> {
    rmp_serde::encode::to_vec(&(round, message))
        .unwrap()
        .into_boxed_slice()
}

fn deserialize_with_round(
    message_bytes: &[u8],
) -> Result<(u8, Box<[u8]>), rmp_serde::decode::Error> {
    rmp_serde::decode::from_slice(message_bytes)
}

#[derive(Clone)]
pub(crate) struct Stage<R: Round>
where
    for<'de> <R as Round>::Message: Deserialize<'de>,
{
    round: R,
    accum: Option<HoleVecAccum<R::Payload>>,
}

impl<R: Round> Stage<R>
where
    for<'de> <R as Round>::Message: Deserialize<'de>,
{
    pub(crate) fn new(round: R) -> Self {
        Self { round, accum: None }
    }

    pub(crate) fn get_messages(
        &mut self,
        rng: &mut (impl RngCore + CryptoRng),
        num_parties: usize,
        index: PartyIdx,
    ) -> Result<ToSendSerialized, String> {
        if self.accum.is_some() {
            return Err("The session is not in a sending state".into());
        }

        let to_send = match self.round.to_send(rng) {
            ToSendTyped::Broadcast(message) => {
                let message = serialize_message(&message);
                ToSendSerialized::Broadcast(message)
            }
            ToSendTyped::Direct(messages) => ToSendSerialized::Direct(
                messages
                    .into_iter()
                    .map(|(idx, message)| (idx, serialize_message(&message)))
                    .collect(),
            ),
        };

        let accum = HoleVecAccum::<R::Payload>::new(num_parties, index);
        self.accum = Some(accum);
        Ok(to_send)
    }

    pub(crate) fn receive(&mut self, from: PartyIdx, message_bytes: &[u8]) -> Result<(), String> {
        let accum = match self.accum.as_mut() {
            Some(accum) => accum,
            None => return Err("The session is in a sending stage, cannot receive messages".into()),
        };

        let message: R::Message = deserialize_message(message_bytes)
            .map_err(|err| format!("Error deserializing: {}", err))?;

        let slot = match accum.get_mut(from) {
            Some(slot) => slot,
            None => return Err("Invalid `from` ID".into()),
        };

        if slot.is_some() {
            return Err("Already received from this ID".into());
        }

        let payload = match self.round.verify_received(from, message) {
            Ok(res) => res,
            Err(_) => return Err("Error validating message".into()),
        };

        *slot = Some(payload);

        Ok(())
    }

    pub(crate) fn is_finished_receiving(&self) -> Result<bool, String> {
        Ok(match &self.accum {
            Some(accum) => accum.can_finalize(),
            None => return Err("Not in a receiving state".into()),
        })
    }

    pub(crate) fn finalize(
        self,
        rng: &mut (impl RngCore + CryptoRng),
    ) -> Result<R::NextRound, String> {
        let accum = match self.accum {
            Some(accum) => accum,
            None => return Err("The session is in a sending stage, cannot receive messages".into()),
        };

        if accum.can_finalize() {
            match accum.finalize() {
                Ok(finalized) => self
                    .round
                    .finalize(rng, finalized)
                    .map_err(|err| format!("Could not finalize: {err}")),
                Err(_) => Err("Messages from some of the parties are missing".into()),
            }
        } else {
            Err("Messages from some of the parties are missing".into())
        }
    }
}

// TODO: may be able to get rid of the clone requirement - perhaps with `take_mut`.
pub trait SessionState: Clone {
    type Context;
    fn new(
        rng: &mut (impl RngCore + CryptoRng),
        session_id: &SessionId,
        context: &Self::Context,
        index: PartyIdx,
    ) -> Self;
    fn get_messages(
        &mut self,
        rng: &mut (impl RngCore + CryptoRng),
        num_parties: usize,
        index: PartyIdx,
    ) -> Result<ToSendSerialized, String>;
    fn receive_current_stage(&mut self, from: PartyIdx, message_bytes: &[u8])
        -> Result<(), String>;
    fn is_finished_receiving(&self) -> Result<bool, String>;
    fn finalize_stage(self, rng: &mut (impl RngCore + CryptoRng)) -> Result<Self, String>;
    fn is_final_stage(&self) -> bool;
    fn current_stage_num(&self) -> u8;
    fn stages_num(&self) -> u8;
    fn result(&self) -> Result<Self::Result, String>;
    type Result;
}

pub trait PartyId:
    Clone + PartialEq + Eq + PartialOrd + Ord + Serialize + for<'de> Deserialize<'de>
{
}

pub trait ToTypedId<I: PartyId> {
    type Output;
    fn to_typed_id(self, ids: &[I], my_id: &I) -> Self::Output;
}

pub struct Session<S: SessionState, I: PartyId> {
    index: PartyIdx,
    my_id: I,
    all_parties: Vec<I>,
    next_stage_messages: Vec<(PartyIdx, Box<[u8]>)>,
    state: S,
}

impl<S: SessionState, I: PartyId> Session<S, I>
where
    S::Result: ToTypedId<I>,
{
    pub fn new(
        rng: &mut (impl RngCore + CryptoRng),
        session_id: &SessionId,
        all_parties: &[I],
        party_id: &I,
        context: &S::Context,
    ) -> Self {
        let index = all_parties.iter().position(|id| id == party_id).unwrap();
        let index = PartyIdx::from_usize(index);

        // CHECK: in the paper session id includes all the party ID's;
        // but since it's going to contain a random component too
        // (to distinguish sessions on the same node sets),
        // it might as well be completely random, right?

        let state = S::new(rng, session_id, context, index);
        Self {
            index,
            my_id: party_id.clone(),
            all_parties: all_parties.to_vec(),
            next_stage_messages: Vec::new(),
            state,
        }
    }

    pub fn get_messages(
        &mut self,
        rng: &mut (impl RngCore + CryptoRng),
    ) -> Result<ToSend<I>, String> {
        let to_send = self
            .state
            .get_messages(rng, self.all_parties.len(), self.index)?;
        let stage_num = self.state.current_stage_num();
        Ok(match to_send {
            ToSendSerialized::Broadcast(message) => {
                let ids = self
                    .all_parties
                    .iter()
                    .cloned()
                    .filter(|id| id != &self.my_id)
                    .collect();
                let message = serialize_with_round(stage_num, &message);
                ToSend::Broadcast { ids, message }
            }
            ToSendSerialized::Direct(messages) => ToSend::Direct(
                messages
                    .into_iter()
                    .map(|(index, message)| {
                        let id = self.all_parties[index.as_usize()].clone();
                        let message = serialize_with_round(stage_num, &message);
                        (id, message)
                    })
                    .collect(),
            ),
        })
    }

    pub fn receive(&mut self, from: &I, message_bytes: &[u8]) -> Result<(), String> {
        let stage_num = self.state.current_stage_num();
        let max_stages = self.state.stages_num();
        let (stage, message_bytes) = deserialize_with_round(message_bytes)
            .map_err(|err| format!("Error deserializing message: {}", err))?;
        let index = self.all_parties.iter().position(|id| id == from).unwrap();
        let index = PartyIdx::from_usize(index);

        if stage == stage_num + 1 && stage <= max_stages {
            self.next_stage_messages.push((index, message_bytes));
        } else if stage == stage_num {
            self.state.receive_current_stage(index, &message_bytes)?;
        } else {
            return Err(format!(
                "Unexpected message from round {stage} (current stage: {stage_num})"
            ));
        }

        Ok(())
    }

    pub fn receive_cached_message(&mut self) -> Result<(), String> {
        let (from, message_bytes) = self
            .next_stage_messages
            .pop()
            .ok_or("No more cached messages left")?;
        self.state.receive_current_stage(from, &message_bytes)
    }

    pub fn is_finished_receiving(&self) -> Result<bool, String> {
        self.state.is_finished_receiving()
    }

    pub fn finalize_stage(&mut self, rng: &mut (impl RngCore + CryptoRng)) -> Result<(), String> {
        // TODO: check that there are no cached messages left
        self.state = self.state.clone().finalize_stage(rng)?;
        Ok(())
    }

    pub fn result(&self) -> Result<<S::Result as ToTypedId<I>>::Output, String> {
        self.state
            .result()
            .map(|result| result.to_typed_id(&self.all_parties, &self.my_id))
    }

    pub fn is_final_stage(&self) -> bool {
        self.state.is_final_stage()
    }

    pub fn current_stage_num(&self) -> u8 {
        self.state.current_stage_num()
    }

    pub fn stages_num(&self) -> u8 {
        self.state.stages_num()
    }

    pub fn has_cached_messages(&self) -> bool {
        !self.next_stage_messages.is_empty()
    }
}

use alloc::collections::BTreeMap;

use k256::ecdsa::VerifyingKey;

use crate::protocols::common::{
    KeyShareChangePublic, KeyShareChangeSecret, KeyShareChangeVectorized, KeySharePublic,
    KeyShareSecret, KeyShareVectorized, PresigningData,
};
use crate::tools::group::{Point, Signature};
use crate::SchemeParams;

#[derive(Clone, Serialize, Deserialize)]
#[serde(bound(serialize = "KeyShareSecret<P>: Serialize, KeySharePublic<P>: Serialize"))]
#[serde(bound(deserialize = "for <'x> KeyShareSecret<P>: Deserialize<'x>,
    for <'x> KeySharePublic<P>: Deserialize<'x>"))]
pub struct KeyShare<I: PartyId, P: SchemeParams> {
    pub id: I,
    pub secret: KeyShareSecret<P>,
    pub public: BTreeMap<I, KeySharePublic<P>>,
}

impl<I: PartyId, P: SchemeParams> core::fmt::Debug for KeyShare<I, P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
        write!(
            f,
            "KeyShare(vkey={})",
            hex::encode(self.verifying_key_as_point().to_compressed_array())
        )
    }
}

impl<I: PartyId, P: SchemeParams> KeyShare<I, P> {
    pub(crate) fn verifying_key_as_point(&self) -> Point {
        self.public.values().map(|p| p.x).sum()
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        // TODO: need to ensure on creation of the share that the verifying key actually exists
        // (that is, the sum of public keys does not evaluate to the infinity point)
        self.verifying_key_as_point().to_verifying_key().unwrap()
    }

    pub fn parties(&self) -> Box<[I]> {
        self.public.keys().cloned().collect()
    }

    pub fn party(&self) -> I {
        self.id.clone()
    }
}

/// The result of the Auxiliary Info & Key Refresh protocol - the update to the key share.
#[derive(Clone)]
pub struct KeyShareChange<I: PartyId, P: SchemeParams> {
    pub id: I,
    #[allow(dead_code)]
    pub(crate) secret: KeyShareChangeSecret<P>,
    #[allow(dead_code)] // TODO: to be used in KeyShare.apply(KeyShareChange)
    pub(crate) public: BTreeMap<I, KeyShareChangePublic<P>>,
}

impl<I: PartyId> ToTypedId<I> for PresigningData {
    type Output = Self;
    fn to_typed_id(self, _ids: &[I], _my_id: &I) -> Self::Output {
        self
    }
}

impl<I: PartyId> ToTypedId<I> for Signature {
    type Output = Self;
    fn to_typed_id(self, _ids: &[I], _my_id: &I) -> Self::Output {
        self
    }
}

impl<I: PartyId, P: SchemeParams> ToTypedId<I> for KeyShareChangeVectorized<P> {
    type Output = KeyShareChange<I, P>;
    fn to_typed_id(self, ids: &[I], my_id: &I) -> Self::Output {
        KeyShareChange {
            id: my_id.clone(),
            secret: self.secret,
            public: ids
                .iter()
                .cloned()
                .zip(self.public.iter().cloned())
                .collect(),
        }
    }
}

impl<I: PartyId, P: SchemeParams> ToTypedId<I> for KeyShareVectorized<P> {
    type Output = KeyShare<I, P>;
    fn to_typed_id(self, ids: &[I], my_id: &I) -> Self::Output {
        KeyShare {
            id: my_id.clone(),
            secret: self.secret,
            public: ids
                .iter()
                .cloned()
                .zip(self.public.iter().cloned())
                .collect(),
        }
    }
}
