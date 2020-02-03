// Copyright 2019-2020 Parity Technologies (UK) Ltd.
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

//! Implementations of the `IntoProtocolsHandler` and `ProtocolsHandler` traits for outgoing
//! substreams of a single gossiping protocol.
//!
//! > **Note**: Each instance corresponds to a single protocol. In order to support multiple
//! >			protocols, you need to create multiple instances and group them.
//!

use crate::protocol::generic_proto::upgrade::{NotificationsOut, NotificationsOutSubstream};
use futures::prelude::*;
use libp2p::core::{ConnectedPoint, Negotiated, PeerId};
use libp2p::core::upgrade::{DeniedUpgrade, InboundUpgrade, ReadOneError, OutboundUpgrade};
use libp2p::swarm::{
	ProtocolsHandler, ProtocolsHandlerEvent,
	IntoProtocolsHandler,
	KeepAlive,
	ProtocolsHandlerUpgrErr,
	SubstreamProtocol,
};
use log::error;
use smallvec::SmallVec;
use std::{borrow::Cow, fmt, marker::PhantomData, mem, pin::Pin, task::{Context, Poll}, time::{Duration, Instant}};

/// Maximum duration to open a substream and receive the handshake message. After that, we
/// consider that we failed to open the substream.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// After successfully establishing a connection with the remote, we keep the connection open for
/// at least this amount of time in order to give the rest of the code the chance to notify us to
/// open substreams.
const INITIAL_KEEPALIVE_TIME: Duration = Duration::from_secs(5);

/// Implements the `IntoProtocolsHandler` trait of libp2p.
///
/// Every time a connection with a remote starts, an instance of this struct is created and
/// sent to a background task dedicated to this connection. Once the connection is established,
/// it is turned into a [`NotifsOutHandler`].
///
/// See the documentation of [`NotifsOutHandler`] for more information.
pub struct NotifsOutHandlerProto<TSubstream> {
	/// Name of the protocol to negotiate.
	proto_name: Cow<'static, [u8]>,

	/// Marker to pin the generic type.
	marker: PhantomData<TSubstream>,
}

impl<TSubstream> NotifsOutHandlerProto<TSubstream> {
	/// Builds a new [`NotifsOutHandlerProto`]. Will use the given protocol name for the
	/// notifications substream.
	pub fn new(proto_name: impl Into<Cow<'static, [u8]>>) -> Self {
		NotifsOutHandlerProto {
			proto_name: proto_name.into(),
			marker: PhantomData,
		}
	}
}

impl<TSubstream> IntoProtocolsHandler for NotifsOutHandlerProto<TSubstream>
where
	TSubstream: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
	type Handler = NotifsOutHandler<TSubstream>;

	fn inbound_protocol(&self) -> DeniedUpgrade {
		DeniedUpgrade
	}

	fn into_handler(self, _: &PeerId, _: &ConnectedPoint) -> Self::Handler {
		NotifsOutHandler {
			proto_name: self.proto_name,
			when_connection_open: Instant::now(),
			state: State::Disabled,
			events_queue: SmallVec::new(),
		}
	}
}

/// Handler for an outbound notification substream.
///
/// When a connection is established, this handler starts in the "disabled" state, meaning that
/// no substream will be open.
///
/// One can try open a substream by sending an [`NotifsOutHandlerIn::Enable`] message to the
/// handler. Once done, the handler will try to establish then maintain an outbound substream with
/// the remote for the purpose of sending notifications to it.
pub struct NotifsOutHandler<TSubstream> {
	/// Name of the protocol to negotiate.
	proto_name: Cow<'static, [u8]>,

	/// Relationship with the node we're connected to.
	state: State<TSubstream>,

	/// When the connection with the remote has been successfully established.
	when_connection_open: Instant,

	/// Queue of events to send to the outside.
	///
	/// This queue must only ever be modified to insert elements at the back, or remove the first
	/// element.
	events_queue: SmallVec<[ProtocolsHandlerEvent<NotificationsOut, (), NotifsOutHandlerOut, void::Void>; 16]>,
}

/// Our relationship with the node we're connected to.
enum State<TSubstream> {
	/// The handler is disabled and idle. No substream is open.
	Disabled,

	/// The handler is disabled. A substream is still open and needs to be closed.
	///
	/// > **Important**: Having this state means that `poll_close` has been called at least once,
	/// >				 but the `Sink` API is unclear about whether or not the stream can then
	/// >				 be recovered. Because of that, we must never switch from the
	/// >				 `DisabledOpen` state to the `Open` state while keeping the same substream.
	DisabledOpen(NotificationsOutSubstream<Negotiated<TSubstream>>),

