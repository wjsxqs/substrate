// Copyright 2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! # I'm online Module
//!
//! If the local node is a validator (i.e. contains an authority key), this module
//! gossips a heartbeat transaction with each new session. The heartbeat functions
//! as a simple mechanism to signal that the node is online in the current era.
//!
//! Received heartbeats are tracked for one era and reset with each new era. The
//! module exposes two public functions to query if a heartbeat has been received
//! in the current era or session.
//!
//! The heartbeat is a signed transaction, which was signed using the session key
//! and includes the recent best block number of the local validators chain as well
//! as the [NetworkState](../../core/offchain/struct.NetworkState.html).
//! It is submitted as an Unsigned Transaction via off-chain workers.
//!
//! - [`im_online::Trait`](./trait.Trait.html)
//! - [`Call`](./enum.Call.html)
//! - [`Module`](./struct.Module.html)
//!
//! ## Interface
//!
//! ### Public Functions
//!
//! - `is_online_in_current_session` - True if the validator sent a heartbeat in the current session.
//!
//! ## Usage
//!
//! ```
//! use srml_support::{decl_module, dispatch::Result};
//! use system::ensure_signed;
//! use srml_im_online::{self as im_online};
//!
//! pub trait Trait: im_online::Trait {}
//!
//! decl_module! {
//! 	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
//! 		pub fn is_online(origin, authority_index: u32) -> Result {
//! 			let _sender = ensure_signed(origin)?;
//! 			let _is_online = <im_online::Module<T>>::is_online_in_current_session(authority_index);
//! 			Ok(())
//! 		}
//! 	}
//! }
//! # fn main() { }
//! ```
//!
//! ## Dependencies
//!
//! This module depends on the [Session module](../srml_session/index.html).

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

use substrate_primitives::{
	//crypto::{CryptoType, KeyTypeId},
	offchain::{self, OpaqueNetworkState, StorageKind},
};
use codec::{Encode, Decode};
use session::SessionIndex;
use sr_primitives::{
	Perbill, ApplyError, traits::{Extrinsic as ExtrinsicT, CurrentEraStartSessionIndex},
	transaction_validity::{TransactionValidity, TransactionLongevity, ValidTransaction},
	offence::{ReportOffence, Offence, TimeSlot, Kind},
};
use sr_std::prelude::*;
use sr_io::Printable;
use srml_support::{
	StorageValue, decl_module, decl_event, decl_storage,
	StorageDoubleMap, print,
};
use system::ensure_none;

mod app {
// TODO [slashing] </switch to new key management>
// 	pub use app_crypto::sr25519 as crypto;
// 	use app_crypto::{app_crypto, key_types::IM_ONLINE, sr25519};
//
// 	app_crypto!(sr25519, IM_ONLINE);
	pub use substrate_primitives::sr25519 as crypto;
	#[cfg(feature = "std")]
	pub use self::crypto::{Pair, Public, Signature};
	#[cfg(not(feature = "std"))]
	pub use self::crypto::{Public, Signature};
// TODO [slashing] </switch to new key management>
}

/// A Babe authority keypair. Necessarily equivalent to the schnorrkel public key used in
/// the main Babe module. If that ever changes, then this must, too.
#[cfg(feature = "std")]
pub type AuthorityPair = app::Pair;

/// A Babe authority signature.
pub type AuthoritySignature = app::Signature;

/// A Babe authority identifier. Necessarily equivalent to the schnorrkel public key used in
/// the main Babe module. If that ever changes, then this must, too.
pub type AuthorityId = app::Public;

// The local storage database key under which the worker progress status
// is tracked.
const DB_KEY: &[u8] = b"srml/im-online-worker-status";

// It's important to persist the worker state, since e.g. the
// server could be restarted while starting the gossip process, but before
// finishing it. With every execution of the off-chain worker we check
// if we need to recover and resume gossipping or if there is already
// another off-chain worker in the process of gossipping.
#[derive(Encode, Decode, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "std", derive(Debug))]
struct WorkerStatus<BlockNumber> {
	done: bool,
	gossipping_at: BlockNumber,
}

// Error which may occur while executing the off-chain code.
enum OffchainErr {
	DecodeWorkerStatus,
	NoKeys,
	ExtrinsicCreation,
	FailedSigning,
	NetworkState,
	SubmitTransaction,
}

impl Printable for OffchainErr {
	fn print(self) {
		match self {
			OffchainErr::DecodeWorkerStatus => print("Offchain error: decoding WorkerStatus failed!"),
			OffchainErr::NoKeys => print("Offchain error: could not find local keys!"),
			OffchainErr::ExtrinsicCreation => print("Offchain error: extrinsic creation failed!"),
			OffchainErr::FailedSigning => print("Offchain error: signing failed!"),
			OffchainErr::NetworkState => print("Offchain error: fetching network state failed!"),
			OffchainErr::SubmitTransaction => print("Offchain error: submitting transaction failed!"),
		}
	}
}

