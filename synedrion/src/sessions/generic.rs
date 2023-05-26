use alloc::boxed::Box;
use alloc::vec::Vec;

use rand_core::CryptoRngCore;
use serde::{Deserialize, Serialize};

use super::error::{Error, MyFault, TheirFault};
use crate::protocols::common::{PartyIdx, SessionId};
use crate::protocols::generic::{Round, ToSendTyped};
use crate::tools::collections::HoleVecAccum;

/// Serialized messages without the stage number specified.
pub enum ToSendSerialized {
    Broadcast(Box<[u8]>),
    // TODO: return an iterator instead, since preparing one message can take some time
    Direct(Vec<(PartyIdx, Box<[u8]>)>),
}

/// Serialized messages with the stage number specified.
pub enum ToSend {
    Broadcast(Box<[u8]>),
    // TODO: return an iterator instead, since preparing one message can take some time
    Direct(Vec<(PartyIdx, Box<[u8]>)>),
}

fn serialize_message(message: &impl Serialize) -> Result<Box<[u8]>, MyFault> {
    rmp_serde::encode::to_vec(message)
        .map(|serialized| serialized.into_boxed_slice())
        .map_err(MyFault::SerializationError)
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
pub(crate) struct Stage<R: Round> {
    round: R,
    accum: Option<HoleVecAccum<R::Payload>>,
}

impl<R: Round> Stage<R> {
    pub(crate) fn new(round: R) -> Self {
        Self { round, accum: None }
    }

    pub(crate) fn get_messages(
        &mut self,
        rng: &mut impl CryptoRngCore,
        num_parties: usize,
        index: PartyIdx,
    ) -> Result<ToSendSerialized, MyFault> {
        if self.accum.is_some() {
            return Err(MyFault::InvalidState(
                "The session is not in a sending state".into(),
            ));
        }

        let to_send = match self.round.to_send(rng) {
            ToSendTyped::Broadcast(message) => {
                let message = serialize_message(&message)?;
                ToSendSerialized::Broadcast(message)
            }
            ToSendTyped::Direct(messages) => ToSendSerialized::Direct({
                let mut serialized = Vec::with_capacity(messages.len());
                for (idx, message) in messages.into_iter() {
                    serialized.push((idx, serialize_message(&message)?));
                }
                serialized
            }),
        };

        let accum = HoleVecAccum::<R::Payload>::new(num_parties, index.as_usize());
        self.accum = Some(accum);
        Ok(to_send)
    }

    pub(crate) fn receive(&mut self, from: PartyIdx, message_bytes: &[u8]) -> Result<(), Error> {
        let accum = match self.accum.as_mut() {
            Some(accum) => accum,
            None => {
                return Err(Error::MyFault(MyFault::InvalidState(
                    "The session is in a sending stage, cannot receive messages".into(),
                )))
            }
        };

        let message: R::Message =
            deserialize_message(message_bytes).map_err(|err| Error::TheirFault {
                party: from,
                error: TheirFault::DeserializationError(err),
            })?;

        let slot = match accum.get_mut(from.as_usize()) {
            Some(slot) => slot,
            None => return Err(Error::MyFault(MyFault::InvalidId(from))),
        };

        if slot.is_some() {
            return Err(Error::TheirFault {
                party: from,
                error: TheirFault::DuplicateMessage,
            });
        }

        let payload = match self.round.verify_received(from, message) {
            Ok(res) => res,
            Err(err) => {
                return Err(Error::TheirFault {
                    party: from,
                    error: err,
                })
            }
        };

        *slot = Some(payload);

        Ok(())
    }

    pub(crate) fn is_finished_receiving(&self) -> Result<bool, MyFault> {
        Ok(match &self.accum {
            Some(accum) => accum.can_finalize(),
            None => return Err(MyFault::InvalidState("Not in a receiving state".into())),
        })
    }

    pub(crate) fn finalize(self, rng: &mut impl CryptoRngCore) -> Result<R::NextRound, Error> {
        let accum = match self.accum {
            Some(accum) => accum,
            None => {
                return Err(Error::MyFault(MyFault::InvalidState(
                    "The session is in a sending stage, cannot receive messages".into(),
                )))
            }
        };

        if accum.can_finalize() {
            match accum.finalize() {
                Ok(finalized) => self
                    .round
                    .finalize(rng, finalized)
                    // TODO: we need to switch to the error round here
                    .map_err(|_err| Error::ErrorRound),
                // TODO: If this error fires, it indicates an error in `accum` implementation.
                // Can we make it impossible via types?
                Err(_) => Err(Error::MyFault(MyFault::InvalidState(
                    "Messages from some of the parties are missing".into(),
                ))),
            }
        } else {
            // This is our fault, because the caller needs to wait for all the messages,
            // and then invoke a special method to get the list of missing ones.
            // TODO: implement that method.
            Err(Error::MyFault(MyFault::InvalidState(
                "Messages from some of the parties are missing".into(),
            )))
        }
    }
}

// TODO: may be able to get rid of the clone requirement - perhaps with `take_mut`.
pub trait SessionState: Clone {
    type Context;
    fn new(
        rng: &mut impl CryptoRngCore,
        session_id: &SessionId,
        context: &Self::Context,
        index: PartyIdx,
    ) -> Self;
    fn get_messages(
        &mut self,
        rng: &mut impl CryptoRngCore,
        num_parties: usize,
        index: PartyIdx,
    ) -> Result<ToSendSerialized, MyFault>;
    fn receive_current_stage(&mut self, from: PartyIdx, message_bytes: &[u8]) -> Result<(), Error>;
    fn is_finished_receiving(&self) -> Result<bool, MyFault>;
    fn finalize_stage(self, rng: &mut impl CryptoRngCore) -> Result<Self, Error>;
    fn is_final_stage(&self) -> bool;
    fn current_stage_num(&self) -> u8;
    fn stages_num(&self) -> u8;
    fn result(&self) -> Result<Self::Result, MyFault>;
    type Result;
}

pub struct Session<S: SessionState> {
    index: PartyIdx,
    num_parties: usize,
    next_stage_messages: Vec<(PartyIdx, Box<[u8]>)>,
    state: S,
}

impl<S: SessionState> Session<S> {
    pub fn new(
        rng: &mut impl CryptoRngCore,
        session_id: &SessionId,
        num_parties: usize,
        index: PartyIdx,
        context: &S::Context,
    ) -> Self {
        // CHECK: in the paper session id includes all the party ID's;
        // but since it's going to contain a random component too
        // (to distinguish sessions on the same node sets),
        // it might as well be completely random, right?

        let state = S::new(rng, session_id, context, index);
        Self {
            index,
            num_parties,
            next_stage_messages: Vec::new(),
            state,
        }
    }

    pub fn get_messages(&mut self, rng: &mut impl CryptoRngCore) -> Result<ToSend, Error> {
        let to_send = self
            .state
            .get_messages(rng, self.num_parties, self.index)
            .map_err(Error::MyFault)?;
        let stage_num = self.state.current_stage_num();
        Ok(match to_send {
            ToSendSerialized::Broadcast(message) => {
                let message = serialize_with_round(stage_num, &message);
                ToSend::Broadcast(message)
            }
            ToSendSerialized::Direct(messages) => ToSend::Direct(
                messages
                    .into_iter()
                    .map(|(index, message)| {
                        let message = serialize_with_round(stage_num, &message);
                        (index, message)
                    })
                    .collect(),
            ),
        })
    }

    pub fn receive(&mut self, from: PartyIdx, message_bytes: &[u8]) -> Result<(), Error> {
        let stage_num = self.state.current_stage_num();
        let max_stages = self.state.stages_num();
        let (stage, message_bytes) =
            deserialize_with_round(message_bytes).map_err(|err| Error::TheirFault {
                party: from,
                error: TheirFault::DeserializationError(err),
            })?;

        if stage == stage_num + 1 && stage <= max_stages {
            self.next_stage_messages.push((from, message_bytes));
        } else if stage == stage_num {
            self.state.receive_current_stage(from, &message_bytes)?;
        } else {
            return Err(Error::TheirFault {
                party: from,
                error: TheirFault::OutOfOrderMessage {
                    current_stage: stage_num,
                    message_stage: stage,
                },
            });
        }

        Ok(())
    }

    pub fn receive_cached_message(&mut self) -> Result<(), Error> {
        let (from, message_bytes) = self.next_stage_messages.pop().ok_or_else(|| {
            Error::MyFault(MyFault::InvalidState("No more cached messages left".into()))
        })?;
        self.state.receive_current_stage(from, &message_bytes)
    }

    pub fn is_finished_receiving(&self) -> Result<bool, Error> {
        self.state.is_finished_receiving().map_err(Error::MyFault)
    }

    pub fn finalize_stage(&mut self, rng: &mut impl CryptoRngCore) -> Result<(), Error> {
        // TODO: check that there are no cached messages left
        self.state = self.state.clone().finalize_stage(rng)?;
        Ok(())
    }

    pub fn result(&self) -> Result<S::Result, Error> {
        self.state.result().map_err(Error::MyFault)
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