	/// The handler is disabled but we are still trying to open a substream with the remote.
	///
	/// If the handler gets enabled again, we can immediately switch to `Opening`.
	DisabledOpening,

	/// The handler is enabled and we are trying to open a substream with the remote.
	Opening,

	/// The handler is enabled. We have tried opening a substream in the past but the remote
	/// refused it.
	Refused,

	/// The handler is enabled and substream is open.
	Open(NotificationsOutSubstream<Negotiated<TSubstream>>),

	/// Poisoned state. Shouldn't be found in the wild.
	Poisoned,
}

/// Event that can be received by a `NotifsOutHandler`.
#[derive(Debug)]
pub enum NotifsOutHandlerIn {
	/// Enables the notifications substream for this node. The handler will try to maintain a
	/// substream with the remote.
	Enable,

	/// Disables the notifications substream for this node. This is the default state.
	Disable,

	/// Sends a message on the notifications substream. Ignored if the substream isn't open.
	///
	/// It is only valid to send this if the notifications substream has been enabled.
	Send(Vec<u8>),
}

/// Event that can be emitted by a `NotifsOutHandler`.
#[derive(Debug)]
pub enum NotifsOutHandlerOut {
	/// The notifications substream has been accepted by the remote.
	Open {
		/// Handshake message sent by the remote after we opened the substream.
		handshake: Vec<u8>,
	},

	/// The notifications substream has been closed by the remote.
	Closed,

	/// We tried to open a notifications substream, but the remote refused it.
	///
	/// Can only happen if we're in a closed state.
	Refused,
}

impl<TSubstream> NotifsOutHandler<TSubstream> {
	/// Returns true if the handler is enabled.
	pub fn is_enabled(&self) -> bool {
		match &self.state {
			State::Disabled => false,
			State::DisabledOpening => false,
			State::DisabledOpen(_) => false,
			State::Opening => true,
			State::Refused => true,
			State::Open(_) => true,
			State::Poisoned => false,
		}
	}

	/// Returns true if the substream is open.
	pub fn is_open(&self) -> bool {
		match &self.state {
			State::Disabled => false,
			State::DisabledOpening => false,
			State::DisabledOpen(_) => true,
			State::Opening => false,
			State::Refused => false,
			State::Open(_) => true,
			State::Poisoned => false,
		}
	}

	/// Returns the name of the protocol that we negotiate.
	pub fn protocol_name(&self) -> &[u8] {
		&self.proto_name
	}
}

