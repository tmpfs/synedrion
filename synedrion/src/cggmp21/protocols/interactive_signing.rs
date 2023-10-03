use alloc::boxed::Box;

use rand_core::CryptoRngCore;

use super::common::{KeyShare, PartyIdx};
use super::generic::{
    BroadcastRound, DirectRound, FinalizableToNextRound, FinalizableToResult, FinalizeError,
    FirstRound, InitError, ToNextRound, ToResult,
};
use super::presigning;
use super::signing;
use super::wrappers::RoundWrapper;
use crate::cggmp21::params::SchemeParams;
use crate::curve::{RecoverableSignature, Scalar};
use crate::tools::collections::HoleVec;

struct RoundContext<P: SchemeParams> {
    shared_randomness: Box<[u8]>,
    key_share: KeyShare<P>,
    message: Scalar,
}

#[derive(Clone)]
pub(crate) struct Context<P: SchemeParams> {
    pub(crate) key_share: KeyShare<P>,
    pub(crate) message: Scalar,
}

pub(crate) struct Round1<P: SchemeParams> {
    round: presigning::Round1<P>,
    context: RoundContext<P>,
}

impl<P: SchemeParams> FirstRound for Round1<P> {
    type Context = Context<P>;
    fn new(
        rng: &mut impl CryptoRngCore,
        shared_randomness: &[u8],
        num_parties: usize,
        party_idx: PartyIdx,
        context: Self::Context,
    ) -> Result<Self, InitError> {
        let round = presigning::Round1::new(
            rng,
            shared_randomness,
            num_parties,
            party_idx,
            context.key_share.clone(),
        )?;
        let context = RoundContext {
            shared_randomness: shared_randomness.into(),
            key_share: context.key_share,
            message: context.message,
        };
        Ok(Self { context, round })
    }
}

impl<P: SchemeParams> RoundWrapper for Round1<P> {
    type Type = ToNextRound;
    type Result = RecoverableSignature;
    type InnerRound = presigning::Round1<P>;
    const ROUND_NUM: u8 = 1;
    const NEXT_ROUND_NUM: Option<u8> = Some(2);
    fn inner_round(&self) -> &Self::InnerRound {
        &self.round
    }
}

impl<P: SchemeParams> FinalizableToNextRound for Round1<P> {
    type NextRound = Round2<P>;
    fn finalize_to_next_round(
        self,
        rng: &mut impl CryptoRngCore,
        bc_payloads: Option<HoleVec<<Self as BroadcastRound>::Payload>>,
        dm_payloads: Option<HoleVec<<Self as DirectRound>::Payload>>,
        dm_artefacts: Option<HoleVec<<Self as DirectRound>::Artefact>>,
    ) -> Result<Self::NextRound, FinalizeError> {
        let round =
            self.round
                .finalize_to_next_round(rng, bc_payloads, dm_payloads, dm_artefacts)?;
        Ok(Round2 {
            round,
            context: self.context,
        })
    }
}

pub(crate) struct Round2<P: SchemeParams> {
    round: presigning::Round2<P>,
    context: RoundContext<P>,
}

impl<P: SchemeParams> RoundWrapper for Round2<P> {
    type Type = ToNextRound;
    type Result = RecoverableSignature;
    type InnerRound = presigning::Round2<P>;
    const ROUND_NUM: u8 = 2;
    const NEXT_ROUND_NUM: Option<u8> = Some(3);
    fn inner_round(&self) -> &Self::InnerRound {
        &self.round
    }
}

impl<P: SchemeParams> FinalizableToNextRound for Round2<P> {
    type NextRound = Round3<P>;
    fn finalize_to_next_round(
        self,
        rng: &mut impl CryptoRngCore,
        bc_payloads: Option<HoleVec<<Self as BroadcastRound>::Payload>>,
        dm_payloads: Option<HoleVec<<Self as DirectRound>::Payload>>,
        dm_artefacts: Option<HoleVec<<Self as DirectRound>::Artefact>>,
    ) -> Result<Self::NextRound, FinalizeError> {
        let round =
            self.round
                .finalize_to_next_round(rng, bc_payloads, dm_payloads, dm_artefacts)?;
        Ok(Round3 {
            round,
            context: self.context,
        })
    }
}

pub(crate) struct Round3<P: SchemeParams> {
    round: presigning::Round3<P>,
    context: RoundContext<P>,
}

impl<P: SchemeParams> RoundWrapper for Round3<P> {
    type Type = ToNextRound;
    type Result = RecoverableSignature;
    type InnerRound = presigning::Round3<P>;
    const ROUND_NUM: u8 = 3;
    const NEXT_ROUND_NUM: Option<u8> = Some(4);
    fn inner_round(&self) -> &Self::InnerRound {
        &self.round
    }
}

impl<P: SchemeParams> FinalizableToNextRound for Round3<P> {
    type NextRound = Round4;
    fn finalize_to_next_round(
        self,
        rng: &mut impl CryptoRngCore,
        bc_payloads: Option<HoleVec<<Self as BroadcastRound>::Payload>>,
        dm_payloads: Option<HoleVec<<Self as DirectRound>::Payload>>,
        dm_artefacts: Option<HoleVec<<Self as DirectRound>::Artefact>>,
    ) -> Result<Self::NextRound, FinalizeError> {
        let presigning_data =
            self.round
                .finalize_to_result(rng, bc_payloads, dm_payloads, dm_artefacts)?;
        let num_parties = self.context.key_share.num_parties();
        let party_idx = self.context.key_share.party_index();
        let signing_context = signing::Context {
            message: self.context.message,
            presigning: presigning_data,
            verifying_key: self.context.key_share.verifying_key_as_point(),
        };
        let signing_round = signing::Round1::new(
            rng,
            &self.context.shared_randomness,
            num_parties,
            party_idx,
            signing_context,
        )
        .map_err(FinalizeError::ProtocolMergeSequential)?;

        Ok(Round4 {
            round: signing_round,
        })
    }
}

pub(crate) struct Round4 {
    round: signing::Round1,
}

impl RoundWrapper for Round4 {
    type Type = ToResult;
    type Result = RecoverableSignature;
    type InnerRound = signing::Round1;
    const ROUND_NUM: u8 = 4;
    const NEXT_ROUND_NUM: Option<u8> = None;
    fn inner_round(&self) -> &Self::InnerRound {
        &self.round
    }
}

impl FinalizableToResult for Round4 {
    fn finalize_to_result(
        self,
        rng: &mut impl CryptoRngCore,
        bc_payloads: Option<HoleVec<<Self as BroadcastRound>::Payload>>,
        dm_payloads: Option<HoleVec<<Self as DirectRound>::Payload>>,
        dm_artefacts: Option<HoleVec<<Self as DirectRound>::Artefact>>,
    ) -> Result<Self::Result, FinalizeError> {
        self.round
            .finalize_to_result(rng, bc_payloads, dm_payloads, dm_artefacts)
    }
}
