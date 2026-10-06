//! Files moving through the hub: from an agent to the chat, from an agent to a peer, and from a person to agents.
//! Files travel in small chunks. Chunks bound for the chat are collected and checked as a whole (size cap, secret
//! scan) before anything is posted. Peer transfers are relayed chunk by chunk without being held.

use super::core::HubCore;
use super::effects::{Chat, Effect};
use super::model::*;
use crate::protocol::{FILE_CHUNK_BYTES, HubFrame, MAX_FILE_BYTES};
use crate::security::redact::find_secrets_in_file;
use base64::{
    Engine,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};

const MAX_ACTIVE_UPLOADS: usize = 16;
const MAX_UPLOAD_MEMORY: usize = 64 * 1024 * 1024;
const UPLOAD_TTL_MS: i64 = 2 * 60_000;

/// Lenient like Node's base64 decoding: padding optional, stray trailing bits tolerated.
const B64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// A file being collected from chunks.
pub(super) struct Upload {
    chunks: Vec<Vec<u8>>,
    bytes: usize,
    at: i64,
    /// The chunk number expected next: a missing or repeated one means the file is damaged, and it is dropped rather than posted.
    next: u64,
    project: String,
    from: String,
    name: String,
}

impl HubCore {
    /// One chunk from an agent. With a peer named, it is relayed to that peer. Otherwise it is collected, and when the
    /// last chunk arrives the whole file is checked and posted to the chat.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn on_file_chunk(
        &mut self,
        transfer_id: &str,
        agent_id: &str,
        name: &str,
        seq: u64,
        last: bool,
        data: &str,
        sha256: Option<String>,
        to: Option<String>,
        caption: Option<String>,
        thread: Option<String>,
        now: i64,
        fx: &mut Vec<Effect>,
    ) {
        let Some(a) = self.agents.get(agent_id).cloned() else {
            return;
        };
        if let Some(to) = to.filter(|t| !t.is_empty()) {
            let Some(peer) = self.find_by_name(&a.project, &to).cloned() else {
                if seq == 0 {
                    Self::notice(
                        &a.project,
                        format!(
                            "{} tried to send a file to {to}, but no such agent is in this project.",
                            a.name
                        ),
                        false,
                        fx,
                    );
                }
                return;
            };
            // A file between two agents goes in the thread of that pair, the same one their messages use, so the chat shows it next to what
            // they said about it and the main channel stays for people.
            let thread = thread.or_else(|| {
                let mut pair = [a.name.clone(), peer.name.clone()];
                pair.sort();
                Some(pair.join(" & ").chars().take(90).collect())
            });
            let frame = HubFrame::FileChunk {
                transfer_id: transfer_id.into(),
                agent_id: peer.agent_id.clone(),
                from: a.name.clone(),
                name: name.into(),
                seq,
                last,
                data: data.into(),
                sha256,
                caption: caption.clone(),
                thread: thread.clone(),
            };
            self.send_to(&peer, frame, fx);
            self.metrics
                .inc("file_bytes", (data.len() * 3 / 4) as f64, now);
            if last {
                self.metrics.inc("file_count", 1.0, now);
                let note = caption
                    .filter(|c| !c.is_empty())
                    .map_or(String::new(), |c| format!(" {c}"));
                fx.push(Effect::Chat(Chat::Post {
                    project: a.project.clone(),
                    agent: a.clone(),
                    text: format!("Sent {name} to @{}.{note}", peer.name),
                    thread,
                }));
            }
            return;
        }
        // A transfer that stopped part way (the machine lost its connection, say) is dropped, and the chat is told.
        let stale: Vec<String> = self
            .uploads
            .iter()
            .filter(|(_, u)| now - u.at > UPLOAD_TTL_MS)
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            if let Some(u) = self.uploads.remove(&k) {
                Self::notice(
                    &u.project,
                    format!(
                        "{} did not finish sending {}: it stopped part way, so nothing was posted.",
                        u.from, u.name
                    ),
                    false,
                    fx,
                );
            }
        }
        if !self.uploads.contains_key(transfer_id) {
            if seq != 0 {
                return;
            }
            let held: usize = self.uploads.values().map(|u| u.bytes).sum();
            if self.uploads.len() >= MAX_ACTIVE_UPLOADS || held >= MAX_UPLOAD_MEMORY {
                Self::notice(
                    &a.project,
                    format!(
                        "Too many files in flight, so {name} from {} was refused. Try again shortly.",
                        a.name
                    ),
                    false,
                    fx,
                );
                return;
            }
            self.uploads.insert(
                transfer_id.into(),
                Upload {
                    chunks: Vec::new(),
                    bytes: 0,
                    at: now,
                    next: 0,
                    project: a.project.clone(),
                    from: a.name.clone(),
                    name: name.into(),
                },
            );
        }
        let Ok(buf) = B64.decode(data.trim_end_matches('=')) else {
            self.uploads.remove(transfer_id);
            Self::notice(
                &a.project,
                format!(
                    "{} sent {name} damaged (it could not be decoded), so nothing was posted.",
                    a.name
                ),
                false,
                fx,
            );
            return;
        };
        let u = self.uploads.get_mut(transfer_id).expect("inserted above");
        if seq != u.next {
            self.uploads.remove(transfer_id);
            Self::notice(
                &a.project,
                format!(
                    "{name} from {} arrived with a piece missing or repeated, so nothing was posted. Send it again.",
                    a.name
                ),
                false,
                fx,
            );
            return;
        }
        u.next += 1;
        u.at = now;
        u.bytes += buf.len();
        if u.bytes > MAX_FILE_BYTES {
            self.uploads.remove(transfer_id);
            Self::notice(
                &a.project,
                format!(
                    "{} tried to send {name}, which is over the {} MB limit.",
                    a.name,
                    MAX_FILE_BYTES / 1_048_576
                ),
                false,
                fx,
            );
            return;
        }
        u.chunks.push(buf);
        if !last {
            return;
        }
        let bytes = self
            .uploads
            .remove(transfer_id)
            .map(|u| u.chunks.concat())
            .unwrap_or_default();
        // Put together, it must be what the sender had.
        if let Some(want) = &sha256
            && crate::agents::text::sha256_hex(&bytes) != *want
        {
            Self::notice(
                &a.project,
                format!(
                    "{name} from {} arrived damaged (its checksum did not match), so nothing was posted. Send it again.",
                    a.name
                ),
                false,
                fx,
            );
            return;
        }
        let secrets = find_secrets_in_file(&bytes);
        if !secrets.is_empty() {
            self.metrics.inc("secret_blocks", 1.0, now);
            Self::notice(
                &a.project,
                format!(
                    "Blocked {name} from {}: it appears to contain {}.",
                    a.name,
                    secrets.join(", ")
                ),
                true,
                fx,
            );
            return;
        }
        self.metrics.inc("file_count", 1.0, now);
        self.metrics.inc("file_bytes", bytes.len() as f64, now);
        fx.push(Effect::Chat(Chat::File {
            project: a.project.clone(),
            agent: a,
            name: name.into(),
            data: bytes,
            caption,
            thread,
        }));
    }

    /// A person sends a file to the agents their message names (or the lead). It is chunked here. The caller has
    /// already checked size and content on the way in. Needs the operator role.
    #[allow(clippy::too_many_arguments)]
    pub fn send_file(
        &mut self,
        by: &Human,
        project: &str,
        text: &str,
        name: &str,
        data: &[u8],
        thread: Option<String>,
        transfer_id: &str,
    ) -> Result<(Vec<String>, Vec<Effect>), Denied> {
        self.require(project, &by.id, Role::Operator)?;
        let mut fx = Vec::new();
        let targets = self.pick_targets(project, text);
        let from = self.label(project, by);
        let total = data.len().div_ceil(FILE_CHUNK_BYTES).max(1);
        let sum = crate::agents::text::sha256_hex(data);
        for t in &targets {
            for seq in 0..total {
                let slice = &data[(seq * FILE_CHUNK_BYTES).min(data.len())
                    ..((seq + 1) * FILE_CHUNK_BYTES).min(data.len())];
                let frame = HubFrame::FileChunk {
                    transfer_id: transfer_id.into(),
                    agent_id: t.agent_id.clone(),
                    from: from.clone(),
                    name: name.into(),
                    seq: seq as u64,
                    last: seq == total - 1,
                    data: base64::engine::general_purpose::STANDARD.encode(slice),
                    sha256: (seq == total - 1).then(|| sum.clone()),
                    caption: (!text.is_empty()).then(|| text.to_string()),
                    thread: thread.clone(),
                };
                self.send_to(t, frame, &mut fx);
            }
        }
        Ok((targets.into_iter().map(|t| t.name).collect(), fx))
    }
}
