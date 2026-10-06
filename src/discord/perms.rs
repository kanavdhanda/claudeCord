//! The Discord permissions the bot needs, in one place, so the invite link, the self-check and the docs cannot drift
//! apart. Nothing here is Administrator. These are exactly what channels, webhooks, threads, reactions, files and the
//! pinned status board need.

use base64::{
    Engine, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};

/// Permission name, its bit, and what to tell the user it is for.
pub const REQUIRED: [(&str, u32, &str); 11] = [
    ("ViewChannel", 10, "View Channels"),
    ("ManageChannels", 4, "Manage Channels"),
    ("ManageWebhooks", 29, "Manage Webhooks"),
    ("SendMessages", 11, "Send Messages"),
    ("SendMessagesInThreads", 38, "Send Messages in Threads"),
    ("CreatePublicThreads", 35, "Create Public Threads"),
    ("AddReactions", 6, "Add Reactions"),
    ("AttachFiles", 15, "Attach Files"),
    ("ReadMessageHistory", 16, "Read Message History"),
    (
        "ManageMessages",
        13,
        "Manage Messages (to pin the status board)",
    ),
    (
        "ManageRoles",
        28,
        "Manage Roles (so each agent can be @mentioned by name)",
    ),
];

/// What the bot is missing, by name, given the permissions Discord says it has in a server. Administrator covers everything.
pub fn missing_permissions(granted: u64) -> Vec<&'static str> {
    if granted & (1 << 3) != 0 {
        return Vec::new();
    }
    REQUIRED
        .iter()
        .filter(|(_, bit, _)| granted & (1u64 << bit) == 0)
        .map(|(_, _, what)| *what)
        .collect()
}

/// The permissions as the number Discord puts in an invite link.
pub fn permissions_integer() -> u64 {
    REQUIRED
        .iter()
        .fold(0, |acc, (_, bit, _)| acc | 1u64 << bit)
}

/// A bot token starts with the base64 of the bot's user id, which for a bot is also its application id, so the invite
/// link can be built from the token alone. None for anything that is not a token.
pub fn app_id_from_token(token: &str) -> Option<String> {
    // Lenient like Node's Buffer.from: padding optional, and stray trailing bits tolerated.
    const LENIENT: GeneralPurpose = GeneralPurpose::new(
        &alphabet::URL_SAFE,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    let first = token.split('.').next()?;
    let unpadded = first.trim_end_matches('=');
    let valid = (20..=30).contains(&unpadded.len())
        && first.len() - unpadded.len() <= 2
        && unpadded
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !valid {
        return None;
    }
    let id = String::from_utf8(LENIENT.decode(first).ok()?).ok()?;
    ((17..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

/// The link that adds the bot to a server with exactly the permissions above.
pub fn invite_url(app_id: &str) -> String {
    format!(
        "https://discord.com/oauth2/authorize?client_id={app_id}&scope=bot+applications.commands&permissions={}",
        permissions_integer()
    )
}
