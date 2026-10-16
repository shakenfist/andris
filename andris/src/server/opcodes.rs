//! The messages andris sends, one closed enum per channel.
//!
//! A channel's sender takes its opcode from that channel's enum, so
//! andris can only emit a message type listed here. Each variant maps to
//! one of ryll's `*_server` constants, and `opcodes_are_named_by_ryll`
//! checks that ryll's `logging::message_names` names every one: kerbside
//! terminates a session on any opcode those tables do not name.
//!
//! Each enum lists only what andris sends today. Add a variant when a
//! channel starts sending a new message, never a raw opcode.

use std::fmt::Debug;

use shakenfist_spice_protocol::logging::message_names;
use shakenfist_spice_protocol::{
    ChannelType, cursor_server, display_server, inputs_server, main_server,
};

/// The server-to-client messages of one channel type.
pub trait ServerOpcode: Copy + Debug + PartialEq + Send + Sync + 'static {
    /// The channel these messages belong to.
    const CHANNEL: ChannelType;
    /// The largest message body andris accepts from the client on this
    /// channel.
    const MAX_CLIENT_BODY: usize;
    /// Every variant.
    const ALL: &'static [Self];

    /// The opcode on the wire.
    fn code(self) -> u16;

    /// The name ryll's `logging::message_names` gives a server opcode on
    /// this channel, or `"unknown"`.
    fn ryll_name(code: u16) -> &'static str;

    /// The name ryll's `logging::message_names` gives a client opcode on
    /// this channel, or `"unknown"`. For logs only.
    fn ryll_client_name(code: u16) -> &'static str;
}

/// A channel that runs SPICE's ack window, and so sends `SET_ACK`.
pub trait AckedOpcode: ServerOpcode {
    /// This channel's `SET_ACK`.
    const SET_ACK: Self;
}

/// Declares one channel's opcode enum and its [`ServerOpcode`] impl.
macro_rules! server_opcodes {
    (
        $(#[$meta:meta])*
        $name:ident: $channel:expr, max body $max:expr, names $names:path, client names $cnames:path {
            $($(#[$vmeta:meta])* $variant:ident = $code:path,)+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
        }

        impl ServerOpcode for $name {
            const CHANNEL: ChannelType = $channel;
            const MAX_CLIENT_BODY: usize = $max;
            const ALL: &'static [Self] = &[$($name::$variant,)+];

            fn code(self) -> u16 {
                match self {
                    $($name::$variant => $code,)+
                }
            }

            fn ryll_name(code: u16) -> &'static str {
                $names(code)
            }

            fn ryll_client_name(code: u16) -> &'static str {
                $cnames(code)
            }
        }
    };
}

/// 64 KiB: the main and display channels' client message limit.
const MAX_BODY_LARGE: usize = 64 * 1024;
/// 4 KiB: the inputs and cursor channels' client message limit.
const MAX_BODY_SMALL: usize = 4 * 1024;

server_opcodes! {
    /// What the main channel sends.
    MainOpcode: ChannelType::Main, max body MAX_BODY_LARGE, names message_names::main_server,
        client names message_names::main_client {
        Ping = main_server::PING,
        Init = main_server::INIT,
        ChannelsList = main_server::CHANNELS_LIST,
        MouseMode = main_server::MOUSE_MODE,
        /// A chunk of a guest agent message; see `server::agent`.
        AgentData = main_server::AGENT_DATA,
        /// Agent tokens handed back to the client.
        AgentToken = main_server::AGENT_TOKEN,
    }
}

server_opcodes! {
    /// What the display channel sends.
    DisplayOpcode: ChannelType::Display, max body MAX_BODY_LARGE, names message_names::display_server,
        client names message_names::display_client {
        SetAck = display_server::SET_ACK,
        Mark = display_server::MARK,
        DrawCopy = display_server::DRAW_COPY,
        SurfaceCreate = display_server::SURFACE_CREATE,
        SurfaceDestroy = display_server::SURFACE_DESTROY,
        MonitorsConfig = display_server::MONITORS_CONFIG,
        /// A video stream begins; see `server::updates::stream`.
        StreamCreate = display_server::STREAM_CREATE,
        /// One frame of a video stream.
        StreamData = display_server::STREAM_DATA,
        /// A video stream ends.
        StreamDestroy = display_server::STREAM_DESTROY,
        /// Every video stream ends, before the surface they are on goes.
        StreamDestroyAll = display_server::STREAM_DESTROY_ALL,
        /// Asks the client to report on a video stream.
        StreamActivateReport = display_server::STREAM_ACTIVATE_REPORT,
    }
}

server_opcodes! {
    /// What the inputs channel sends.
    InputsOpcode: ChannelType::Inputs, max body MAX_BODY_SMALL, names message_names::inputs_server,
        client names message_names::inputs_client {
        Init = inputs_server::INIT,
        MouseMotionAck = inputs_server::MOUSE_MOTION_ACK,
    }
}

server_opcodes! {
    /// What the cursor channel sends.
    CursorOpcode: ChannelType::Cursor, max body MAX_BODY_SMALL, names message_names::cursor_server,
        client names message_names::cursor_client {
        SetAck = cursor_server::SET_ACK,
        Init = cursor_server::INIT,
        Set = cursor_server::SET,
    }
}

impl AckedOpcode for DisplayOpcode {
    const SET_ACK: Self = DisplayOpcode::SetAck;
}

impl AckedOpcode for CursorOpcode {
    const SET_ACK: Self = CursorOpcode::SetAck;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every opcode of `O` must be named by ryll's table for its channel.
    fn check_named<O: ServerOpcode>() {
        assert!(!O::ALL.is_empty());
        for op in O::ALL {
            let name = O::ryll_name(op.code());
            assert_ne!(
                name,
                "unknown",
                "{op:?} (opcode {}) on the {} channel is not named by ryll's message_names",
                op.code(),
                O::CHANNEL.name()
            );
        }
    }

    #[test]
    fn opcodes_are_named_by_ryll() {
        check_named::<MainOpcode>();
        check_named::<DisplayOpcode>();
        check_named::<InputsOpcode>();
        check_named::<CursorOpcode>();
    }

    #[test]
    fn opcodes_are_distinct_within_a_channel() {
        fn check<O: ServerOpcode>() {
            let mut codes: Vec<u16> = O::ALL.iter().map(|op| op.code()).collect();
            codes.sort_unstable();
            codes.dedup();
            assert_eq!(codes.len(), O::ALL.len(), "{}", O::CHANNEL.name());
        }
        check::<MainOpcode>();
        check::<DisplayOpcode>();
        check::<InputsOpcode>();
        check::<CursorOpcode>();
    }

    #[test]
    fn msg_list_is_never_sent() {
        // Opcode 8, SPICE_MSG_LIST, is a common message ryll does not
        // name, so kerbside would refuse it.
        assert!(!MainOpcode::ALL.iter().any(|op| op.code() == 8));
        assert!(!DisplayOpcode::ALL.iter().any(|op| op.code() == 8));
        assert!(!InputsOpcode::ALL.iter().any(|op| op.code() == 8));
        assert!(!CursorOpcode::ALL.iter().any(|op| op.code() == 8));
    }
}