pub type AuthIndex = u32;

/// Heartbeat which is sent/received.
#[derive(Encode, Decode, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct Heartbeat<BlockNumber>
	where BlockNumber: PartialEq + Eq + Decode + Encode,
{
	block_number: BlockNumber,
	network_state: OpaqueNetworkState,
	session_index: SessionIndex,
	authority_index: AuthIndex,
}

pub trait Trait: system::Trait + session::Trait {
	/// The overarching event type.
	type Event: From<Event> + Into<<Self as system::Trait>::Event>;

	/// The function call.
	type Call: From<Call<Self>>;

	/// A extrinsic right from the external world. This is unchecked and so
	/// can contain a signature.
	type UncheckedExtrinsic: ExtrinsicT<Call=<Self as Trait>::Call> + Encode + Decode;

	/// A type that gives us ability to submit unresponsivness offence reports.
	type ReportUnresponsivness: ReportOffence<Self::AccountId, AuthorityId, UnresponsivnessOffence>;

	/// A function that returns the session index that started the current era.
	type CurrentEraStartSessionIndex: CurrentEraStartSessionIndex;
}

decl_event!(
	pub enum Event {
		/// A new heartbeat was received from `AuthorityId`
		HeartbeatReceived(AuthorityId),
	}
);

decl_storage! {
	trait Store for Module<T: Trait> as ImOnline {
		/// The block number when we should gossip.
		GossipAt get(gossip_at) config(): T::BlockNumber;

		/// The current set of keys that may issue a heartbeat.
		Keys get(keys) config(): Vec<AuthorityId>;

		/// For each session index we keep a mapping of `AuthorityId`
		/// to `offchain::OpaqueNetworkState`.
		ReceivedHeartbeats get(received_heartbeats): double_map SessionIndex,
			blake2_256(AuthIndex) => Vec<u8>;
	}
}


decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		fn deposit_event() = default;

		fn heartbeat(
			origin,
			heartbeat: Heartbeat<T::BlockNumber>,
			_signature: AuthoritySignature
		) {
			ensure_none(origin)?;

			let current_session = <session::Module<T>>::current_index();
			let exists = <ReceivedHeartbeats>::exists(
				&current_session,
				&heartbeat.authority_index
			);
			let keys = Keys::get();
			let public = keys.get(heartbeat.authority_index as usize);
			if let (true, Some(public)) = (!exists, public) {
				Self::deposit_event(Event::HeartbeatReceived(public.clone()));

				let network_state = heartbeat.network_state.encode();
				<ReceivedHeartbeats>::insert(
					&current_session,
					&heartbeat.authority_index,
					&network_state
				);
			}
		}

		// Runs after every block.
		fn offchain_worker(now: T::BlockNumber) {
			Self::offchain(now);
		}
	}
}

impl<T: Trait> Module<T> {
	/// Returns `true` if a heartbeat has been received for the authority at `authority_index` in
	/// the authorities series, during the current session. Otherwise `false`.
	pub fn is_online_in_current_session(authority_index: AuthIndex) -> bool {
		let current_session = <session::Module<T>>::current_index();
		<ReceivedHeartbeats>::exists(&current_session, &authority_index)
	}

	fn offchain(now: T::BlockNumber) {
		let next_gossip = <GossipAt<T>>::get();
		let check = Self::check_not_yet_gossipped(now, next_gossip);
		let (curr_worker_status, not_yet_gossipped) = match check {
			Ok((s, v)) => (s, v),
			Err(err) => {
				print(err);
				return;
			},
		};
		if next_gossip < now && not_yet_gossipped {
			let value_set = Self::compare_and_set_worker_status(now, false, curr_worker_status);
			if !value_set {
				// value could not be set in local storage, since the value was
				// different from `curr_worker_status`. this indicates that
				// another worker was running in parallel.
				return;
			}

			match Self::do_gossip_at(now) {
				Ok(_) => {},
				Err(err) => print(err),
			}
		}
	}

