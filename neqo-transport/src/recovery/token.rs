// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::{
    ackrate::AckRate,
    cid::ConnectionIdEntry,
    crypto::CryptoRecoveryToken,
    quic_datagrams::DatagramTracking,
    send_stream,
    stream_id::{StreamId, StreamType},
    tracking::AckToken,
};

pub type Tokens = Vec<Token>;

#[derive(Debug, Clone)]
pub enum StreamRecoveryToken {
    Stream(send_stream::RecoveryToken),
    ResetStream {
        stream_id: StreamId,
    },
    StopSending {
        stream_id: StreamId,
    },

    MaxData(u64),
    DataBlocked(u64),

    MaxStreamData {
        stream_id: StreamId,
        max_data: u64,
    },
    StreamDataBlocked {
        stream_id: StreamId,
        limit: u64,
    },

    MaxStreams {
        stream_type: StreamType,
        max_streams: u64,
    },
    StreamsBlocked {
        stream_type: StreamType,
        limit: u64,
    },
}

#[derive(Debug, Clone)]
pub enum Token {
    Stream(StreamRecoveryToken),
    Ack(AckToken),
    Crypto(CryptoRecoveryToken),
    HandshakeDone,
    KeepAlive, // Special PING.
    #[expect(
        clippy::enum_variant_names,
        reason = "This is how it is called in the spec."
    )]
    NewToken(usize),
    NewConnectionId(ConnectionIdEntry<[u8; 16]>),
    RetireConnectionId(u64),
    AckFrequency(AckRate),
    Datagram(DatagramTracking),
    /// A packet marked with [`neqo_common::Ecn::Ect0`].
    EcnEct0,
}