impl<TSubstream> ProtocolsHandler for NotifsOutHandler<TSubstream>
where TSubstream: AsyncRead + AsyncWrite + Unpin + Send + 'static {
	type InEvent = NotifsOutHandlerIn;
	type OutEvent = NotifsOutHandlerOut;
	type Substream = TSubstream;
	type Error = void::Void;
	type InboundProtocol = DeniedUpgrade;
	type OutboundProtocol = NotificationsOut;
	type OutboundOpenInfo = ();

	fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol> {
		SubstreamProtocol::new(DeniedUpgrade)
	}

	fn inject_fully_negotiated_inbound(
		&mut self,
		proto: <Self::InboundProtocol as InboundUpgrade<Negotiated<TSubstream>>>::Output
	) {
		// We should never reach here. `proto` is a `Void`.
		void::unreachable(proto)
	}

	fn inject_fully_negotiated_outbound(
		&mut self,
		(handshake_msg, sub): <Self::OutboundProtocol as OutboundUpgrade<Negotiated<TSubstream>>>::Output,
		_: ()
	) {
		match mem::replace(&mut self.state, State::Poisoned) {
			State::Opening => {
				let ev = NotifsOutHandlerOut::Open { handshake: handshake_msg };
				self.events_queue.push(ProtocolsHandlerEvent::Custom(ev));
				self.state = State::Open(sub);
			},
			// If the handler was disabled while we were negotiating the protocol, immediately
			// close it.
			State::DisabledOpening => self.state = State::DisabledOpen(sub),

			// Any other situation should never happen.
			State::Disabled | State::Refused | State::Open(_) | State::DisabledOpen(_) =>
				error!("State mismatch in notifications handler: substream already open"),
			State::Poisoned => error!("Notifications handler in a poisoned state"),
		}
	}

	fn inject_event(&mut self, message: NotifsOutHandlerIn) {
		match message {
			NotifsOutHandlerIn::Enable => {
				match mem::replace(&mut self.state, State::Poisoned) {
					State::Disabled => {
						self.events_queue.push(ProtocolsHandlerEvent::OutboundSubstreamRequest {
							protocol: SubstreamProtocol::new(NotificationsOut::new(self.proto_name.clone()))
								.with_timeout(OPEN_TIMEOUT),
							info: (),
						});
						self.state = State::Opening;
					},
					State::DisabledOpening => self.state = State::Opening,
					State::DisabledOpen(sub) => self.state = State::Open(sub),
					State::Opening | State::Refused | State::Open(_) =>
						error!("Tried to enable notifications handler that was already enabled"),
					State::Poisoned => error!("Notifications handler in a poisoned state"),
				}
			},
			NotifsOutHandlerIn::Disable => {
				match mem::replace(&mut self.state, State::Poisoned) {
					State::Disabled | State::DisabledOpening =>
						error!("Tried to disable notifications handler that was already disabled"),
					State::DisabledOpen(sub) => self.state = State::Open(sub),
					State::Opening => self.state = State::DisabledOpening,
					State::Refused => self.state = State::Disabled,
					State::Open(sub) => self.state = State::DisabledOpen(sub),
					State::Poisoned => error!("Notifications handler in a poisoned state"),
				}
			},
			NotifsOutHandlerIn::Send(msg) =>
				if let State::Open(sub) = &mut self.state {
					sub.push_message(msg);
				},
		}
	}

	fn inject_dial_upgrade_error(&mut self, _: (), _: ProtocolsHandlerUpgrErr<ReadOneError>) {
		match mem::replace(&mut self.state, State::Poisoned) {
			State::Disabled => {},
			State::DisabledOpen(_) | State::Refused | State::Open(_) =>
				error!("State mismatch in NotificationsOut"),
			State::Opening => {
				self.state = State::Refused;
				let ev = NotifsOutHandlerOut::Refused;
				self.events_queue.push(ProtocolsHandlerEvent::Custom(ev));
			},
			State::DisabledOpening => self.state = State::Disabled,
			State::Poisoned => error!("Notifications handler in a poisoned state"),
		}
	}

	fn connection_keep_alive(&self) -> KeepAlive {
		match self.state {
			State::Disabled | State::DisabledOpen(_) | State::DisabledOpening =>
				KeepAlive::Until(self.when_connection_open + INITIAL_KEEPALIVE_TIME),
			State::Opening | State::Open(_) => KeepAlive::Yes,
			State::Refused | State::Poisoned => KeepAlive::No,
		}
	}

	fn poll(
		&mut self,
		cx: &mut Context,
	) -> Poll<ProtocolsHandlerEvent<Self::OutboundProtocol, Self::OutboundOpenInfo, Self::OutEvent, Self::Error>> {
		// Flush the events queue if necessary.
		if !self.events_queue.is_empty() {
			let event = self.events_queue.remove(0);
			return Poll::Ready(event);
		}

		match &mut self.state {
			State::Open(sub) => match Sink::poll_flush(Pin::new(sub), cx) {
				Poll::Pending | Poll::Ready(Ok(())) => {},
				Poll::Ready(Err(err)) => {
					// We try to re-open a substream.
					self.state = State::Opening;
					self.events_queue.push(ProtocolsHandlerEvent::OutboundSubstreamRequest {
						protocol: SubstreamProtocol::new(NotificationsOut::new(self.proto_name.clone()))
							.with_timeout(OPEN_TIMEOUT),
						info: (),
					});
					let ev = NotifsOutHandlerOut::Closed;
					return Poll::Ready(ProtocolsHandlerEvent::Custom(ev));
				}
			},
			State::DisabledOpen(sub) => match Sink::poll_close(Pin::new(sub), cx) {
				Poll::Pending => {},
				Poll::Ready(Ok(())) | Poll::Ready(Err(_)) => {
					self.state = State::Disabled;
					let ev = NotifsOutHandlerOut::Closed;
					return Poll::Ready(ProtocolsHandlerEvent::Custom(ev));
				},
			},
			_ => {}
		}

		Poll::Pending
	}
}

impl<TSubstream> fmt::Debug for NotifsOutHandler<TSubstream> {
	fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
		f.debug_struct("NotifsOutHandler")
			.field("open", &self.is_open())
			.finish()
	}
}