	fn do_gossip_at(block_number: T::BlockNumber) -> Result<(), OffchainErr> {
		// we run only when a local authority key is configured

		// TODO [slashing] <switch to new key management>
		// let key_type = KeyTypeId(*b"imon");
		// let local_keys = sr_io::public_keys(AuthorityId::KIND, key_type)
		// 	.map_err(|_| OffchainErr::NoKeys)?;
		let local_keys = vec![
			sr_io::pubkey(offchain::CryptoKey::AuthorityKey)
				.map_err(|_| OffchainErr::NoKeys)?
		];
		// TODO [slashing] </switch to new key management>

		let authorities = Keys::get();
		let maybe_key = authorities.into_iter()
			.enumerate()
			.filter_map(|(index, id)| {
				let id = app::crypto::Public::from(id);
				let id_slice: &[u8] = id.as_ref();

				local_keys
					.iter()
					// TODO [slashing] <remove>
					//.find(|k| &*k.public == id_slice)
					.find(|k| k.as_slice() == id_slice)
					// TODO [slashing] </remove>
					.map(|key| (index as u32, key.clone()))
			})
			.next();
		if let Some((authority_index, key)) = maybe_key {
			let network_state =
				sr_io::network_state().map_err(|_| OffchainErr::NetworkState)?;
			let heartbeat_data = Heartbeat {
				block_number,
				network_state,
				session_index: <session::Module<T>>::current_index(),
				authority_index,
			};

			// TODO [slashing] <remove>
			let key = offchain::CryptoKey::AuthorityKey;
			// TODO [slashing] </remove>
			let signature = sr_io::sign(key, &heartbeat_data.encode())
				.map_err(|_| OffchainErr::FailedSigning)?;
			let signature = AuthoritySignature::decode(&mut &*signature)
					.map_err(|_| OffchainErr::ExtrinsicCreation)?;
			let call = Call::heartbeat(heartbeat_data, signature);
			let ex = T::UncheckedExtrinsic::new_unsigned(call.into())
				.ok_or(OffchainErr::ExtrinsicCreation)?;
			sr_io::submit_transaction(&ex)
				.map_err(|_| OffchainErr::SubmitTransaction)?;

			// once finished we set the worker status without comparing
			// if the existing value changed in the meantime. this is
			// because at this point the heartbeat was definitely submitted.
			Self::set_worker_status(block_number, true);
		}
		Ok(())
	}

	fn compare_and_set_worker_status(
		gossipping_at: T::BlockNumber,
		done: bool,
		curr_worker_status: Option<Vec<u8>>,
	) -> bool {
		let enc = WorkerStatus {
			done,
			gossipping_at,
		};
		sr_io::local_storage_compare_and_set(
			StorageKind::PERSISTENT,
			DB_KEY,
			curr_worker_status.as_ref().map(Vec::as_slice),
			&enc.encode()
		)
	}

	fn set_worker_status(
		gossipping_at: T::BlockNumber,
		done: bool,
	) {
		let enc = WorkerStatus {
			done,
			gossipping_at,
		};
		sr_io::local_storage_set(
			StorageKind::PERSISTENT, DB_KEY, &enc.encode());
	}

	// Checks if a heartbeat gossip already occurred at this block number.
	// Returns a tuple of `(current worker status, bool)`, whereby the bool
	// is true if not yet gossipped.
	fn check_not_yet_gossipped(
		now: T::BlockNumber,
		next_gossip: T::BlockNumber,
	) -> Result<(Option<Vec<u8>>, bool), OffchainErr> {
		let last_gossip = sr_io::local_storage_get(StorageKind::PERSISTENT, DB_KEY);
		match last_gossip {
			Some(last) => {
				let worker_status: WorkerStatus<T::BlockNumber> = Decode::decode(&mut &last[..])
					.map_err(|_| OffchainErr::DecodeWorkerStatus)?;

				let was_aborted = !worker_status.done && worker_status.gossipping_at < now;

				// another off-chain worker is currently in the process of submitting
				let already_submitting =
					!worker_status.done && worker_status.gossipping_at == now;

				let not_yet_gossipped =
					worker_status.done && worker_status.gossipping_at < next_gossip;

				let ret = (was_aborted && !already_submitting) || not_yet_gossipped;
				Ok((Some(last), ret))
			},
			None => Ok((None, true)),
		}
	}

}

impl<T: Trait> session::OneSessionHandler<T::AccountId> for Module<T> {
	type Key = AuthorityId;

	fn on_new_session<'a, I: 'a>(_changed: bool, _validators: I, next_validators: I)
		where I: Iterator<Item=(&'a T::AccountId, AuthorityId)>
	{
		// Reset heartbeats
		<ReceivedHeartbeats>::remove_prefix(&<session::Module<T>>::current_index());

		// Tell the offchain worker to start making the next session's heartbeats.
		<GossipAt<T>>::put(<system::Module<T>>::block_number());

		// Remember who the authorities are for the new session.
		Keys::put(next_validators.map(|x| x.1).collect::<Vec<_>>());
	}

	fn on_before_session_ending() {
		let current_session = <session::Module<T>>::current_index();

		let keys = Keys::get();
		let mut unresponsive = vec![];
		for (auth_idx, auth_key) in keys.iter().enumerate() {
			if !<ReceivedHeartbeats>::exists(&current_session, &(auth_idx as u32)) {
				unresponsive.push(auth_key.clone());
			}
		}

		let current_era_start_session_index =
			T::CurrentEraStartSessionIndex::current_era_start_session_index();
		let validators_count = keys.len() as u32;

		let offence = UnresponsivnessOffence {
			session_index: current_session,
			current_era_start_session_index,
			validators_count,
			offenders: unresponsive,
		};

		// TODO [slashing]: Handle the result. Just write a proof?
		T::ReportUnresponsivness::report_offence(None, offence);
	}

	fn on_disabled(_i: usize) {
		// ignore
	}
}

impl<T: Trait> srml_support::unsigned::ValidateUnsigned for Module<T> {
	type Call = Call<T>;

