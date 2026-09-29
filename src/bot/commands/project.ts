import {
  ChannelType,
  ChatInputCommandInteraction,
  PermissionFlagsBits,
  SlashCommandBuilder,
  type CategoryChannel,
  type TextChannel,
} from "discord.js";
import { getAgentByPersona, getAgentsInChannel, getProject, recordAudit, registerAgent } from "../../db/database.js";
import { agentKeyFor, safePersonaName } from "../../agents/identity.js";
import { announce, ensureRole } from "../../agents/persona.js";
import { getConfig } from "../../utils/config.js";
import { cloneRepo } from "../../git/clone.js";
import { channelNameFor, parseGithubRepo } from "../../utils/repo.js";
import { L } from "../../utils/i18n.js";

export const data = new SlashCommandBuilder()
  .setName("project")
  .setDescription("Start a project from a public GitHub repo")
  .addSubcommand((sub) =>
    sub
      .setName("new")
      .setDescription("Clone a repo on this machine and give it its own channel")
      .addStringOption((o) => o.setName("repo").setDescription("https://github.com/owner/name or owner/name").setRequired(true))
      .addStringOption((o) => o.setName("name").setDescription("Agent name people will see and @mention (default: the repo name)"))
      .addStringOption((o) => o.setName("avatar").setDescription("Optional image URL for the agent's avatar"))
      .addBooleanOption((o) =>
        o.setName("here").setDescription("Use this channel instead of creating a new one (default: create)"),
      )
      .addStringOption((o) => o.setName("host").setDescription("Which device should clone and run it (default: the primary device)"))
      .addChannelOption((o) =>
        o
          .setName("category")
          .setDescription("Category for the new channel (default: this channel's category)")
          .addChannelTypes(ChannelType.GuildCategory),
      ),
  )
  .setDefaultMemberPermissions(PermissionFlagsBits.Administrator);

export async function execute(interaction: ChatInputCommandInteraction): Promise<void> {
  const config = getConfig();
  const ref = parseGithubRepo(interaction.options.getString("repo", true));
  if (!ref) {
    await interaction.editReply(L("That doesn't look like a public GitHub repo URL. Use `https://github.com/owner/name`.", "공개 GitHub 저장소 URL이 아닙니다."));
    return;
  }

  const here = interaction.options.getBoolean("here") ?? false;
  const persona = safePersonaName(interaction.options.getString("name") ?? ref.repo);
  const avatar = interaction.options.getString("avatar");
  if (avatar && !/^https:\/\//.test(avatar)) {
    await interaction.editReply("The avatar must be an https:// image URL.");
    return;
  }
  if (getAgentByPersona(interaction.guildId!, persona)) {
    await interaction.editReply(L(`An agent called **${persona}** already exists in this server on this machine. Pick another \`name\`.`, `**${persona}** 에이전트가 이미 있습니다.`));
    return;
  }
  if (here && getProject(interaction.channelId) && !getAgentsInChannel(interaction.channelId).every((a) => a.persona)) {
    await interaction.editReply(L("This channel is registered the old way (no name). Use `/unregister` first, or omit `here`.", "이 채널은 이미 등록되어 있습니다."));
    return;
  }

  await interaction.editReply(L(`⏳ Cloning \`${ref.owner}/${ref.repo}\` on **${config.HOST_NAME}**...`, `⏳ \`${ref.owner}/${ref.repo}\` 클론 중...`));
  const result = await cloneRepo(ref, config.BASE_PROJECT_DIR);
  if (!result.ok) {
    await interaction.editReply(`❌ ${result.message}`);
    return;
  }

  const guild = interaction.guild!;
  let target = interaction.channel as TextChannel;
  if (!here) {
    const category =
      (interaction.options.getChannel("category") as CategoryChannel | null)?.id ??
      (interaction.channel && "parentId" in interaction.channel ? interaction.channel.parentId : null);
    const name = channelNameFor(config.HOST_NAME, ref.repo);
    target = (await guild.channels.create({
      name,
      type: ChannelType.GuildText,
      parent: category ?? undefined,
      topic: `Claude on ${config.HOST_NAME} · ${ref.owner}/${ref.repo}`,
    })) as TextChannel;
  }

  const key = agentKeyFor(target.id, persona, getAgentsInChannel(target.id).map((a) => a.channel_id));
  const roleId = await ensureRole(guild, persona);
  registerAgent({
    key,
    discordChannelId: target.id,
    projectPath: result.path,
    guildId: guild.id,
    persona,
    repoUrl: ref.cloneUrl,
    avatarUrl: avatar,
    roleId,
  });
  recordAudit({ guildId: guild.id, channelId: key, userId: interaction.user.id, action: "agent-new", detail: `${persona} ${ref.cloneUrl}` });

  const project = getProject(key)!;
  try {
    await announce(interaction.client, project);
  } catch (e) {
    console.warn("[project] announce failed:", e instanceof Error ? e.message : e);
    await target.send("⚠️ I couldn't create a webhook here (the bot needs **Manage Webhooks**), so I'll speak as the bot instead.");
  }

  const howToAddress = roleId ? `<@&${roleId}>` : `\`@${persona}\``;
  await interaction.editReply(
    L(
      `✅ **${persona}** is ready in <#${target.id}> (${result.message}). Address it with ${howToAddress}${getAgentsInChannel(target.id).length === 1 ? ", or just type, since it's the only agent there" : ""}.`,
      `✅ **${persona}** 준비 완료 <#${target.id}>.`,
    ),
  );
}
