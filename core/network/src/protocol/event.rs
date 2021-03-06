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

//! Network event types. These are are not the part of the protocol, but rather
//! events that happen on the network like DHT get/put results received.

use libp2p::kad::record::Key;

/// Events generated by DHT as a response to get_value and put_value requests.
pub enum DhtEvent {
	/// The value was found.
	ValueFound(Vec<(Key, Vec<u8>)>),

	/// The requested record has not been found in the DHT.
	ValueNotFound(Key),

	/// The record has been successfully inserted into the DHT.
	ValuePut(Key),

	/// An error has occured while putting a record into the DHT.
	ValuePutFailed(Key),
}

/// Type for events generated by networking layer.
pub enum Event {
	/// Event generated by a DHT.
	Dht(DhtEvent),
}