	fn validate_unsigned(call: &Self::Call) -> srml_support::unsigned::TransactionValidity {
		if let Call::heartbeat(heartbeat, signature) = call {
			if <Module<T>>::is_online_in_current_session(heartbeat.authority_index) {
				// we already received a heartbeat for this authority
				return TransactionValidity::Invalid(ApplyError::Stale as i8);
			}

			// check if session index from heartbeat is recent
			let current_session = <session::Module<T>>::current_index();
			if heartbeat.session_index != current_session {
				return TransactionValidity::Invalid(ApplyError::Stale as i8);
			}

			// verify that the incoming (unverified) pubkey is actually an authority id
			let keys = Keys::get();
			let authority_id = match keys.get(heartbeat.authority_index as usize) {
				Some(id) => id,
				None => return TransactionValidity::Invalid(ApplyError::BadSignature as i8),
			};

			// check signature (this is expensive so we do it last).
			let signature_valid = heartbeat.using_encoded(|encoded_heartbeat| {
				// TODO [slashing] <remove>
				// let sig: &app::crypto::Signature = signature.as_ref();
				let sig: &app::crypto::Signature = signature;
				// TODO [slashing] </remove>
				sr_io::sr25519_verify(
					sig.as_ref(),
					encoded_heartbeat,
					&authority_id
				)
			});
			if !signature_valid {
				return TransactionValidity::Invalid(ApplyError::BadSignature as i8);
			}

			return TransactionValidity::Valid(ValidTransaction {
				priority: 0,
				requires: vec![],
				provides: vec![(current_session, authority_id).encode()],
				longevity: TransactionLongevity::max_value(),
				propagate: true,
			})
		}

		TransactionValidity::Invalid(0)
	}
}

/// An offense which is filed if a validator didn't send a heartbeat message.
pub struct UnresponsivnessOffence {
	/// The session index that starts an era in which the incident happened.
	current_era_start_session_index: u32, // TODO [slashing]: Should be a SessionIndex.
	/// The session index at which we file this report.
	///
	/// It acts as a time measure for unresponsivness reports and effectively will always point
	/// at the end of the session.
	session_index: u32, // TODO [slashing]: Should be a SessionIndex.
	/// The size of the validator set in current session/era.
	validators_count: u32,
	/// Authorities which were unresponsive during the current epoch.
	offenders: Vec<AuthorityId>,
}

impl Offence<AuthorityId> for UnresponsivnessOffence {
	const ID: Kind = *b"im-online:offlin";

	fn offenders(&self) -> Vec<AuthorityId> {
		self.offenders.clone()
	}

	fn current_era_start_session_index(&self) -> u32 { // TODO [slashing]: Should be a SessionIndex.
		self.current_era_start_session_index
	}

	fn validators_count(&self) -> u32 {
		self.validators_count
	}

	fn time_slot(&self) -> TimeSlot {
		self.session_index as TimeSlot
	}

	fn slash_fraction(offenders: u32, validators_count: u32) -> Perbill {
		// the formula is min((3 * (k - 1)) / n, 1) * 0.05
		let x = Perbill::from_rational_approximation(3 * (offenders - 1), validators_count);

		// _ * 0.05
		// For now, Perbill doesn't support multiplication other than an integer so we perform
		// a manual scaling.
		// TODO: #3189 should fix this.
		let p = (x.into_parts() as u64 * 50_000_000u64) / 1_000_000_000u64;
		Perbill::from_parts(p as u32)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_unresponsivness_slash_fraction() {
		// A single case of unresponsivness is not slashed.
		assert_eq!(
			UnresponsivnessOffence::slash_fraction(1, 50),
			Perbill::zero(),
		);

		assert_eq!(
			UnresponsivnessOffence::slash_fraction(3, 50),
			Perbill::from_parts(6000000), // 0.6%
		);

		// One third offline should be punished around 5%.
		assert_eq!(
			UnresponsivnessOffence::slash_fraction(17, 50),
			Perbill::from_parts(48000000), // 4.8%
		);
	}
}