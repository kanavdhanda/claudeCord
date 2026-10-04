//! Everything that runs on a machine with agents: remembering who the machine is, keeping a connection to the hub, and
//! (later in this folder) owning the agents' terminals. The device talks to the hub only through `link`, so nothing
//! else here cares how messages travel.
//!
//! Files:
//! - `config`  where the hub is and what token this machine uses, stored privately in the user's home
//! - `link`    the connection to the hub: connects, keeps alive, reconnects with backoff, hands frames both ways
//! - `inject`  the rules for when a message may be pasted into a terminal someone may be typing in
//! - `agent`   one agent's terminal: start it, watch it, type into it
//! - `daemon`  the program on a machine: link, agents, safe pasting, status reporting, the local command door
//! - `doctor`  step-by-step check of whether this machine can reach the hub, and where it stops
//! - `ipc`     how the command line and the agents' own shells talk to the daemon on this machine

pub mod agent;
pub mod config;
pub mod daemon;
pub mod doctor;
pub mod inject;
pub mod ipc;
pub mod link;
